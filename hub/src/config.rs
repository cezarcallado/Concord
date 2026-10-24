//! `<data-dir>/config.toml`: created with defaults on first run.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util::random_hex;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_name: String,
    /// Shared password clients must send on login. Empty = open server.
    pub server_password: String,
    /// Where the hub listens. `[::]:7890` (the default since v1.0.2) and `0.0.0.0:7890` (older
    /// configs) both mean every interface, IPv4 and IPv6 (see [`Config::ipv6`]); a specific
    /// address (`192.168.0.10:7890`, `127.0.0.1:7890`, `[2001:db8::1]:7890`) is used exactly as
    /// written. See `listen.rs`.
    pub bind: String,
    /// v1.0.2: with an every-interface `bind`, also take IPv6 connections (default `true`).
    /// `false` keeps the old IPv4-only behaviour (`0.0.0.0`).
    pub ipv6: bool,
    /// Host/IP clients use to reach LiveKit (the host's public IP or DDNS name).
    pub public_host: String,
    /// The owner ("Dono", item 12): this username has every permission. Compared
    /// case-insensitively. Empty (the usual case) = "quem hospeda vira dono": the first login from
    /// the server PC itself (or whoever uses the claim code the console shows) becomes the owner,
    /// stored in the database. Set, it pins the owner and nothing can claim or transfer it.
    pub owner: String,
    /// Chat images: total storage cap, in GB (default 5). When it is reached new images are
    /// refused with a friendly message (or, with `attachment_retention_days`, the oldest images
    /// older than a day make room).
    pub max_attachment_storage_gb: f64,
    /// Chat images each person may upload per day (UTC), in MB (default 200).
    pub attachment_daily_mb: u64,
    /// Delete chat images older than this many days (the messages stay, with "[imagem]").
    /// 0 (the default) keeps them forever.
    pub attachment_retention_days: u32,
    /// v1.1: TLS on the hub's port (`tls.rs`). `auto` (the default): TLS for the apps that speak
    /// it, plain HTTP for older ones; `required`: older apps get a "update" message (this PC's own
    /// connections may still be plain); `off`: no TLS, as before 1.1.
    pub tls: crate::tls::TlsMode,
    pub livekit: LiveKitConfig,
}

impl Config {
    /// [`Config::max_attachment_storage_gb`] in bytes.
    pub fn attachment_storage_cap(&self) -> u64 {
        let gb = if self.max_attachment_storage_gb.is_finite() { self.max_attachment_storage_gb.max(0.0) } else { 5.0 };
        (gb * 1024.0 * 1024.0 * 1024.0) as u64
    }

    /// [`Config::attachment_daily_mb`] in bytes.
    pub fn attachment_daily_bytes(&self) -> u64 {
        self.attachment_daily_mb.saturating_mul(1024 * 1024)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LiveKitConfig {
    pub api_key: String,
    pub api_secret: String,
    /// Signal (HTTP/WebSocket) port.
    pub signal_port: u16,
    /// RTC over TCP (fallback when UDP is blocked).
    pub rtc_tcp_port: u16,
    /// RTC over UDP, single-port mux.
    pub rtc_udp_port: u16,
    /// Let LiveKit discover its public IP via STUN. Needed for remote peers behind port forwarding.
    pub use_external_ip: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_name: "concord".into(),
            server_password: String::new(),
            bind: "[::]:7890".into(),
            ipv6: true,
            public_host: "127.0.0.1".into(),
            owner: String::new(),
            max_attachment_storage_gb: 5.0,
            attachment_daily_mb: 200,
            attachment_retention_days: 0,
            tls: crate::tls::TlsMode::Auto,
            livekit: LiveKitConfig::default(),
        }
    }
}

impl Default for LiveKitConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            signal_port: 7880,
            rtc_tcp_port: 7881,
            rtc_udp_port: 7882,
            use_external_ip: false,
        }
    }
}

/// Load `config.toml` from `dir`, writing a fresh one (with random LiveKit keys) if missing.
pub fn load_or_create(dir: &Path) -> Result<Config> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("config.toml");
    if !path.exists() {
        let text = default_file(&random_hex::<8>(), &random_hex::<32>());
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), "created default config.toml");
    }
    let text = std::fs::read_to_string(&path)?;
    let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if cfg.livekit.api_key.is_empty() || cfg.livekit.api_secret.len() < 32 {
        anyhow::bail!("{}: livekit.api_key must be set and livekit.api_secret must be at least 32 chars", path.display());
    }
    cfg.public_host = cfg.public_host.trim().to_string();
    cfg.owner = cfg.owner.trim().to_string();
    Ok(cfg)
}

fn default_file(key: &str, secret: &str) -> String {
    format!(
        r#"# concord-server configuration. Restart the server after editing.

server_name = "concord"
# Shared password for logging in. Empty = anyone who can reach the server may join.
server_password = ""
# Where the server listens. "[::]:7890" = every network, IPv4 and IPv6 (friends with IPv6 can then
# join without port forwarding, even behind CGNAT). "0.0.0.0:7890" means the same; add
# `ipv6 = false` to listen on IPv4 only, as before v1.0.2.
bind = "[::]:7890"
# Host/IP that clients use to reach LiveKit: your public IP or DDNS name.
public_host = "127.0.0.1"
# The server owner ("Dono") can do everything, including making Admins. You normally do not need
# this: whoever hosts the server becomes the owner by logging in from this very PC (address
# 127.0.0.1), and the console window also shows a one-time code to claim it from anywhere.
# Writing `owner = "your-name"` here instead pins the owner by name (it can then no longer be
# claimed or transferred in the app). Either way, the owner's powers stay off until the app has
# asked for, and got, a personal password. Lost it? Run
#   concord-server --reset-password <name>
# and log in with the one-time code it prints. To forget the owner and start over:
#   concord-server --reset-owner
# owner = "seu_nome"

# Images in the chat. The app compresses them before sending (a photo is usually 200-500 KB).
# Total space for images, in GB; when it is full new images are refused.
max_attachment_storage_gb = 5
# What each person may send per day, in MB.
attachment_daily_mb = 200
# Delete images older than this many days (0 = keep forever). With it set, a full storage also
# makes room by deleting the oldest images.
attachment_retention_days = 0

# Protected connection (TLS, concord 1.1+). "auto": apps 1.1 and newer connect protected, older
# apps still connect without protection. "required": only protected connections (older apps are
# asked to update; this PC itself may still connect without). "off": no protection (not
# recommended). The certificate lives in the tls folder next to this file: keep it when moving
# the server, or everybody will need a new invite.
tls = "auto"

[livekit]
# Generated on first run. Keep secret.
api_key = "API{key}"
api_secret = "{secret}"
signal_port = 7880
rtc_tcp_port = 7881
rtc_udp_port = 7882
# Set to true so LiveKit advertises your public IP (needed for friends outside your LAN).
use_external_ip = false
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listen::{plan, Plan};

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn tls_is_auto_unless_set() {
        assert_eq!(parse(&default_file("k", &"s".repeat(32))).tls, crate::tls::TlsMode::Auto);
        assert_eq!(parse("server_name = \"x\"").tls, crate::tls::TlsMode::Auto, "a config from before 1.1");
        assert_eq!(parse("tls = \"required\"").tls, crate::tls::TlsMode::Required);
        assert_eq!(parse("tls = \"off\"").tls, crate::tls::TlsMode::Off);
        assert!(toml::from_str::<Config>("tls = \"maybe\"").is_err());
    }

    #[test]
    fn new_configs_listen_on_ipv4_and_ipv6() {
        let cfg = parse(&default_file("k", &"s".repeat(32)));
        assert_eq!((cfg.bind.as_str(), cfg.ipv6), ("[::]:7890", true));
        assert_eq!(plan(&cfg.bind, cfg.ipv6), Plan::DualStack(7890));
        let cfg = Config::default();
        assert_eq!(plan(&cfg.bind, cfg.ipv6), Plan::DualStack(7890));
    }

    #[test]
    fn old_configs_get_ipv6_unless_they_opt_out() {
        // A v1.0.1 config: `0.0.0.0` and no `ipv6` key.
        let cfg = parse(r#"bind = "0.0.0.0:7890"
[livekit]
api_key = "k""#);
        assert!(cfg.ipv6);
        assert_eq!(plan(&cfg.bind, cfg.ipv6), Plan::DualStack(7890));
        // The opt-out: IPv4 only, exactly as before.
        let cfg = parse(r#"bind = "0.0.0.0:7890"
ipv6 = false"#);
        assert_eq!(plan(&cfg.bind, cfg.ipv6), Plan::Exact("0.0.0.0:7890".parse().unwrap()));
        let cfg = parse(r#"bind = "[::]:7000"
ipv6 = false"#);
        assert_eq!(plan(&cfg.bind, cfg.ipv6), Plan::Exact("0.0.0.0:7000".parse().unwrap()));
        // An explicit address stays exactly that (an IPv4 one stays IPv4-only).
        for (bind, want) in [("192.0.2.10:7890", "192.0.2.10:7890"), ("127.0.0.1:7997", "127.0.0.1:7997"), ("[2001:db8::1]:7890", "[2001:db8::1]:7890"), (" [::1]:0 ", "[::1]:0")] {
            assert_eq!(plan(bind, true), Plan::Exact(want.parse().unwrap()), "{bind}");
        }
        // A host name is left to the system, as before.
        assert_eq!(plan("localhost:7890", true), Plan::Named("localhost:7890".into()));
    }
}
