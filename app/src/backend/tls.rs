//! v1.1 (T0): which transport a login uses, from what is known about the server (its pin) and
//! what answers at the address ([`vox_client_core::tls::probe`]). The trust rules of
//! `docs/PLANO-v1.1-E2EE.md`:
//!
//! 1. An invite with a pin: pinned TLS only, never plain HTTP.
//! 2. A saved entry with a pin: pinned TLS only. No TLS there means "servidor antigo /
//!    desligado"; another certificate means the hard "A identidade do servidor mudou" dialog.
//! 3. A saved entry without a pin (v1.0), or an invite without one (an older hub): TLS when the
//!    hub speaks it (its certificate is pinned after a login that confirms the server), else plain
//!    HTTP, silently, with the open lock.
//! 4. A new address typed by hand: TLS when the hub speaks it (pinned on login); else ask once
//!    before going plain.
//! 5. Once there is a pin, never plain HTTP.
//!
//! [`decide`] is the whole table, pure; [`open`] runs it against an address.

use std::sync::Arc;

use tracing::{info, warn};
use vox_client_core::hub::HubClient;
use vox_client_core::tls::{Pin, Probe};

/// Where a login's address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinSource {
    /// Typed in "Servidor ou convite" (rule 4).
    #[default]
    Manual,
    /// A saved server (or this computer's saved login token for it) (rules 2 and 3).
    Saved,
    /// An invite code (rules 1 and 3).
    Invite,
}

/// What a login knows about the server's certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsPlan {
    /// The certificate it must be (an invite's, a saved entry's or saved login token's).
    pub pin: Option<Pin>,
    pub source: PinSource,
    /// The user already said "Conectar sem proteção" for this address (rule 4, asked once).
    pub plain_ok: bool,
}

/// The transport a login goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Pinned TLS. `learned`: the pin was seen now, on first use (save it once the login confirms
    /// the server).
    Tls { pin: Pin, learned: bool },
    /// Plain HTTP (an older hub): the open lock.
    Plain,
}

/// The verdict for one address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Use(Transport),
    /// Rules 1, 2: TLS answers with another certificate. Nothing is sent.
    Mismatch { expected: Pin, got: Pin },
    /// Rules 1, 2, 5: a pinned server that does not speak TLS there (an older or stopped server,
    /// or something else at the address). Nothing is sent.
    NoTlsForPinned,
    /// Rule 4: a typed address without TLS: ask before plain HTTP.
    AskPlain,
    /// Nothing answers.
    Unreachable(String),
}

/// The trust rules (see the module docs). Pure.
pub fn decide(plan: &TlsPlan, probe: &Probe) -> Decision {
    match (plan.pin, probe) {
        (_, Probe::Unreachable(why)) => Decision::Unreachable(why.clone()),
        (Some(want), Probe::Tls(got)) if want == *got => Decision::Use(Transport::Tls { pin: want, learned: false }),
        (Some(want), Probe::Tls(got)) => Decision::Mismatch { expected: want, got: *got },
        (Some(_), Probe::NoTls) => Decision::NoTlsForPinned,
        (None, Probe::Tls(got)) => Decision::Use(Transport::Tls { pin: *got, learned: true }),
        (None, Probe::NoTls) if plan.source != PinSource::Manual || plan.plain_ok => Decision::Use(Transport::Plain),
        (None, Probe::NoTls) => Decision::AskPlain,
    }
}

/// [`decide`] for `url`, after probing it.
pub async fn decide_at(url: &str, plan: &TlsPlan) -> Decision {
    let probe = vox_client_core::tls::probe_url(url).await;
    let d = decide(plan, &probe);
    info!(%url, pinned = plan.pin.is_some(), source = ?plan.source, probe = probe_name(&probe), decision = decision_name(&d), "tls: transport chosen");
    if let Decision::Mismatch { expected, got } = &d {
        warn!(%url, saved = %vox_proto::tls_server_code(expected), answered = %vox_proto::tls_server_code(got), "tls: the server's certificate is not the pinned one: nothing sent");
    }
    d
}

/// A client for `url` on `transport`.
pub fn client(url: &str, transport: Transport) -> anyhow::Result<Arc<HubClient>> {
    Ok(Arc::new(match transport {
        Transport::Tls { pin, .. } => HubClient::new_pinned(url, pin)?,
        Transport::Plain => HubClient::new(url)?,
    }))
}

fn probe_name(p: &Probe) -> &'static str {
    match p {
        Probe::Tls(_) => "tls",
        Probe::NoTls => "no-tls",
        Probe::Unreachable(_) => "unreachable",
    }
}

fn decision_name(d: &Decision) -> &'static str {
    match d {
        Decision::Use(Transport::Tls { learned: false, .. }) => "pinned-tls",
        Decision::Use(Transport::Tls { learned: true, .. }) => "tls-first-use",
        Decision::Use(Transport::Plain) => "plain",
        Decision::Mismatch { .. } => "MISMATCH",
        Decision::NoTlsForPinned => "no-tls-for-a-pinned-server",
        Decision::AskPlain => "ask-before-plain",
        Decision::Unreachable(_) => "unreachable",
    }
}

/// Rule 2's message: a server with a pin that does not answer in TLS.
pub const NO_TLS_FOR_PINNED: &str = "Não foi possível abrir a conexão protegida com este servidor: ele pode estar desligado, ou quem hospeda voltou \
     para uma versão antiga do concord. Por segurança, o concord não conecta sem proteção a um servidor que já usou conexão protegida. \
     Se o servidor voltou mesmo para uma versão antiga, tire-o da lista (\"Remover da lista\") e adicione de novo.";

/// The plain-HTTP prompt (rule 4).
pub const PLAIN_PROMPT: &str = "Este servidor não tem conexão protegida. Senhas e mensagens passam pela rede sem criptografia. Conectar mesmo assim?";

#[cfg(test)]
mod tests {
    use super::*;

    const P: Pin = [7u8; 32];
    const Q: Pin = [9u8; 32];

    fn plan(pin: Option<Pin>, source: PinSource, plain_ok: bool) -> TlsPlan {
        TlsPlan { pin, source, plain_ok }
    }

    /// The trust-rule table.
    #[test]
    fn the_trust_rules() {
        use Decision::*;
        use PinSource::*;
        let tls = |p| Use(Transport::Tls { pin: p, learned: false });
        let tofu = |p| Use(Transport::Tls { pin: p, learned: true });
        let down = Probe::Unreachable("refused".into());
        for source in [Manual, Saved, Invite] {
            for plain_ok in [false, true] {
                let with = plan(Some(P), source, plain_ok);
                // Rules 1, 2, 5: a pin means that certificate over TLS, nothing else, ever.
                assert_eq!(decide(&with, &Probe::Tls(P)), tls(P), "{source:?}");
                assert_eq!(decide(&with, &Probe::Tls(Q)), Mismatch { expected: P, got: Q }, "{source:?}");
                assert_eq!(decide(&with, &Probe::NoTls), NoTlsForPinned, "{source:?} {plain_ok}: never plain with a pin");
                assert_eq!(decide(&with, &down), Unreachable("refused".into()));
                // Rules 3, 4: without a pin, TLS when it is there (first use).
                let without = plan(None, source, plain_ok);
                assert_eq!(decide(&without, &Probe::Tls(Q)), tofu(Q));
                assert_eq!(decide(&without, &down), Unreachable("refused".into()));
            }
        }
        // Rule 3: a saved v1.0 entry or a pinless invite (an older hub): plain, silently.
        assert_eq!(decide(&plan(None, Saved, false), &Probe::NoTls), Use(Transport::Plain));
        assert_eq!(decide(&plan(None, Invite, false), &Probe::NoTls), Use(Transport::Plain));
        // Rule 4: a typed address: ask once, then plain.
        assert_eq!(decide(&plan(None, Manual, false), &Probe::NoTls), AskPlain);
        assert_eq!(decide(&plan(None, Manual, true), &Probe::NoTls), Use(Transport::Plain));
    }

    #[test]
    fn the_messages_are_the_plans() {
        assert_eq!(PLAIN_PROMPT, "Este servidor não tem conexão protegida. Senhas e mensagens passam pela rede sem criptografia. Conectar mesmo assim?");
        assert!(NO_TLS_FOR_PINNED.contains("desligado") && NO_TLS_FOR_PINNED.contains("versão antiga"));
    }
}
