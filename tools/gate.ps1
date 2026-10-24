# Release gate: every autotest scenario against its own fresh dev hub (hub 7997, LiveKit 7970-7972;
# the host-mode scenario uses 7990/7980-7982, the server-list one a second hub on 7998/7973-7975).
# Never touches a live server on 7890/7880-7882.
# Build first:  cargo build --release --workspace   (with CARGO_TARGET_DIR=target-v02)
# Usage:        powershell -ExecutionPolicy Bypass -File tools\gate.ps1 [-Only name,name] [-Assets <main tree>]
#               (-Assets: where bin\livekit-server.exe and releases\ are, when run from a worktree)
param([string[]]$Only = @(), [string]$Assets = "")
$ErrorActionPreference = "Continue"
# -File passes "a,b" as one string: split it.
$Only = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$R = Split-Path $PSScriptRoot -Parent
# CARGO_TARGET_DIR may be relative to the repo (target-v02) or absolute (a worktree sharing the main tree's).
$B = if (-not $env:CARGO_TARGET_DIR) { "$R\target-v02\release" } elseif ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { Join-Path $env:CARGO_TARGET_DIR "release" } else { Join-Path $R "$env:CARGO_TARGET_DIR\release" }
# bin\livekit-server.exe (and, for the gate, releases\) come from -Assets: the repo by default, or the
# main tree when this runs from a git worktree without them (read only).
if (-not $Assets) { $Assets = $R }
$LK = "$Assets\bin\livekit-server.exe"
$G = Join-Path $env:TEMP "concord-gate"
Remove-Item $G -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $G | Out-Null
$env:VOX_NO_PLAYBACK = "1"; $env:VOX_NO_MIC = "1"; $env:VOX_FAKE_CAMERA = "1"

function Want($tag) { $Only.Count -eq 0 -or $Only -contains $tag }

function Start-Hub($tag) {
  $d = "$G\$tag\data"; New-Item -ItemType Directory $d -Force | Out-Null
  @"
server_name = "gate"
server_password = "x"
owner = "dono"
bind = "127.0.0.1:7997"
public_host = "127.0.0.1"
[livekit]
api_key = "APIgate000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = 7970
rtc_tcp_port = 7971
rtc_udp_port = 7972
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
  $h = Start-Process "$B\concord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$G\$tag\hub`"" -WindowStyle Hidden -PassThru
  Start-Sleep 4
  return $h
}
function Stop-Hub($h, $tag) {
  if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force }
  Start-Sleep 1
  # Only the LiveKit this scenario started (its command line names this scenario's folder).
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape("$G\$tag") } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
}
# A program on the gate's own desktop ("concord-gate", created once, never switched to): nothing it
# shows reaches the user's screen and it cannot take the foreground. For old releases, which have no
# background test mode. Returns its Process (exit code readable).
function Start-Offscreen($exe, $argline) {
  if (-not ("GateDesk" -as [type])) {
    Add-Type @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class GateDesk {
  [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr devmode, uint flags, uint access, IntPtr sa);
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct SI { public int cb; public string reserved; public string desktop; public string title; public int x, y, xs, ys, xc, yc, fill, flags; public short show, r2; public IntPtr r3, i, o, e; }
  [StructLayout(LayoutKind.Sequential)]
  struct PI { public IntPtr process, thread; public int pid, tid; }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern bool CreateProcessW(string app, StringBuilder cmd, IntPtr pa, IntPtr ta, bool inherit, uint flags, IntPtr env, string dir, ref SI si, out PI pi);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
  static IntPtr desk;
  public static int Start(string exe, string args) {
    if (desk == IntPtr.Zero) desk = CreateDesktopW("concord-gate", IntPtr.Zero, IntPtr.Zero, 0, 0x10000000, IntPtr.Zero);
    if (desk == IntPtr.Zero) return -1;
    var si = new SI(); si.cb = Marshal.SizeOf(typeof(SI)); si.desktop = "concord-gate";
    PI pi;
    if (!CreateProcessW(null, new StringBuilder("\"" + exe + "\" " + args), IntPtr.Zero, IntPtr.Zero, false, 0, IntPtr.Zero, null, ref si, out pi)) return -1;
    CloseHandle(pi.thread); CloseHandle(pi.process);
    return pi.pid;
  }
}
"@
  }
  $id = [GateDesk]::Start($exe, $argline)
  if ($id -le 0) { throw "could not start $exe on the gate desktop" }
  $p = [System.Diagnostics.Process]::GetProcessById($id)
  $null = $p.Handle   # keeps the handle open, so ExitCode is readable once it has exited
  $p
}
# $specs: role, name, extra args, delay after start, optional exe (another app build, e.g. an old
# release), offscreen=$true (run it on the gate's own desktop, see Start-Offscreen) and label (the
# log folder and the result's name when several clients log in with the same name).
function Run-Clients($tag, $specs, $timeout, [switch]$NoHub) {
  if (-not (Want $tag)) { return }
  $hub = if ($NoHub) { $null } else { Start-Hub $tag }
  $procs = @()
  foreach ($s in $specs) {
    $exe = if ($s.exe) { $s.exe } else { "$B\concord.exe" }
    $server = if ($s.server) { $s.server } else { "--server http://127.0.0.1:7997 --password x" }
    $label = if ($s.label) { $s.label } else { $s.name }
    $args = "--autotest $($s.role) $server --name $($s.name) --run-id $tag --log-dir `"$G\$tag\$label`" $($s.extra)"
    # Our own clients keep their windows at the bottom without focus (background test mode). An
    # old release (offscreen=$true) knows nothing of that and takes the foreground, so it runs on
    # a desktop of its own that is never shown (its window can be neither seen nor focused).
    $p = if ($s.offscreen) { Start-Offscreen $exe $args } else { Start-Process $exe -ArgumentList $args -PassThru }
    $procs += [pscustomobject]@{ name = $label; p = $p }
    Start-Sleep $s.delay
  }
  $deadline = (Get-Date).AddSeconds($timeout)
  while ((Get-Date) -lt $deadline -and ($procs | Where-Object { -not $_.p.HasExited })) { Start-Sleep 2 }
  $line = "$tag :"
  foreach ($x in $procs) {
    if ($x.p.HasExited) { $line += " $($x.name)=$($x.p.ExitCode)" } else { $line += " $($x.name)=TIMEOUT"; Stop-Process -Id $x.p.Id -Force }
  }
  Stop-Hub $hub $tag
  $line
}

# The test windows never take the foreground (they sit at the bottom of the z-order), so
# MainWindowHandle may name another of their windows: each one prints its own "hwnd=<n>".
$tw1 = Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw1.txt" -PassThru
$tw2 = Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw2.txt" -PassThru
Start-Sleep 5
function TwHwnd($f) { [int64]((Get-Content $f -ErrorAction SilentlyContinue | Select-String -Pattern '^hwnd=(\d+)' | Select-Object -First 1).Matches.Groups[1].Value) }
$h1 = TwHwnd "$G\tw1.txt"; $h2 = TwHwnd "$G\tw2.txt"

Run-Clients "golive"  @(@{role="a"; name="dono"; extra="--hwnd $h1"; delay=3}, @{role="b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "preview" @(@{role="a"; name="dono"; extra="--hwnd $h1 --preview"; delay=3}, @{role="b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "camera"  @(@{role="cam-a"; name="dono"; extra=""; delay=3}, @{role="cam-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "multi"   @(@{role="multi-a"; name="mA1"; extra="--hwnd $h1"; delay=2}, @{role="multi-a"; name="mA2"; extra="--hwnd $h2"; delay=3}, @{role="multi-b"; name="Tati"; extra=""; delay=0}) 300
# STAGE scenario (v1.0): the call stage. Two streamers (multi-a, each sharing a test window), a
# camera (cam-a) and stage-b, which pins both streams and the camera, hides a card without video,
# enters and leaves the stage fullscreen, checking that the video windows follow the cards.
Run-Clients "stage"   @(@{role="multi-a"; name="sgA1"; extra="--hwnd $h1"; delay=2}, @{role="multi-a"; name="sgA2"; extra="--hwnd $h2"; delay=2}, @{role="cam-a"; name="sgC1"; extra="--hold 20"; delay=3}, @{role="stage-b"; name="sgB1"; extra=""; delay=0}) 300
Run-Clients "audio"   @(@{role="audio-a"; name="dono"; extra=""; delay=3}, @{role="audio-b"; name="Tati"; extra=""; delay=0}) 300
Run-Clients "mod"     @(@{role="mod-a"; name="dono"; extra="--mode Tati"; delay=3}, @{role="mod-b"; name="Tati"; extra=""; delay=0}) 300
Run-Clients "avatar"  @(@{role="avatar-a"; name="dono"; extra=""; delay=3}, @{role="avatar-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "sound"   @(@{role="sound-a"; name="dono"; extra=""; delay=3}, @{role="sound-b"; name="Tati"; extra=""; delay=0}) 240
# MICSWITCH scenario (v1.1, autotest_micswitch.rs): the microphone changes mid-call. micswitch-a has
# made-up microphones (no device is opened, VOX_NO_MIC stays on) in several formats and joins with
# none plugged in; it plugs them, switches between them, unplugs one with and without an error,
# breaks one, loses them all; micswitch-b must hear it after each change, without anybody rejoining.
if (Want "micswitch") {
  $mx = "$G\micswitch\x"; New-Item -ItemType Directory $mx -Force | Out-Null
  Run-Clients "micswitch" @(@{role="micswitch-a"; name="dono"; extra="--mode `"$mx`""; delay=3}, @{role="micswitch-b"; name="Tati"; extra="--mode `"$mx`""; delay=0}) 300
}
Run-Clients "profile" @(@{role="profile-a"; name="dono"; extra=""; delay=3}, @{role="profile-b"; name="Tati"; extra=""; delay=0}) 240
Run-Clients "dm"      @(@{role="dm-b"; name="dmB1"; extra=""; delay=2}, @{role="dm-a"; name="dmA1"; extra=""; delay=0}) 300
Run-Clients "menu"    @(@{role="menu-b"; name="menuB1"; extra=""; delay=2}, @{role="menu-a"; name="menuA1"; extra="--hwnd $h1"; delay=0}) 300
Run-Clients "img"     @(@{role="img-b"; name="imgB1"; extra=""; delay=2}, @{role="img-a"; name="imgA1"; extra=""; delay=0}) 300

$env:VOX_TRAY_TEST = "1"   # notify-b gets the real tray icon and taskbar badge; notifications/sounds stay silent
Run-Clients "notify"  @(@{role="notify-c"; name="ntC1"; extra=""; delay=1}, @{role="notify-b"; name="ntB1"; extra=""; delay=1}, @{role="notify-a"; name="ntA1"; extra=""; delay=0}) 300
Remove-Item Env:VOX_TRAY_TEST

$env:VOX_FAKE_CAMERA_LOWLIGHT = "1"   # the synthetic camera plays a dim room ("Priorizar fluidez")
Run-Clients "camwin"  @(@{role="camwin"; name="dono"; extra=""; delay=0}) 180
Remove-Item Env:VOX_FAKE_CAMERA_LOWLIGHT

# HOST-MODE scenario and CHATX scenario: added below by their owners.

# HOST-MODE scenario (v0.8): no dev hub, the server runs inside host-a's app on the test block
# 7990/7980-7982 with the network part off (the router is never touched), everything on 127.0.0.1.
# A hosts and makes an invite, B joins with only the code (chat + voice), A revokes, C is refused.
# The clients exchange the code through files in $G\hostinv\x (--mode).
if (Want "hostinv") {
  $hx = "$G\hostinv\x"; New-Item -ItemType Directory $hx -Force | Out-Null
  $env:VOX_HOST_DIR = "$G\hostinv\host"; $env:VOX_HOST_OFFLINE = "1"; $env:VOX_HOST_BIND = "127.0.0.1"
  $env:VOX_HOST_FIRST_BLOCK = "1"; $env:VOX_HOST_LAN = "127.0.0.1/8"; $env:VOX_HOST_LIVEKIT_BIN = $LK
  $hs = "--mode `"$hx`""
  Run-Clients "hostinv" @(@{role="host-a"; name="hostA1"; server=$hs; extra=""; delay=1}, @{role="host-b"; name="hostB1"; server=$hs; extra=""; delay=1}, @{role="host-c"; name="hostC1"; server=$hs; extra=""; delay=0}) 360 -NoHub
  foreach ($k in "VOX_HOST_DIR", "VOX_HOST_OFFLINE", "VOX_HOST_BIND", "VOX_HOST_FIRST_BLOCK", "VOX_HOST_LAN", "VOX_HOST_LIVEKIT_BIN") { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
}

# CHATX scenario (v0.8): mentions, replies and reactions. chatx-a mentions chatx-b from the member
# list, B replies (with and without "@ ligado"), both react live in the channel and in a DM, A
# deletes a quoted message. The released v0.7.0 app (its notify-c role) reads the same channel
# alongside: the old-app check (it must skip the new events and still pass). The old app is
# unzipped once from releases\v0.7.0 into a cache under %TEMP% (kept between runs, never in the repo).
if (Want "chatx") {
  $cache = Join-Path $env:TEMP "voxhub-gate-cache\v0.7.0"
  $old = "$cache\voxhub.exe"
  if (-not (Test-Path $old)) {
    New-Item -ItemType Directory $cache -Force | Out-Null
    Expand-Archive "$Assets\releases\v0.7.0\voxhub-v0.7.0-PUBLICO.zip" $cache -Force
    $inner = Get-ChildItem $cache -Recurse -Filter voxhub.exe | Select-Object -First 1
    if ($inner -and $inner.FullName -ne $old) { Move-Item $inner.FullName $old -Force }
  }
  Run-Clients "chatx" @(@{role="notify-c"; name="ntC1"; exe=$old; offscreen=$true; extra=""; delay=1}, @{role="chatx-b"; name="cxB1"; extra=""; delay=2}, @{role="chatx-a"; name="cxA1"; extra=""; delay=0}) 300
}

# FIX scenario (v0.8.2): the owner (fix-a, "dono") and a member (fix-b) delete their own messages
# with images (channel and DM; the confirmation and Shift), a new picture after the newest one was
# deleted shows its own thumbnail (fresh ids, different content hash), a deleted and recreated
# channel opens empty with none of the old state, and the sidebar badges never cover the name.
Run-Clients "fix"     @(@{role="fix-a"; name="dono"; extra=""; delay=3}, @{role="fix-b"; name="fxB1"; extra=""; delay=0}) 300

# PRIV scenario (v0.9): private channels. The owner (priv-a, "dono") makes a text and a voice
# channel private for one member (priv-b); the other member (priv-c) sees nothing of them and its
# voice join is refused; priv-b chats there and joins the voice room; the owner takes priv-b off
# and priv-b loses both live (taken out of the LiveKit room at once, cached messages dropped).
Run-Clients "priv"    @(@{role="priv-c"; name="pvC1"; extra=""; delay=1}, @{role="priv-b"; name="pvB1"; extra=""; delay=1}, @{role="priv-a"; name="dono"; extra=""; delay=0}) 300

# NAV scenario (v1.0): the navigation and the center modes. The owner (nav-a, "dono") opens a text
# channel, joins the voice room (only the room is highlighted, the text channel loses it), goes
# through the three modes, checks that a message in the visible docked chat arrives read (and one
# in a hidden channel does not), text -> voice -> text from the sidebar, the hover bar, a DM;
# nav-b sits in the room with its camera on and answers on cue.
Run-Clients "nav"     @(@{role="nav-b"; name="navB1"; extra=""; delay=3}, @{role="nav-a"; name="dono"; extra=""; delay=0}) 300

# SRVLIST scenario (v0.9.1): the saved server list. Two dev hubs: "gate1" on 7997 (LiveKit
# 7970-7972) and "gate2" on 7998 (LiveKit 7973-7975). The app keeps its settings in a folder of the
# run (--settings-dir), first written the way v0.9.0 left it: gate1 as the single saved server with
# its password DPAPI-encrypted. Phase 1 checks the migration, saves gate2 by typing its address
# and switches back to gate1 with one click; phase 2 is the restarted app: both online, one click
# each (login token / saved password), "Trocar de servidor", the form asking only for a forgotten
# password, and removing gate2 with its secrets.
if (Want "srvlist") {
  $sd = "$G\srvlist"; $cfg = "$sd\settings"; New-Item -ItemType Directory $cfg -Force | Out-Null
  function Start-ListHub($sub, $port, $lk, $name, $pw) {
    $d = "$sd\$sub\data"; New-Item -ItemType Directory $d -Force | Out-Null
    @"
server_name = "$name"
server_password = "$pw"
owner = "dono"
bind = "127.0.0.1:$port"
public_host = "127.0.0.1"
[livekit]
api_key = "APIgate000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = $lk
rtc_tcp_port = $($lk + 1)
rtc_udp_port = $($lk + 2)
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
    Start-Process "$B\concord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$sd\$sub\hub`"" -WindowStyle Hidden -PassThru
  }
  $pw1 = "srv-um-123"
  $hubs = @((Start-ListHub "hub1" 7997 7970 "gate1" $pw1), (Start-ListHub "hub2" 7998 7973 "gate2" "srv-dois-456"))
  Start-Sleep 5
  # settings.json as v0.9.0 wrote it: one server, its password encrypted for this Windows user.
  Add-Type -AssemblyName System.Security
  $enc = [Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes($pw1), [Text.Encoding]::ASCII.GetBytes("voxhub-settings-v1"), [Security.Cryptography.DataProtectionScope]::CurrentUser)
  $hex = ($enc | ForEach-Object { $_.ToString("x2") }) -join ""
  "{`"server`":`"http://127.0.0.1:7997`",`"username`":`"slA1`",`"remember_password`":true,`"password_enc`":`"$hex`",`"volume_pct`":100,`"device_tokens`":{}}" | Set-Content "$cfg\settings.json" -Encoding ascii
  $line = "srvlist :"
  foreach ($phase in "phase1", "phase2") {
    $a = "--autotest srvlist --server http://127.0.0.1:7997 --password $pw1 --name slA1 --run-id srvlist --log-dir `"$sd\$phase`" --mode $phase --settings-dir `"$cfg`""
    $p = Start-Process "$B\concord.exe" -ArgumentList $a -PassThru
    $null = $p.Handle   # keeps the handle, so ExitCode is readable once it has exited
    if (-not $p.WaitForExit(180000)) { Stop-Process -Id $p.Id -Force; $line += " $phase=TIMEOUT"; break }
    $line += " $phase=$($p.ExitCode)"
    if ($p.ExitCode -ne 0) { break }
  }
  foreach ($h in $hubs) { if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force } }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($sd) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  $line
}

# CALLADEX scenario (v1.0.1): the easter egg. Two members sit online (idle, stopped by PID at the
# end); the owner types "calladex" on the home screen (the credits card), then in a channel sends
# a plain message containing the word (it must arrive) and the exact /calladex (it must open the
# members' card and never reach the hub).
if (Want "calladex") {
  $hub = Start-Hub "calladex"
  $parts = @()
  foreach ($n in "cdxB1", "cdxC1") {
    $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest idle --server http://127.0.0.1:7997 --password x --name $n --run-id calladex --log-dir `"$G\calladex\$n`"" -PassThru
    Start-Sleep 2
  }
  Start-Sleep 3
  $c = Start-Process "$B\concord.exe" -ArgumentList "--autotest calladex --server http://127.0.0.1:7997 --password x --name dono --run-id calladex --log-dir `"$G\calladex\dono`"" -PassThru
  $null = $c.Handle
  $line = "calladex :"
  if ($c.WaitForExit(180000)) { $line += " dono=$($c.ExitCode)" } else { Stop-Process -Id $c.Id -Force; $line += " dono=TIMEOUT" }
  foreach ($p in $parts) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  Stop-Hub $hub "calladex"
  $line
}
# UIX scenario (v1.1): the interface fixes. The owner (uix-a, "dono") makes many channels and,
# in the smallest window, checks that the sidebar and every chat (channel, DM, docked beside the
# cards) scroll, draws a member's speaking ring, opens a message's menu on a link and the text
# box's menu (its "Colar" and "Selecionar tudo" through the menu's own path, never the real
# clipboard); uix-b sits in the voice room until A is done. With VOX_AUTOTEST_SHOT_DIR set, A
# saves uix-*.png there.
Run-Clients "uix"     @(@{role="uix-b"; name="uixB1"; extra=""; delay=3}, @{role="uix-a"; name="dono"; extra=""; delay=0}) 300
# DONATE scenario (v1.1): the Pix popups and page. Only on request (-Only donate), and only on a
# build with the test key: cargo build --release -p voxhub-app --features donate-test. Never the
# release binary. With VOX_AUTOTEST_SHOT_DIR set, it saves donate-*.png there.
if ($Only -contains "donate") { Run-Clients "donate" @(@{role="donate"; name="doa"; extra="--settings-dir `"$G\donate\cfg`""; delay=0}) 180 }

# RESP scenario (v1.1 I6): the responsive layout rules. resp-a (the owner, "dono", --mode gate)
# resizes and zooms its own window at run time (760x440 at 125% and 100%, 1280x720) and checks,
# at each: every window, menu and popup inside the app's window; a header's title never under
# its buttons; the member list collapsed in the zoomed-in smallest window and back over the chat
# with its button; the docked chat and the stage inside the window with the stage's minimum; the
# settings and server windows and the dialogs inside it. resp-b sits in the voice room.
Run-Clients "resp"    @(@{role="resp-b"; name="respB1"; extra=""; delay=3}, @{role="resp-a"; name="dono"; extra="--mode gate"; delay=0}) 300

# KEYS scenario (v1.1): keyboard shortcuts. keys-a joins the voice room alone and presses its
# shortcuts with synthetic key events handed straight to the app's shortcut matcher (never through
# the system: nothing is typed anywhere, and the test app installs no keyboard hook): deafen with
# auto-repeat, a plain key paused while typing, mute, push-to-talk held and released, a shortcut to
# a sound that no longer exists, Go Live's picker, leave. With VOX_AUTOTEST_SHOT_DIR set, it saves
# atalhos.png / atalhos-2.png (the settings page) there.
Run-Clients "keys"    @(@{role="keys-a"; name="dono"; extra=""; delay=0}) 180

# THUMBS scenario (v1.1): Go Live hover cards. thumbs-a (the owner, "dono") streams test window 1
# in the public voice room; thumbs-b and thumbs-c (never in that room) rest the pointer on its "ao
# vivo" tag (handed to egui as the window's own input): the still picture and the app's name
# ("test_window") come from the hub. A moves its live to a private room for B: B still gets it
# (and joins, for the stream card's "Assistir transmissão" and its hover card), C gets a 404. With
# VOX_AUTOTEST_SHOT_DIR set, B saves card-hover-sidebar / botao-assistir / card-hover-palco.png.
Run-Clients "thumbs"  @(@{role="thumbs-c"; name="thC1"; extra=""; delay=1}, @{role="thumbs-b"; name="thB1"; extra=""; delay=1}, @{role="thumbs-a"; name="dono"; extra="--hwnd $h1"; delay=0}) 300

# TLS scenarios (v1.1 T0, autotest_tls.rs). Every scenario above already runs over TLS (a typed
# address of a 1.1 hub: TLS on first use, LiveKit through the local tunnel and the hub's port).
# tls-ab: tls-a (the owner) makes an invite that carries the hub's certificate; tls-b joins with
# only the code (pinned TLS), chats and joins voice with A through the tunnel.
if (Want "tls-ab") {
  $tx = "$G\tls-ab\x"; New-Item -ItemType Directory $tx -Force | Out-Null
  Run-Clients "tls-ab" @(@{role="tls-a"; name="dono"; extra="--mode `"$tx`""; delay=3}, @{role="tls-b"; name="tlsB1"; extra="--mode `"$tx`""; delay=0}) 300
}
# The released v1.0.2 app, unzipped once from releases\v1.0.2 into a cache under %TEMP% (never in
# the repo). It has no background test mode: run it only with offscreen=$true.
function Get-Old102 {
  $cache102 = Join-Path $env:TEMP "voxhub-gate-cache\v1.0.2"
  $old102 = "$cache102\concord.exe"
  if (-not (Test-Path $old102)) {
    New-Item -ItemType Directory $cache102 -Force | Out-Null
    Expand-Archive "$Assets\releases\v1.0.2\concord-v1.0.2-PUBLICO.zip" $cache102 -Force
    $inner = Get-ChildItem $cache102 -Recurse -Filter concord.exe | Select-Object -First 1
    if ($inner -and $inner.FullName -ne $old102) { Move-Item $inner.FullName $old102 -Force }
  }
  $old102
}
# tls-old: the released v1.0.2 app (no TLS: plain HTTP, LiveKit directly on 7970) and this app
# (TLS, LiveKit through the hub's port) in a DM and a private call together on a 1.1 hub ("auto").
# The old app has no background test mode: it runs on the gate's own desktop (Start-Offscreen),
# unzipped once from releases\v1.0.2 into a cache under %TEMP% (never in the repo).
if (Want "tls-old") {
  $old102 = Get-Old102
  Run-Clients "tls-old" @(@{role="dm-b"; name="dmB7"; exe=$old102; offscreen=$true; extra=""; delay=4}, @{role="dm-a"; name="dmA7"; extra=""; delay=0}) 300
}
# A hub of this scenario's own (data folder, log folder, LiveKit 7970-7972) from any concord-server.
function Start-TlsHub($dir, $exe) {
  $d = "$dir\data"; New-Item -ItemType Directory $d -Force | Out-Null
  @"
server_name = "gate"
server_password = "x"
owner = "dono"
bind = "127.0.0.1:7997"
public_host = "127.0.0.1"
[livekit]
api_key = "APIgate000001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = 7970
rtc_tcp_port = 7971
rtc_udp_port = 7972
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
  Start-Process $exe -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$dir\hub`"" -WindowStyle Hidden -PassThru
}
function Stop-TlsHub($h, $dir) {
  if ($h -and -not $h.HasExited) { Stop-Process -Id $h.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($dir) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
}
# tls-mitm: the app logs in to hub A (its certificate pinned, the password saved) and goes home;
# hub A is then replaced by hub B (a fresh install: another certificate) at the same address. One
# click on the saved server must show "A identidade do servidor mudou" with NOTHING sent: hub B
# logs every request (VOX_LOG debug for its HTTP layer) and must have none, and no login. Then
# "Confiar no novo código" (with its second confirmation) logs in to B.
if (Want "tls-mitm") {
  $md = "$G\tls-mitm"; $mx = "$md\x"; New-Item -ItemType Directory $mx -Force | Out-Null
  $line = "tls-mitm :"
  $ha = Start-TlsHub "$md\hubA" "$B\concord-server.exe"
  Start-Sleep 4
  $app = Start-Process "$B\concord.exe" -ArgumentList "--autotest tls-mitm --server http://127.0.0.1:7997 --password x --name mitmA1 --run-id tls-mitm --log-dir `"$md\app`" --mode `"$mx`"" -PassThru
  $null = $app.Handle
  $until = (Get-Date).AddSeconds(90)
  while (-not (Test-Path "$mx\saved.txt") -and (Get-Date) -lt $until -and -not $app.HasExited) { Start-Sleep 1 }
  Stop-TlsHub $ha "$md\hubA"
  $env:VOX_LOG = "info,voxhub_server::api=debug"
  $hb = Start-TlsHub "$md\hubB" "$B\concord-server.exe"
  Remove-Item Env:VOX_LOG
  Start-Sleep 4
  "swapped" | Set-Content "$mx\swapped.txt" -Encoding ascii
  $until = (Get-Date).AddSeconds(60)
  while (-not (Test-Path "$mx\mismatch.txt") -and (Get-Date) -lt $until -and -not $app.HasExited) { Start-Sleep 1 }
  Start-Sleep 2
  $blog = (Get-ChildItem "$md\hubB\hub" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  $seen = @()
  if ($blog -match "mitmA1") { $seen += "login" }
  if ($blog -match "path=") { $seen += "http-request" }
  $verdict = if (-not (Test-Path "$mx\mismatch.txt")) { "no-mismatch" } elseif ($seen.Count -eq 0) { "ok" } else { $seen -join "," }
  $verdict | Set-Content "$mx\checked.txt" -Encoding ascii
  $line += " hubB-before-trust=$verdict"
  if ($app.WaitForExit(120000)) { $line += " app=$($app.ExitCode)" } else { Stop-Process -Id $app.Id -Force; $line += " app=TIMEOUT" }
  # Sanity: hub B does log requests (the check above was not blind): the trusted login reached it.
  $blog = (Get-ChildItem "$md\hubB\hub" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  $line += if ($blog -match "path=" -and $blog -match "mitmA1") { " hubB-after-trust=seen" } else { " hubB-after-trust=BLIND" }
  Stop-TlsHub $hb "$md\hubB"
  $line
}
# tls-oldhub: this app against the released v1.0.2 hub (no TLS): asked once before going plain,
# the open lock, chat and voice; the saved entry then goes plain without asking.
if (Want "tls-oldhub") {
  $od = "$G\tls-oldhub"
  $h = Start-TlsHub $od "$Assets\releases\v1.0.2\concord-server.exe"
  Start-Sleep 4
  $p = Start-Process "$B\concord.exe" -ArgumentList "--autotest tls-oldhub --server http://127.0.0.1:7997 --password x --name tohA1 --run-id tls-oldhub --log-dir `"$od\app`"" -PassThru
  $null = $p.Handle
  $line = "tls-oldhub :"
  if ($p.WaitForExit(180000)) { $line += " app=$($p.ExitCode)" } else { Stop-Process -Id $p.Id -Force; $line += " app=TIMEOUT" }
  Stop-TlsHub $h $od
  $line
}

# E2EE-DEV scenario (v1.1 E0, autotest_e2ee.rs): per-device identity keys. e2ee-b (another member)
# and e2ee-a (its own settings folder: the real e2ee.json, DPAPI) log in and register; once A is
# registered, e2ee-a2 logs in with A's name as a second computer: A gets "Um aparelho novo foi
# adicionado a sua conta", removes it in Configuracoes -> Privacidade -> "Seus aparelhos", A2
# learns it was removed and B sees the list follow. With VOX_AUTOTEST_SHOT_DIR set, A saves
# seus-aparelhos.png / seus-aparelhos-depois.png there.
if (Want "e2ee-dev") {
  $ex = "$G\e2ee-dev\x"; New-Item -ItemType Directory $ex, "$G\e2ee-dev\a-settings" -Force | Out-Null
  Run-Clients "e2ee-dev" @(@{role="e2ee-b"; name="e2B1"; extra="--mode `"$ex`""; delay=2}, @{role="e2ee-a"; name="e2A1"; extra="--mode `"$ex`" --settings-dir `"$G\e2ee-dev\a-settings`""; delay=2}, @{role="e2ee-a2"; name="e2A1"; extra="--mode `"$ex`" --log-dir `"$G\e2ee-dev\e2A1-second`""; delay=0}) 240
}

# E2DM scenario (v1.1 E2, autotest_e2dm.rs): end-to-end encrypted DMs. B (dmB8) and A (dmA8) on
# their first computers and A's second one (e2dm-a2, registered before A sends: it must read A's
# own lines). A sends a text and a generated photo, B opens, draws and decodes them, replies,
# verifies the safety code; then A's third computer (e2dm-new) logs in: the old lines say "antes
# deste aparelho existir" and B gets the local lines and "Verifique de novo". The released v1.0.2
# app joins as B's second device (its own dm-b script, on the gate's own desktop): it must log the
# placeholder as the conversation's preview (its script then fails waiting for a readable text:
# expected, not counted). Afterwards the hub's database and image files are scanned: none of the
# texts, no image signature. VOX_AUTOTEST_SHOT_DIR: e2dm-conversa / e2dm-codigo / e2dm-codigo-verificado /
# e2dm-verifique-de-novo.png from B.
if (Want "e2dm") {
  $ed = "$G\e2dm\x"; New-Item -ItemType Directory $ed -Force | Out-Null
  $old102 = Get-Old102
  Run-Clients "e2dm" @(@{role="e2dm-b"; name="dmB8"; extra="--mode `"$ed`""; delay=2}, @{role="e2dm-a"; name="dmA8"; extra="--mode `"$ed`""; delay=3}, @{role="e2dm-a2"; name="dmA8"; label="dmA8-second"; extra="--mode `"$ed`""; delay=2}, @{role="e2dm-new"; name="dmA8"; label="dmA8-new"; extra="--mode `"$ed`""; delay=2}, @{role="dm-b"; name="dmB8"; label="dmB8-v102"; exe=$old102; offscreen=$true; extra=""; delay=0}) 420
  $ol = (Get-ChildItem "$G\e2dm\dmB8-v102" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  if ($ol -match 'preview=Some\("Mensagem criptografada\. Atualize o concord para ler\."\)') { "e2dm : v1.0.2 sees the placeholder=ok" } else { "e2dm : v1.0.2 sees the placeholder=MISSING" }
  # The scan: the database (with its WAL) and every stored file, byte by byte (Latin-1: one char per byte).
  $latin = [Text.Encoding]::GetEncoding(28591)
  $texts = @("e2dm segredo", "senha do cofre", "e2dm legenda secreta", "e2dm resposta cifrada") | ForEach-Object { $latin.GetString([Text.Encoding]::UTF8.GetBytes($_)) }
  $magic = @($latin.GetString([byte[]](0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A)), "WEBPVP8", "JFIF", "GIF87a", "GIF89a", $latin.GetString([byte[]](0x45,0x78,0x69,0x66,0,0)))
  $files = @(Get-ChildItem "$G\e2dm\data" -File -Filter "voxhub.db*") + @(Get-ChildItem "$G\e2dm\data\attachments" -File -Recurse -ErrorAction SilentlyContinue)
  $bad = @(); $blobs = 0
  foreach ($f in $files) {
    $s = $latin.GetString([IO.File]::ReadAllBytes($f.FullName))
    foreach ($t in $texts) { if ($s.Contains($t)) { $bad += "$($f.Name): text" } }
    foreach ($m in $magic) { if ($s.Contains($m)) { $bad += "$($f.Name): image signature" } }
    if ($f.DirectoryName -ne "$G\e2dm\data") {
      $blobs++
      if ($s.Length -ge 3 -and $s.StartsWith($latin.GetString([byte[]](0xFF,0xD8,0xFF)))) { $bad += "$($f.Name): starts like a JPEG" }
    }
  }
  # A scan of nothing proves nothing: the database and the photo's files must be there.
  if (-not ($files | Where-Object { $_.Name -eq "voxhub.db" })) { $bad += "voxhub.db not found" }
  if ($blobs -eq 0) { $bad += "no image files stored" }
  # The hub's own logs must not have the texts either.
  $hubLogs = @(Get-ChildItem "$G\e2dm\hub" -File -Recurse -ErrorAction SilentlyContinue)
  foreach ($f in $hubLogs) {
    $s = $latin.GetString([IO.File]::ReadAllBytes($f.FullName))
    foreach ($t in $texts) { if ($s.Contains($t)) { $bad += "hub log $($f.Name): text" } }
  }
  if ($bad.Count -eq 0) { "e2dm : plaintext scan=ok ($($files.Count) files, $blobs image files, $($hubLogs.Count) hub logs)" } else { "e2dm : plaintext scan=FOUND " + ($bad -join "; ") }
}

# E2V scenarios (v1.1 E1, autotest_e2v.rs): end-to-end encrypted calls.
# e2v: the owner (e2v-a) turns "Chamada criptografada de ponta a ponta" on for the first voice room
# and joins with a synthetic tone as the microphone (VOX_TEST_MIC_TONE), the synthetic camera and a
# pre-encoded H.264 live (VOX_TEST_H264: the hardware encoder's own output of test window 1, made
# once by capture_probe; the path NVENC / Media Foundation take). e2v-b checks that A's voice
# (~100 blocks/s with sound), camera and live (~30 fps) decode, A's media state is Ok and A shows
# as speaking; then e2v-c comes and goes (keys rotate twice) and B must never lose A's tone for
# more than 500 ms. Afterwards: no rx-health decode-error report in any log. VOX_AUTOTEST_SHOT_DIR:
# e2v-canal.png (A: the setting) and e2v-sala.png (B: the call with its header).
function Get-E2vH264 {
  $f = "$G\e2v.h264"
  if (-not (Test-Path $f)) {
    $p = Start-Process "$B\examples\capture_probe.exe" -ArgumentList "encode --window $h1 --secs 12 --fps 30 --out `"$f`" --force-key-every 60" -WindowStyle Hidden -PassThru -RedirectStandardOutput "$G\e2v-probe.txt" -RedirectStandardError "$G\e2v-probe-err.txt"
    if (-not $p.WaitForExit(90000)) { Stop-Process -Id $p.Id -Force }
  }
  $f
}
if (Want "e2v") {
  $vx = "$G\e2v\x"; New-Item -ItemType Directory $vx -Force | Out-Null
  $env:VOX_TEST_H264 = Get-E2vH264
  "e2v : test live file $((Get-Item $env:VOX_TEST_H264 -ErrorAction SilentlyContinue).Length) bytes"
  $env:VOX_TEST_MIC_TONE = "440"
  # E2V_HWND=1: A shares test window 1 (live capture) instead of the file. E2V_PLAIN=1: the same run
  # in a room without encryption (a baseline for the numbers; the encryption checks are skipped).
  $va = if ($env:E2V_HWND) { "--hwnd $h1" } else { "" }
  # (A live capture adapts to the bandwidth estimate: B also checks the camera+live ramp, with a
  # camera that spends its bitrate like a real one; see camlive.)
  if ($env:E2V_HWND) { $env:E2V_RAMP_SECS = "40"; $env:VOX_FAKE_CAMERA_NOISE = "4" }
  Run-Clients "e2v" @(@{role="e2v-a"; name="dono"; extra="--mode `"$vx`" $va"; delay=3}, @{role="e2v-b"; name="Tati"; extra="--mode `"$vx`""; delay=2}, @{role="e2v-c"; name="Caio"; extra="--mode `"$vx`""; delay=0}) 420
  Remove-Item Env:VOX_TEST_MIC_TONE, Env:VOX_TEST_H264
  Remove-Item Env:E2V_RAMP_SECS, Env:VOX_FAKE_CAMERA_NOISE -ErrorAction SilentlyContinue
  $vl = (Get-ChildItem "$G\e2v" -Recurse -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  "e2v : rx-health decode-error reports=" + ([regex]::Matches($vl, "told the publisher its video decodes badly here")).Count
  "e2v : key rotations=" + ([regex]::Matches($vl, "e2ee: my media key rotated")).Count + ", ignored keys=" + ([regex]::Matches($vl, "e2ee: a media key that does not open here")).Count
  "e2v : clients that got the SFU's active speakers=" + ([regex]::Matches($vl, "e2ee: the SFU's active speakers arrive")).Count + " (of 3)"
  foreach ($m in [regex]::Matches($vl, "AUTOTEST e2v: (live ramp[^\r\n]*|B receives from A[^\r\n]*|while C came[^\r\n]*|C hears A[^\r\n]*|live \(pre-encoded[^\r\n]*)")) { "e2v : " + $m.Groups[1].Value }
  $sends = [regex]::Matches($vl, "AUTOTEST e2v: (A sends[^\r\n]*)"); if ($sends.Count -gt 0) { "e2v : " + $sends[$sends.Count - 1].Groups[1].Value }
  # The live's sender side: frames WebRTC dropped (bandwidth ramp-up) and keyframes, per report.
  $fb = [regex]::Matches($vl, "encoder feedback backend=(h264file|nvenc|mf) [^\r\n]*?keyframes_sent=(\d+)[^\r\n]*?webrtc_encoded=(\d+) webrtc_dropped=(\d+)")
  if ($fb.Count -gt 0) { "e2v : sender reports, backend sent/dropped (camera and live): " + (($fb | Select-Object -First 8 | ForEach-Object { "$($_.Groups[1].Value) $($_.Groups[3].Value)/$($_.Groups[4].Value)" }) -join ", ") }
}
# CAMLIVE scenario (v1.1 G5): camera + Go Live from one person. The e2v roles in a room without
# encryption: A turns the synthetic camera on, then goes live capturing test window 1 (NVENC / Media
# Foundation, 720p30); B must see the live reach 27 fps (a ~2 s window) within 40 s of watching it
# (E2V_RAMP_SECS) and then ~30 fps on average, while C comes and goes. The synthetic camera gets
# sensor-like noise (VOX_FAKE_CAMERA_NOISE): its flat picture alone sends ~330 kbps of the ~1 Mbps
# WebRTC gives it, which no real webcam does, and WebRTC's estimate (never above ~1.5x what is
# sent, split evenly between the two videos) then stayed at ~2 Mbps with the live at 15 fps for
# good (G5: a test artifact; with real cameras v1.0.2 and v1.1 reach full rate). Reports the ramp
# and what A sent.
if (Want "camlive") {
  $cx = "$G\camlive\x"; New-Item -ItemType Directory $cx -Force | Out-Null
  $plain0 = $env:E2V_PLAIN
  $env:VOX_TEST_MIC_TONE = "440"; $env:E2V_PLAIN = "1"; $env:E2V_RAMP_SECS = "40"; $env:VOX_FAKE_CAMERA_NOISE = "4"
  Run-Clients "camlive" @(@{role="e2v-a"; name="dono"; extra="--mode `"$cx`" --hwnd $h1"; delay=3}, @{role="e2v-b"; name="Tati"; extra="--mode `"$cx`""; delay=2}, @{role="e2v-c"; name="Caio"; extra="--mode `"$cx`""; delay=0}) 420
  Remove-Item Env:VOX_TEST_MIC_TONE, Env:E2V_RAMP_SECS, Env:VOX_FAKE_CAMERA_NOISE
  if ($null -eq $plain0) { Remove-Item Env:E2V_PLAIN } else { $env:E2V_PLAIN = $plain0 }
  $cl = (Get-ChildItem "$G\camlive" -Recurse -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  foreach ($m in [regex]::Matches($cl, "AUTOTEST e2v: (live ramp[^\r\n]*|B receives from A[^\r\n]*)")) { "camlive : " + $m.Groups[1].Value }
  $sends = [regex]::Matches($cl, "AUTOTEST e2v: (A sends[^\r\n]*)"); if ($sends.Count -gt 0) { "camlive : " + $sends[$sends.Count - 1].Groups[1].Value }
}
# e2v-old: the owner (e2v-admin) turns the encryption on; the released v1.0.2 app (its own `voice`
# script, on the gate's own desktop) must be refused with the hub's message, and nobody may get in.
if (Want "e2v-old") {
  $vo = "$G\e2v-old\x"; New-Item -ItemType Directory $vo -Force | Out-Null
  $old102 = Get-Old102
  Run-Clients "e2v-old" @(@{role="e2v-admin"; name="dono"; extra="--mode `"$vo`""; delay=25}, @{role="voice"; name="velho1"; exe=$old102; offscreen=$true; extra=""; delay=0}) 150
  $ol = (Get-ChildItem "$G\e2v-old\velho1" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  if ($ol -match "Atualize o concord para entrar") { "e2v-old : v1.0.2 refused with the message=ok" } else { "e2v-old : v1.0.2 refused with the message=MISSING" }
}
# e2v-dmcall: the dm scenario's call between two 1.1 apps with device keys must be end-to-end
# encrypted (VOX_TEST_EXPECT_E2EE: B checks it and that A's media decrypts). VOX_AUTOTEST_SHOT_DIR:
# e2v-chamada-dm.png (B).
if (Want "e2v-dmcall") {
  $env:VOX_TEST_EXPECT_E2EE = "1"
  Run-Clients "e2v-dmcall" @(@{role="dm-b"; name="dmB9"; extra=""; delay=2}, @{role="dm-a"; name="dmA9"; extra=""; delay=0}) 300
  Remove-Item Env:VOX_TEST_EXPECT_E2EE
  $dl = (Get-ChildItem "$G\e2v-dmcall" -Recurse -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  "e2v-dmcall : rx-health decode-error reports=" + ([regex]::Matches($dl, "told the publisher its video decodes badly here")).Count
}

# SINGLE scenario (v1.1): one concord at a time. Two launches with the same isolated test key
# (VOX_SINGLE_INSTANCE_TEST; the real app's key is never touched): the second must quit at once
# (exit 0, "already running" in its log) and the first must hear its "show" signal. No hub.
if (Want "single") {
  $sd = "$G\single"; New-Item -ItemType Directory $sd -Force | Out-Null
  $env:VOX_SINGLE_INSTANCE_TEST = "gate$PID"
  $first = Start-Process "$B\concord.exe" -ArgumentList "--autotest kit --hold 20 --name s1 --run-id single --log-dir `"$sd\first`"" -PassThru
  $null = $first.Handle
  Start-Sleep 5
  $second = Start-Process "$B\concord.exe" -ArgumentList "--autotest kit --hold 20 --name s2 --run-id single --log-dir `"$sd\second`"" -PassThru
  $null = $second.Handle
  $line = "single :"
  if ($second.WaitForExit(15000)) { $line += " second=$($second.ExitCode)" } else { Stop-Process -Id $second.Id -Force; $line += " second=STILL-RUNNING" }
  Start-Sleep 3
  $l1 = (Get-ChildItem "$sd\first" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  $l2 = (Get-ChildItem "$sd\second" -Filter *.log -ErrorAction SilentlyContinue | Get-Content -Raw) -join ""
  $line += if ($l2 -match "already running") { " second-log=ok" } else { " second-log=MISSING" }
  $line += if ($l1 -match "another launch asked this one to show its window") { " first-signal=ok" } else { " first-signal=MISSING" }
  $line += if ($l1 -match "window restored from the tray") { " first-shown=ok" } else { " first-shown=MISSING" }
  if ($first.WaitForExit(40000)) { $line += " first=$($first.ExitCode)" } else { Stop-Process -Id $first.Id -Force; $line += " first=TIMEOUT" }
  Remove-Item Env:VOX_SINGLE_INSTANCE_TEST
  $line
}
foreach ($p in $tw1, $tw2) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
# (GATE_NO_LIVE_CHECK=1: agents never touch the user's live server, not even its health.)
if (-not $env:GATE_NO_LIVE_CHECK) { try { "live: " + (Invoke-RestMethod http://127.0.0.1:7890/api/health -TimeoutSec 3 | ConvertTo-Json -Compress) } catch { "live: off" } }
Run-Clients "emoji"   @(@{role="emoji-a"; name="emA1"; extra=""; delay=0}) 180   # v1.1 colour emoji (autotest_emoji.rs); VOX_AUTOTEST_SHOT_DIR: emoji-*.png
