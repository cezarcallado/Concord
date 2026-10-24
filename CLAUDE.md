# CLAUDE.md

Guidance for Claude Code when someone asks for help with concord in this repository, usually "set up a concord server for my group". concord is a self-hosted, Discord-style app for Windows: a desktop client (`app/`, egui) and a server (`hub/`, axum + SQLite) that starts a LiveKit SFU for voice and video. The UI and the user-facing docs are in Portuguese (pt-BR); talk to the user in their language.

## What the user usually wants

1. **Use concord with friends** → point them to the Releases page: `concord-v1.0.1-PUBLICO.zip` (the app). Nothing to build.
2. **Host a server** → two options, explain both and recommend the first for most people:
   - **From the app:** `concord.exe` → "Criar / abrir servidor". It downloads LiveKit, tries UPnP, and makes invite codes. The server runs while the app is open or in the tray.
   - **Standalone:** `concord-servidor-v1.1.0.zip` from Releases → `baixar-livekit.bat` → `liberar-firewall.bat` → `iniciar-servidor.bat` → set `server_password` in `data\config.toml` → restart → open `concord.exe` on the same PC at `127.0.0.1` to become the owner (the first loopback login claims ownership and must set a personal password).
3. **Build from source** → `cargo build --release -p voxhub-app -p voxhub-server` (Rust stable, MSVC, Visual Studio Build Tools with C++). Outputs: `target\release\concord.exe` and `concord-server.exe`. Package names stay `voxhub-*` on purpose.

## Making the server reachable

Ask the user how their friends will connect, then pick one:

- **VPN (recommended):** Tailscale or ZeroTier on every PC. Friends connect to the host's VPN address. No router changes, and the traffic is encrypted by the VPN.
- **Port forwarding:** TCP 7890 (hub), 7880 (LiveKit signal), 7881 (LiveKit TCP), UDP 7882 (media) to the host PC. Never suggest the router's DMZ.
- **IPv6:** the standalone server (v1.0.2 and later) listens on IPv4 and IPv6. Friends with IPv6 can usually reach the host's global IPv6 without port forwarding, even behind CGNAT; some routers block incoming IPv6 until it is allowed in their IPv6 firewall. The server prints a connection check a few seconds after it starts.
- **CGNAT:** if the router's WAN address is in 100.64.0.0/10 or a private range, or differs from the public IP, port forwarding won't work. Use IPv6 (if both sides have it) or a VPN.
- Windows Firewall: allow `concord.exe` / `concord-server.exe` on the networks the user actually uses when prompted (a network Windows marks as public needs the public box too). For the standalone server, the user runs `liberar-firewall.bat` from the server zip (v1.0.2 and later): it adds inbound rules for the server and LiveKit on all profiles, IPv4 and IPv6; `remover-firewall.bat` removes them.

Then the owner creates an invite in the app ("Convidar") with an expiry and a use limit, and sends it privately.

## Safety rules for you (Claude)

- **TLS since v1.1:** a 1.1 server and 1.1 apps talk over TLS with the server's own pinned certificate (`tls = "auto"` by default; `"required"` refuses 1.0 apps). Keep the server's `data	ls` folder when moving it. Still recommend a strong `server_password` and a VPN when the user exposes a server to the internet. Read `SECURITY.md` before giving security advice.
- **Never print, paste, commit or upload** the contents of `data\` (config with secrets, `voxhub.db` with messages), `%APPDATA%\concord\`, `%LOCALAPPDATA%\concord\`, logs, invite codes or passwords. Don't ask the user to paste them into an issue.
- Don't change router settings, firewall rules or system settings yourself; tell the user what to change.
- Don't stop processes by image name (other concord or voxhub instances may be the user's live server); stop only processes you started, by PID.
- Don't open ports or contact servers other than the user's own.

## Developing

- Workspace: `app/` (client), `hub/` (server), `crates/vox-proto` (wire types), `crates/vox-client-core` (hub client, voice, Go Live), `crates/vox-capture` (Windows capture and hardware encoders), `crates/vox-log`, `vendor/` (patched LiveKit SDK, DeepFilterNet subset).
- Wire compatibility matters: friends run different versions. New protocol fields are additive (`#[serde(default, skip_serializing_if = ...)]`), `PROTOCOL_VERSION` is not bumped for additions, and database migrations are additive and idempotent. Identifiers like the `vox1-` invite prefix and the `vox-rx-health` topic are kept on purpose.
- Tests: `cargo test --workspace`. End-to-end: `tools\gate.ps1` (needs a release build and `bin\livekit-server.exe`). It runs throwaway servers on loopback ports 7997/7990/7998 with LiveKit on 7970–7982, never the user's real ports, and its windows open in the background without taking focus.
- Keep it light: the whole point of concord is low CPU, GPU and RAM. Avoid per-frame allocations, continuous repaints and new heavy dependencies; measure with `tools\perf.ps1`.
- Code and comments must not contain real people's names, IP addresses or device identifiers.
