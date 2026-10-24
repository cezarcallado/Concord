# Design-review screenshots (v1.0): the ui_kit gallery and the main screens (home, call, chat,
# settings), saved as PNGs by the app itself (autotest roles `kit` and `shots`, VOX_AUTOTEST_SHOT_DIR).
# Its own fresh dev hub (hub 7997, LiveKit 7970-7972); never touches a live server on 7890/7880-7882.
# Background test mode: the windows stay behind the others and never take the focus; silent.
#
# Build first:  cargo build --release --workspace   (with CARGO_TARGET_DIR=target-v02)
# Usage:        powershell -ExecutionPolicy Bypass -File tools\shots.ps1 [-OutDir dist\design\shots-phase1]
#               -Stage: the call stage instead (phase 2b, autotest role `stage-b` with two streamers and
#               a camera): stage-normal, stage-pinned, stage-menu, stage-fullscreen, stage-dock.
#               -Nav: the navigation and the center modes instead (phase 2a, autotest roles `nav-a` /
#               `nav-b`): home, sidebar-chat, mode-cards-chat, mode-chat, mode-cards, hover-reactions, dm.
#               -Phase3: the secondary windows instead (phase 3a, autotest role `shots3` with two test
#               windows for the Go Live picker): settings-<section>[-2], camera, soundboard, trim,
#               voice-fx, golive, golive-audio.
#               -Calladex: concord (v1.0.1) instead: home, calladex-creditos ("calladex" typed on the home
#               screen), calladex (the /calladex card, with three partners online) and sobre (autotest role
#               `calladex`, partners `idle`).
#               -Phase3b: the server window, menus, profile, DMs and dialogs instead (phase 3b, autotest
#               roles `shots3b-a` / `shots3b-b`, and `shots3b-host` hosting a server in the app on the
#               test ports 7990/7980-7982 with the router left alone): admin-<page>, menu-pessoa,
#               menu-card, perfil-cartao, perfil-editor, dm-cabecalho, dm-bloqueado, nova-mensagem,
#               chamada-recebida, aviso, dialogo-*, host-painel, host-inicio.
#               v1.0 phase 4, for every mode: -Window 900x600 (another window size, in points);
#               -Zoom 1.25 (a stand-in for Windows' display scale: egui's zoom, in a window that much
#               bigger in pixels); -Prefix small- (in front of every file name, so these pictures sit
#               next to the normal ones).
#               The pictures may be shared: VOX_SHOT_PRIVACY=1 swaps the PC's microphones, speakers
#               and video card for generic names, and the work folder (logs, test sounds: every path
#               a picture can show) is C:\Users\Public\concord-shots, never the user's own profile.
#               -Resp (v1.1 I6): the responsive matrix instead (autotest roles `resp-a` / `resp-b`):
#               every screen at 760x440, 1024x640, 1280x720 and 1920x1080 at zoom 1.0 / 1.25 / 1.5 and
#               2560x1440 at 2.0, saved as <scene>__<w>x<h>-z<zoom>.png, the layout rules checked at
#               each (the breaks are logged as "AUTOTEST resp DEFECT"), then the host panel the same
#               way. VOX_RESP_COMBOS=760x440@1.25,1280x720@1 and VOX_RESP_SCENES=canal,dm,host narrow it.
param([string]$OutDir = "dist\design\shots-phase1", [switch]$Stage, [switch]$Nav, [switch]$Phase3, [switch]$Phase3b, [switch]$Calladex, [switch]$Resp,
  [string]$Window = "", [double]$Zoom = 1.0, [string]$Prefix = "", [string]$Assets = "")
$ErrorActionPreference = "Continue"
$R = Split-Path $PSScriptRoot -Parent
# CARGO_TARGET_DIR may be relative to the repo (target-v02) or absolute (a worktree sharing the main tree's).
$B = if (-not $env:CARGO_TARGET_DIR) { "$R\target-v02\release" } elseif ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) { Join-Path $env:CARGO_TARGET_DIR "release" } else { Join-Path $R "$env:CARGO_TARGET_DIR\release" }
# bin\livekit-server.exe (and, for the gate, releases\) come from -Assets: the repo by default, or the
# main tree when this runs from a git worktree without them (read only).
if (-not $Assets) { $Assets = $R }
$LK = "$Assets\bin\livekit-server.exe"
$G = "C:\Users\Public\concord-shots"
Remove-Item $G -Recurse -Force -ErrorAction SilentlyContinue; New-Item -ItemType Directory $G | Out-Null
New-Item -ItemType Directory "$G\tmp" | Out-Null
$env:TEMP = "$G\tmp"; $env:TMP = "$G\tmp"
$env:VOX_SHOT_PRIVACY = "1"
if ($Window) { $env:VOX_AUTOTEST_WINDOW = $Window } else { Remove-Item Env:VOX_AUTOTEST_WINDOW -ErrorAction SilentlyContinue }
if ($Zoom -ne 1.0) { $env:VOX_AUTOTEST_ZOOM = ([string]$Zoom).Replace(',', '.') } else { Remove-Item Env:VOX_AUTOTEST_ZOOM -ErrorAction SilentlyContinue }
$env:VOX_AUTOTEST_SHOT_PREFIX = $Prefix
$Out = if ([System.IO.Path]::IsPathRooted($OutDir)) { $OutDir } else { Join-Path $R $OutDir }
New-Item -ItemType Directory $Out -Force | Out-Null
$env:VOX_NO_PLAYBACK = "1"; $env:VOX_NO_MIC = "1"; $env:VOX_FAKE_CAMERA = "1"
$env:VOX_AUTOTEST_SHOT_DIR = $Out

function Wait-Exit($p, $secs) {
  if (-not $p.WaitForExit($secs * 1000)) { Stop-Process -Id $p.Id -Force; return "TIMEOUT" }
  return "$($p.ExitCode)"
}

# The gallery: no server.
if (-not $Stage -and -not $Nav -and -not $Phase3 -and -not $Phase3b -and -not $Calladex -and -not $Resp) {
  $k = Start-Process "$B\concord.exe" -ArgumentList "--autotest kit --hold 2 --name kit --run-id kit --log-dir `"$G\kit`"" -PassThru
  $null = $k.Handle
  "kit : " + (Wait-Exit $k 60)
}

# The main screens, with a partner whose camera is on in the voice room.
$d = "$G\hub\data"; New-Item -ItemType Directory $d -Force | Out-Null
@"
server_name = "Turma da Lua"
server_password = "x"
owner = "dono"
bind = "127.0.0.1:7997"
public_host = "127.0.0.1"
[livekit]
api_key = "APIshots00001"
api_secret = "0123456789abcdef0123456789abcdef0123456789abcdef"
signal_port = 7970
rtc_tcp_port = 7971
rtc_udp_port = 7972
use_external_ip = false
"@ | Set-Content "$d\config.toml" -Encoding ascii
$hub = Start-Process "$B\concord-server.exe" -ArgumentList "--data-dir `"$d`" --livekit-bin `"$LK`" --log-dir `"$G\hub\log`"" -WindowStyle Hidden -PassThru
Start-Sleep 4
$srv = "--server http://127.0.0.1:7997 --password x"
if ($Resp) {
  # Test windows to stream, as many streamers (VOX_RESP_STREAMERS, 1 to 3; 3 by default) and a
  # camera; the owner goes through the matrix.
  $n = if ($env:VOX_RESP_STREAMERS) { [Math]::Max(1, [Math]::Min(3, [int]$env:VOX_RESP_STREAMERS)) } else { 3 }
  $env:VOX_RESP_STREAMERS = "$n"
  $tws = @(1..$n) | ForEach-Object { Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw$_.txt" -PassThru }
  Start-Sleep 5
  $hw = @(1..$n) | ForEach-Object { [int64]((Get-Content "$G\tw$_.txt" -ErrorAction SilentlyContinue | Select-String -Pattern '^hwnd=(\d+)' | Select-Object -First 1).Matches.Groups[1].Value) }
  $parts = @()
  $names = @("lua", "sol", "mar")
  for ($i = 0; $i -lt $n; $i++) {
    $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest resp-b $srv --name $($names[$i]) --hwnd $($hw[$i]) --run-id resp --log-dir `"$G\$($names[$i])`"" -PassThru
    Start-Sleep 2
  }
  $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest resp-b $srv --name rio --mode cam --run-id resp --log-dir `"$G\rio`"" -PassThru
  Start-Sleep 3
  $s = Start-Process "$B\concord.exe" -ArgumentList "--autotest resp-a $srv --name dono --run-id resp --log-dir `"$G\dono`"" -PassThru
  $null = $s.Handle
  "resp : " + (Wait-Exit $s 3600)
  Start-Sleep 3
  foreach ($p in $parts + $tws) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  if (-not $env:VOX_RESP_SCENES -or $env:VOX_RESP_SCENES -match 'host') {
    # The host panel: a server hosted inside the app (the gate's host-mode environment).
    $env:VOX_HOST_DIR = "$G\host"; $env:VOX_HOST_OFFLINE = "1"; $env:VOX_HOST_BIND = "127.0.0.1"
    $env:VOX_HOST_FIRST_BLOCK = "1"; $env:VOX_HOST_LAN = "127.0.0.1/8"; $env:VOX_HOST_LIVEKIT_BIN = $LK
    $h = Start-Process "$B\concord.exe" -ArgumentList "--autotest resp-a --mode host --name dono --run-id resp --log-dir `"$G\host-log`"" -PassThru
    $null = $h.Handle
    "resp host : " + (Wait-Exit $h 900)
    foreach ($k in "VOX_HOST_DIR", "VOX_HOST_OFFLINE", "VOX_HOST_BIND", "VOX_HOST_FIRST_BLOCK", "VOX_HOST_LAN", "VOX_HOST_LIVEKIT_BIN") { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
    Start-Sleep 1
    Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape("$G\host") } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  }
  Get-ChildItem "$G" -Recurse -Filter *.log | Select-String -Pattern 'AUTOTEST resp DEFECT' | ForEach-Object { $_.Line -replace '^.*AUTOTEST resp DEFECT ', 'DEFECT ' }
  "{0} pictures" -f (Get-ChildItem $Out -Filter *.png).Count
  return
}
if ($Calladex) {
  # Three members online first (they are the card's entries), then the owner takes the pictures.
  $parts = @()
  foreach ($n in "Lua", "Sol", "Rio") {
    $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest idle --hold 120 $srv --name $n --run-id calladex --log-dir `"$G\$n`"" -PassThru
    Start-Sleep 2
  }
  Start-Sleep 3
  $c = Start-Process "$B\concord.exe" -ArgumentList "--autotest calladex $srv --name dono --run-id calladex --log-dir `"$G\dono`"" -PassThru
  $null = $c.Handle
  "calladex : " + (Wait-Exit $c 180)
  foreach ($p in $parts) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  Get-ChildItem $Out -Filter *.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
  return
}
if ($Nav) {
  # The owner checks and takes the pictures; the other person sits in the voice room (camera on).
  $nb = Start-Process "$B\concord.exe" -ArgumentList "--autotest nav-b $srv --name visita --run-id nav --log-dir `"$G\nav-b`"" -PassThru
  Start-Sleep 3
  $na = Start-Process "$B\concord.exe" -ArgumentList "--autotest nav-a $srv --name dono --run-id nav --log-dir `"$G\nav-a`"" -PassThru
  $null = $na.Handle
  "nav : " + (Wait-Exit $na 240)
  Start-Sleep 2
  if (-not $nb.HasExited) { Stop-Process -Id $nb.Id -Force }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  Get-ChildItem $Out -Filter *.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
  return
}
if ($Phase3b) {
  # The host panel first: a server hosted inside the app (the gate's host-mode environment).
  $env:VOX_HOST_DIR = "$G\host"; $env:VOX_HOST_OFFLINE = "1"; $env:VOX_HOST_BIND = "127.0.0.1"
  $env:VOX_HOST_FIRST_BLOCK = "1"; $env:VOX_HOST_LAN = "127.0.0.1/8"; $env:VOX_HOST_LIVEKIT_BIN = $LK
  $h = Start-Process "$B\concord.exe" -ArgumentList "--autotest shots3b-host --name dono --run-id shots3b --log-dir `"$G\host-log`"" -PassThru
  $null = $h.Handle
  "shots3b-host : " + (Wait-Exit $h 180)
  foreach ($k in "VOX_HOST_DIR", "VOX_HOST_OFFLINE", "VOX_HOST_BIND", "VOX_HOST_FIRST_BLOCK", "VOX_HOST_LAN", "VOX_HOST_LIVEKIT_BIN") { Remove-Item "Env:$k" -ErrorAction SilentlyContinue }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape("$G\host") } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  # Then the owner and a partner on the dev hub.
  $pb = Start-Process "$B\concord.exe" -ArgumentList "--autotest shots3b-b $srv --name lua --run-id shots3b --log-dir `"$G\lua`"" -PassThru
  Start-Sleep 3
  $pa = Start-Process "$B\concord.exe" -ArgumentList "--autotest shots3b-a $srv --name dono --run-id shots3b --log-dir `"$G\dono`"" -PassThru
  $null = $pa.Handle
  "shots3b-a : " + (Wait-Exit $pa 300)
  Start-Sleep 2
  if (-not $pb.HasExited) { Stop-Process -Id $pb.Id -Force }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  Get-ChildItem $Out -Filter *.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
  return
}
if ($Phase3) {
  # Two test windows: the only cards the Go Live picker is left with (never the person's own windows).
  $tws = @(1, 2) | ForEach-Object { Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw$_.txt" -PassThru }
  Start-Sleep 4
  $s = Start-Process "$B\concord.exe" -ArgumentList "--autotest shots3 $srv --name dono --run-id shots3 --log-dir `"$G\dono`"" -PassThru
  $null = $s.Handle
  "shots3 : " + (Wait-Exit $s 300)
  foreach ($p in $tws) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  Get-ChildItem $Out -Filter *.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
  return
}if ($Stage) {
  # Two test windows to stream (they print their own "hwnd=<n>"), two streamers, a camera, and B.
  $tws = @(1, 2) | ForEach-Object { Start-Process "$B\examples\test_window.exe" -RedirectStandardOutput "$G\tw$_.txt" -PassThru }
  Start-Sleep 5
  $hw = @(1, 2) | ForEach-Object { [int64]((Get-Content "$G\tw$_.txt" -ErrorAction SilentlyContinue | Select-String -Pattern '^hwnd=(\d+)' | Select-Object -First 1).Matches.Groups[1].Value) }
  $parts = @()
  $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest multi-a $srv --name Lua --hwnd $($hw[0]) --run-id stage --log-dir `"$G\a1`"" -PassThru
  Start-Sleep 2
  $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest multi-a $srv --name Sol --hwnd $($hw[1]) --run-id stage --log-dir `"$G\a2`"" -PassThru
  Start-Sleep 2
  $parts += Start-Process "$B\concord.exe" -ArgumentList "--autotest cam-a --hold 20 $srv --name Rio --run-id stage --log-dir `"$G\cam`"" -PassThru
  Start-Sleep 3
  $s = Start-Process "$B\concord.exe" -ArgumentList "--autotest stage-b $srv --name dono --run-id stage --log-dir `"$G\dono`"" -PassThru
  $null = $s.Handle
  "stage : " + (Wait-Exit $s 300)
  Start-Sleep 5
  foreach ($p in $parts + $tws) { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } }
  if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
  Start-Sleep 1
  Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
  Get-ChildItem $Out -Filter stage-*.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
  return
}
$cam = Start-Process "$B\concord.exe" -ArgumentList "--autotest cam-a --hold 120 $srv --name dono --run-id shots --log-dir `"$G\dono`"" -PassThru
Start-Sleep 3
$s = Start-Process "$B\concord.exe" -ArgumentList "--autotest shots $srv --name visita --run-id shots --log-dir `"$G\visita`"" -PassThru
$null = $s.Handle
"shots : " + (Wait-Exit $s 180)
if (-not $cam.HasExited) { Stop-Process -Id $cam.Id -Force }
if (-not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
Start-Sleep 1
Get-CimInstance Win32_Process -Filter "Name='livekit-server.exe'" | Where-Object { $_.CommandLine -match [regex]::Escape($G) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
Get-ChildItem $Out -Filter *.png | ForEach-Object { "{0}  {1:N0} KB" -f $_.Name, ($_.Length / 1KB) }
