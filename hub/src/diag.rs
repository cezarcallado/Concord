//! Startup diagnostics of the standalone server (`concord-server.exe`, v1.0.2), in plain pt-BR on
//! the console and in the log: where it listens (IPv4 / IPv6), this PC's public IPv6, the public
//! IPv4 and whether the connection is behind CGNAT, whether the Windows Firewall lets the server
//! and LiveKit in on the network in use (and whether that network is "Pública"), and what to do
//! about it.
//!
//! Everything here is read-only and best effort: a STUN question, a UPnP search (nothing is
//! mapped), and one hidden PowerShell that reads the firewall rules and the network profile (no
//! elevation needed). It runs in the background after the server started, every part is bounded
//! in time, and nothing here can stop or delay the server. The app's host mode does not run it
//! (it has its own panel); a server bound to loopback only (the tests, the gate) skips it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use crate::host::nat::{self, Reach};

/// How long the router search and the STUN question may take.
const NET_TIMEOUT: Duration = Duration::from_secs(4);
/// How long the firewall check may take (PowerShell reading ~1000 rules: 2-3 s usually).
#[cfg(windows)]
const FW_TIMEOUT: Duration = Duration::from_secs(25);

/// What the server knows about itself when it starts.
#[derive(Debug, Clone)]
pub struct Input {
    pub addrs: Vec<SocketAddr>,
    pub dual_stack: bool,
    pub hub_port: u16,
    pub signal_port: u16,
    pub rtc_tcp_port: u16,
    pub rtc_udp_port: u16,
    /// `concord-server.exe` itself.
    pub server_exe: Option<PathBuf>,
    /// The LiveKit it runs (`None`: `--no-livekit`).
    pub livekit_exe: Option<PathBuf>,
}

impl Input {
    /// Nobody but this PC can reach the hub: nothing to diagnose.
    fn loopback_only(&self) -> bool {
        self.addrs.iter().all(|a| a.ip().is_loopback())
    }

    fn listens_on_ipv6(&self) -> bool {
        self.addrs.iter().any(|a| a.is_ipv6() && !a.ip().is_loopback())
    }
}

/// Run the diagnostics in the background; the report goes to the console (stderr) and the log.
pub fn spawn(input: Input) {
    if input.loopback_only() {
        return;
    }
    tokio::spawn(async move {
        let facts = gather(&input).await;
        let lines = report(&input, &facts);
        // Not eprintln!: it panics when stderr is gone (a closed pipe), and release builds abort
        // on panic. A report nobody can read is simply dropped.
        {
            use std::io::Write;
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "\n{}\n", lines.join("\n"));
            let _ = err.flush();
        }
        tracing::info!(target: "diag", report = %lines.join(" | "), "connection diagnostics");
    });
}

// ---------------------------------------------------------------- facts

/// What the checks found.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub ipv6: Option<Ipv6Addr>,
    pub lan: Option<Ipv4Addr>,
    /// The public IPv4 (router's or STUN's) and what kind of connection it is.
    pub reach: Option<Reach>,
    pub public_v4: Option<Ipv4Addr>,
    /// `None`: not checked (not Windows) or the check failed.
    pub firewall: Option<Firewall>,
}

async fn gather(input: &Input) -> Facts {
    let exes: Vec<PathBuf> = [input.server_exe.clone(), input.livekit_exe.clone()].into_iter().flatten().collect();
    let router = tokio::task::spawn_blocking(|| {
        use crate::host::upnp::PortMapper;
        let igd = crate::host::upnp::discover(NET_TIMEOUT).ok()?;
        igd.external_ip().ok()
    });
    let stun = crate::host::stun::public_ipv4(NET_TIMEOUT);
    let fw = firewall_check(&exes);
    let (wan, seen, firewall) = tokio::join!(router, stun, fw);
    let wan = wan.ok().flatten();
    let mine = seen.is_some_and(|ip| crate::linkpreview::ssrf::interface_addresses().iter().any(|(a, _)| *a == IpAddr::V4(ip)));
    let reach = nat::classify(wan, seen.map(IpAddr::V4), mine);
    let public_v4 = match wan {
        Some(IpAddr::V4(v4)) if matches!(reach, Reach::Open) => Some(v4),
        _ => seen,
    };
    Facts {
        ipv6: crate::ipv6::stable_ipv6(),
        lan: crate::host::net::lan_ipv4().map(|(ip, _)| ip),
        reach: Some(reach),
        public_v4,
        firewall,
    }
}

// ---------------------------------------------------------------- Windows Firewall

/// A firewall profile (Windows calls a network "Domínio", "Privada" or "Pública").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Domain,
    Private,
    Public,
}

impl Profile {
    const ALL: [Profile; 3] = [Profile::Domain, Profile::Private, Profile::Public];

    fn parse(s: &str) -> Option<Profile> {
        match s.trim().to_ascii_lowercase().as_str() {
            "domain" | "domainauthenticated" => Some(Profile::Domain),
            "private" => Some(Profile::Private),
            "public" => Some(Profile::Public),
            _ => None,
        }
    }

    fn pt(self) -> &'static str {
        match self {
            Profile::Domain => "Domínio",
            Profile::Private => "Privada",
            Profile::Public => "Pública",
        }
    }
}

/// One firewall rule for one of the programs checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Which program (index into the list checked).
    pub exe: usize,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    pub profiles: Vec<Profile>,
}

/// What the firewall check read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Firewall {
    pub rules: Vec<Rule>,
    /// Each profile and whether the firewall is on for it.
    pub on: Vec<(Profile, bool)>,
    /// The networks this PC is connected to: their profile and name.
    pub networks: Vec<(Profile, String)>,
}

/// Whether a program gets in on one profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// A rule blocks it (Windows makes these when its question is cancelled).
    Blocked,
    /// No rule: the firewall drops what comes in (friends see a time-out).
    NoRule,
    FirewallOff,
}

impl Firewall {
    pub fn verdict(&self, exe: usize, p: Profile) -> Verdict {
        if self.on.iter().any(|(q, on)| *q == p && !on) {
            return Verdict::FirewallOff;
        }
        let applies = |r: &&Rule| r.exe == exe && r.enabled && r.inbound && r.profiles.contains(&p);
        let rules: Vec<&Rule> = self.rules.iter().filter(applies).collect();
        // A block rule wins over any allow rule.
        if rules.iter().any(|r| !r.allow) {
            Verdict::Blocked
        } else if rules.iter().any(|r| r.allow) {
            Verdict::Allowed
        } else {
            Verdict::NoRule
        }
    }

    /// The profiles of the networks in use (each once).
    pub fn active(&self) -> Vec<Profile> {
        let mut v: Vec<Profile> = Vec::new();
        for (p, _) in &self.networks {
            if !v.contains(p) {
                v.push(*p);
            }
        }
        v
    }
}

/// Parse what [`FW_SCRIPT`] prints. `None` when the output is cut short (no `END` line).
pub fn parse_firewall(out: &str) -> Option<Firewall> {
    let mut fw = Firewall::default();
    let mut complete = false;
    for line in out.lines() {
        let cols: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
        let yes = |s: &str| s.trim().eq_ignore_ascii_case("true");
        match cols.as_slice() {
            ["RULE", exe, enabled, direction, action, profile] => {
                let Ok(exe) = exe.trim().parse() else { continue };
                let profiles = if profile.trim().eq_ignore_ascii_case("any") {
                    Profile::ALL.to_vec()
                } else {
                    profile.split(',').filter_map(Profile::parse).collect()
                };
                fw.rules.push(Rule {
                    exe,
                    enabled: yes(enabled),
                    inbound: direction.trim().eq_ignore_ascii_case("inbound"),
                    allow: action.trim().eq_ignore_ascii_case("allow"),
                    profiles,
                });
            }
            ["FW", name, enabled] => {
                if let Some(p) = Profile::parse(name) {
                    fw.on.push((p, yes(enabled)));
                }
            }
            ["NET", category, name @ ..] => {
                if let Some(p) = Profile::parse(category) {
                    fw.networks.push((p, name.join(" ").trim().to_string()));
                }
            }
            ["END"] => complete = true,
            _ => {}
        }
    }
    complete.then_some(fw)
}

/// Reads, never writes: the rules of the programs in `$exes` (matched by full path, any case,
/// environment variables expanded), each firewall profile's on/off, and the network profiles.
const FW_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
foreach ($f in (Get-NetFirewallApplicationFilter -PolicyStore ActiveStore)) {
  $p = [Environment]::ExpandEnvironmentVariables([string]$f.Program)
  for ($i = 0; $i -lt $exes.Count; $i++) {
    if ($p -ieq $exes[$i]) {
      foreach ($r in ($f | Get-NetFirewallRule)) { "RULE`t$i`t$($r.Enabled)`t$($r.Direction)`t$($r.Action)`t$($r.Profile)" }
    }
  }
}
foreach ($x in (Get-NetFirewallProfile -PolicyStore ActiveStore)) { "FW`t$($x.Name)`t$($x.Enabled)" }
foreach ($n in (Get-NetConnectionProfile)) { "NET`t$($n.NetworkCategory)`t$($n.Name)" }
"END"
"#;

/// The script for these programs: `$exes` first, as single-quoted PowerShell strings.
pub fn firewall_script(exes: &[PathBuf]) -> String {
    let list: Vec<String> = exes.iter().map(|p| format!("'{}'", p.display().to_string().replace('\'', "''"))).collect();
    format!("$exes = @({})\n{FW_SCRIPT}", list.join(", "))
}

#[cfg(windows)]
async fn firewall_check(exes: &[PathBuf]) -> Option<Firewall> {
    use base64::Engine;
    use std::process::Stdio;
    let script = firewall_script(exes);
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = match tokio::time::timeout(FW_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::info!("diag: firewall check could not run: {e}");
            return None;
        }
        Err(_) => {
            tracing::info!("diag: firewall check took too long");
            return None;
        }
    };
    let parsed = parse_firewall(&String::from_utf8_lossy(&out.stdout));
    if parsed.is_none() {
        tracing::info!(status = %out.status, "diag: firewall check gave no answer");
    }
    parsed
}

#[cfg(not(windows))]
async fn firewall_check(_exes: &[PathBuf]) -> Option<Firewall> {
    None
}

// ---------------------------------------------------------------- the report

fn exe_name(p: &Option<PathBuf>, fallback: &str) -> String {
    p.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| fallback.to_string())
}

/// The report's lines (pure: tests feed it made-up facts).
pub fn report(input: &Input, f: &Facts) -> Vec<String> {
    let mut out = vec!["---- Diagnóstico de conexão do servidor ----".to_string()];
    let mut todo: Vec<String> = Vec::new();
    let ports = format!("TCP {}, {} e {} e UDP {}", input.hub_port, input.signal_port, input.rtc_tcp_port, input.rtc_udp_port);

    out.push(format!("Escutando em: {}", crate::listen::describe(&input.addrs, input.dual_stack)));

    // IPv6.
    match (f.ipv6, input.listens_on_ipv6()) {
        (Some(v6), true) => {
            out.push(format!("IPv6 público deste PC: {v6}"));
            todo.push(format!(
                "Amigos com IPv6 entram por [{v6}]:{} sem abrir portas no roteador (os convites criados neste servidor já levam esse endereço, se o public_host do config.toml estiver vazio). Se não der, o roteador pode estar bloqueando IPv6 vindo de fora: procure \"firewall IPv6\" nas configurações dele.",
                input.hub_port
            ));
        }
        (Some(v6), false) => {
            out.push(format!("IPv6 público deste PC: {v6} (mas o servidor NÃO escuta em IPv6)"));
            todo.push("Para aceitar IPv6, use bind = \"[::]:7890\" no data\\config.toml e tire o ipv6 = false (se houver).".into());
        }
        (None, _) => out.push("IPv6 público deste PC: nenhum (a operadora ou o roteador não entregam IPv6 a este PC).".into()),
    }

    // IPv4.
    let lan = f.lan.map(|ip| format!(" ({ip})")).unwrap_or_default();
    match (f.reach, f.public_v4) {
        (Some(Reach::Cgnat(_)), v4) => {
            out.push(format!("IPv4 público: {} (esta internet está atrás de CGNAT: o IPv4 é dividido com outros clientes da operadora).", v4.map_or("?".into(), |ip| ip.to_string())));
            todo.push("CGNAT: abrir portas no roteador não adianta pelo IPv4. Use o IPv6 (se houver) ou uma VPN (Tailscale ou ZeroTier), ou peça IP público à operadora.".into());
        }
        (Some(Reach::Open), Some(v4)) => {
            out.push(format!("IPv4 público: {v4} (o roteador responde por UPnP)."));
            todo.push(format!("Pelo IPv4: redirecione no roteador as portas {ports} para este PC{lan}, ou use uma VPN (Tailscale ou ZeroTier)."));
        }
        (Some(Reach::Direct), v4) => {
            out.push(format!("IPv4 público: {} (direto neste PC, sem roteador no caminho).", v4.map_or("?".into(), |ip| ip.to_string())));
        }
        (Some(Reach::Unknown), v4) => {
            out.push(format!("IPv4 público: {} (o roteador não respondeu por UPnP).", v4.map_or("?".into(), |ip| ip.to_string())));
            todo.push(format!("Pelo IPv4: confira se as portas {ports} estão redirecionadas no roteador para este PC{lan}. Se a operadora usar CGNAT, isso não basta: use o IPv6 ou uma VPN."));
        }
        (Some(Reach::Offline), _) | (None, _) => out.push("IPv4 público: desconhecido (nenhum servidor da internet respondeu).".into()),
        (Some(Reach::Open), None) => out.push("IPv4 público: desconhecido.".into()),
    }

    // Windows Firewall.
    match &f.firewall {
        None if cfg!(windows) => {
            out.push("Firewall do Windows: não deu para conferir.".into());
            todo.push("Se os amigos não conseguirem entrar, rode liberar-firewall.bat (na pasta do servidor).".into());
        }
        None => {}
        Some(fw) => {
            let active = fw.active();
            let nets: Vec<String> = fw.networks.iter().map(|(p, name)| format!("\"{name}\" ({})", p.pt())).collect();
            if nets.is_empty() {
                out.push("Rede atual: nenhuma conectada.".into());
            } else {
                out.push(format!("Rede atual: {}", nets.join(", ")));
            }
            let names = [exe_name(&input.server_exe, "concord-server.exe"), exe_name(&input.livekit_exe, "livekit-server.exe")];
            let checked = if input.livekit_exe.is_some() { 2 } else { 1 };
            let mut blocked_somewhere = false;
            let mut only_private = false;
            for (i, name) in names.iter().enumerate().take(checked) {
                let per: Vec<String> = active
                    .iter()
                    .map(|p| {
                        let v = fw.verdict(i, *p);
                        if !matches!(v, Verdict::Allowed | Verdict::FirewallOff) {
                            blocked_somewhere = true;
                            if *p == Profile::Public && fw.verdict(i, Profile::Private) == Verdict::Allowed {
                                only_private = true;
                            }
                        }
                        let what = match v {
                            Verdict::Allowed => "liberado",
                            Verdict::Blocked => "BLOQUEADO",
                            Verdict::NoRule => "sem regra (bloqueado)",
                            Verdict::FirewallOff => "firewall desligado",
                        };
                        format!("{what} na rede {}", p.pt())
                    })
                    .collect();
                if per.is_empty() {
                    out.push(format!("Firewall do Windows: {name}: sem rede para conferir."));
                } else {
                    out.push(format!("Firewall do Windows: {name}: {}.", per.join(", ")));
                }
            }
            if active.contains(&Profile::Public) {
                out.push("Atenção: a rede deste PC está marcada como Pública.".into());
            }
            if only_private {
                todo.push("O Firewall só libera o servidor em redes Privadas, e a rede deste PC está como Pública: rode liberar-firewall.bat (libera em todas as redes) ou marque a rede como Privada nas Configurações do Windows.".into());
            } else if blocked_somewhere {
                todo.push("Rode liberar-firewall.bat (na pasta do servidor) e aceite o pedido do Windows: ele libera o servidor e a voz no Firewall do Windows em todas as redes.".into());
            }
        }
    }

    if todo.is_empty() {
        out.push("Tudo certo daqui. Se alguém não conseguir entrar, veja o LEIA-ME-SERVIDOR.txt.".into());
    } else {
        out.push("O que fazer:".into());
        out.extend(todo.into_iter().map(|t| format!("  - {t}")));
    }
    out.push("--------------------------------------------".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> Input {
        Input {
            addrs: vec!["[::]:7890".parse().unwrap()],
            dual_stack: true,
            hub_port: 7890,
            signal_port: 7880,
            rtc_tcp_port: 7881,
            rtc_udp_port: 7882,
            server_exe: Some(PathBuf::from(r"C:\concord-servidor\bin\concord-server.exe")),
            livekit_exe: Some(PathBuf::from(r"C:\concord-servidor\bin\livekit-server.exe")),
        }
    }

    /// What the script prints on a PC where the user allowed both programs on private networks
    /// only (what Windows' own question offers first), on a network marked as public.
    const ONLY_PRIVATE: &str = "RULE\t0\tTrue\tInbound\tAllow\tPrivate\r\n\
        RULE\t0\tTrue\tInbound\tAllow\tPrivate\r\n\
        RULE\t1\tTrue\tInbound\tAllow\tPrivate\r\n\
        RULE\t1\tFalse\tInbound\tAllow\tPublic\r\n\
        RULE\t1\tTrue\tOutbound\tAllow\tAny\r\n\
        FW\tDomain\tTrue\r\nFW\tPrivate\tTrue\r\nFW\tPublic\tTrue\r\n\
        NET\tPublic\tRede 2\r\nEND\r\n";

    #[test]
    fn firewall_output_is_parsed() {
        let fw = parse_firewall(ONLY_PRIVATE).unwrap();
        assert_eq!(fw.rules.len(), 5);
        assert_eq!(fw.rules[3], Rule { exe: 1, enabled: false, inbound: true, allow: true, profiles: vec![Profile::Public] });
        assert_eq!(fw.rules[4].profiles, Profile::ALL, "Any = every profile");
        assert_eq!(fw.on, [(Profile::Domain, true), (Profile::Private, true), (Profile::Public, true)]);
        assert_eq!(fw.networks, [(Profile::Public, "Rede 2".to_string())]);
        assert_eq!(fw.active(), [Profile::Public]);
        for exe in 0..2 {
            assert_eq!(fw.verdict(exe, Profile::Private), Verdict::Allowed);
            assert_eq!(fw.verdict(exe, Profile::Public), Verdict::NoRule, "disabled and outbound rules do not count");
        }
        // Cut short: no answer at all rather than a wrong one.
        assert_eq!(parse_firewall("RULE\t0\tTrue\tInbound\tAllow\tAny\n"), None);
        assert_eq!(parse_firewall(""), None);
        // Junk lines are skipped.
        let fw = parse_firewall("WARNING: something\nRULE\tx\tTrue\tInbound\tAllow\tAny\nNET\tDomainAuthenticated\tcorp\tnet\nEND").unwrap();
        assert!(fw.rules.is_empty());
        assert_eq!(fw.networks, [(Profile::Domain, "corp net".to_string())]);
    }

    #[test]
    fn block_rules_win_and_a_firewall_off_lets_everything_in() {
        let fw = parse_firewall(
            "RULE\t0\tTrue\tInbound\tAllow\tPrivate, Public\nRULE\t0\tTrue\tInbound\tBlock\tPublic\n\
             RULE\t1\tTrue\tInbound\tAllow\tAny\nFW\tPublic\tTrue\nFW\tPrivate\tFalse\nNET\tPrivate\tCasa\nEND",
        )
        .unwrap();
        assert_eq!(fw.verdict(0, Profile::Public), Verdict::Blocked);
        assert_eq!(fw.verdict(0, Profile::Private), Verdict::FirewallOff);
        assert_eq!(fw.verdict(1, Profile::Public), Verdict::Allowed);
        assert_eq!(fw.verdict(2, Profile::Public), Verdict::NoRule);
    }

    #[test]
    fn the_report_says_what_to_do() {
        let facts = Facts {
            ipv6: Some("3fff::10".parse().unwrap()),
            lan: Some(Ipv4Addr::new(192, 168, 0, 20)),
            reach: Some(Reach::Cgnat(nat::CgnatSign::SharedRange)),
            public_v4: Some(Ipv4Addr::new(192, 0, 2, 7)),
            firewall: parse_firewall(ONLY_PRIVATE),
        };
        let text = report(&input(), &facts).join("\n");
        println!("{text}");
        assert!(text.contains("Escutando em: [::]:7890 (IPv4 e IPv6)"), "{text}");
        assert!(text.contains("IPv6 público deste PC: 3fff::10"), "{text}");
        assert!(text.contains("[3fff::10]:7890"), "{text}");
        assert!(text.contains("CGNAT"), "{text}");
        assert!(text.contains("VPN"), "{text}");
        assert!(text.contains("concord-server.exe: sem regra (bloqueado) na rede Pública"), "{text}");
        assert!(text.contains("livekit-server.exe: sem regra (bloqueado) na rede Pública"), "{text}");
        assert!(text.contains("marcada como Pública"), "{text}");
        assert!(text.contains("rode liberar-firewall.bat"), "{text}");

        // All fine: an open router, both programs allowed everywhere, no IPv6.
        let fine = Facts {
            ipv6: None,
            lan: Some(Ipv4Addr::new(192, 168, 0, 20)),
            reach: Some(Reach::Open),
            public_v4: Some(Ipv4Addr::new(192, 0, 2, 7)),
            firewall: parse_firewall("RULE\t0\tTrue\tInbound\tAllow\tAny\nRULE\t1\tTrue\tInbound\tAllow\tAny\nNET\tPrivate\tCasa\nEND"),
        };
        let text = report(&input(), &fine).join("\n");
        assert!(text.contains("concord-server.exe: liberado na rede Privada"), "{text}");
        assert!(!text.contains("liberar-firewall.bat"), "{text}");
        assert!(text.contains("TCP 7890, 7880 e 7881 e UDP 7882 para este PC (192.168.0.20)"), "{text}");
        assert!(text.contains("nenhum"), "{text}");

        // IPv4 only (ipv6 = false) on a PC that has IPv6; the firewall could not be read.
        let mut v4only = input();
        v4only.addrs = vec!["0.0.0.0:7890".parse().unwrap()];
        v4only.dual_stack = false;
        let facts = Facts { ipv6: Some("3fff::10".parse().unwrap()), ..Facts::default() };
        let text = report(&v4only, &facts).join("\n");
        assert!(text.contains("0.0.0.0:7890 (IPv4)"), "{text}");
        assert!(text.contains("NÃO escuta em IPv6"), "{text}");
        if cfg!(windows) {
            assert!(text.contains("não deu para conferir"), "{text}");
        }
    }

    #[test]
    fn the_script_quotes_paths() {
        let s = firewall_script(&[PathBuf::from(r"C:\O'Brien\bin\concord-server.exe"), PathBuf::from(r"D:\x\livekit-server.exe")]);
        assert!(s.starts_with(r"$exes = @('C:\O''Brien\bin\concord-server.exe', 'D:\x\livekit-server.exe')"), "{s}");
        assert!(s.contains("Get-NetFirewallApplicationFilter") && s.trim_end().ends_with("\"END\""), "{s}");
        // Read-only: nothing that changes a rule.
        for verb in ["New-NetFirewall", "Set-NetFirewall", "Remove-NetFirewall", "netsh"] {
            assert!(!s.contains(verb), "{verb}");
        }
    }

    #[test]
    fn loopback_servers_are_not_diagnosed() {
        let mut i = input();
        assert!(!i.loopback_only() && i.listens_on_ipv6());
        i.addrs = vec!["127.0.0.1:7997".parse().unwrap()];
        assert!(i.loopback_only() && !i.listens_on_ipv6());
        i.addrs = vec!["[::1]:7997".parse().unwrap()];
        assert!(i.loopback_only());
    }

    /// The whole report with this PC's real facts (STUN, a UPnP search, the firewall rules of the
    /// test program itself); nothing is bound or changed.
    #[tokio::test]
    #[ignore = "asks the internet and the router; cargo test -p voxhub-server --lib diag -- --ignored --nocapture"]
    async fn the_real_report_on_this_pc() {
        let mut i = input();
        i.server_exe = std::env::current_exe().ok();
        i.livekit_exe = None;
        let t = std::time::Instant::now();
        let facts = gather(&i).await;
        println!("{} ms", t.elapsed().as_millis());
        for line in report(&i, &facts) {
            println!("{line}");
        }
    }

    /// The real check on this PC (Windows): it must finish, read only, and parse.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore = "runs PowerShell; cargo test -p voxhub-server --lib diag -- --ignored"]
    async fn the_real_firewall_check_answers() {
        let exe = std::env::current_exe().unwrap();
        let t = std::time::Instant::now();
        let fw = firewall_check(&[exe]).await.expect("an answer");
        println!("{} ms: {fw:?}", t.elapsed().as_millis());
        assert!(!fw.on.is_empty());
    }
}
