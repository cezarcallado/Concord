//! End-to-end encryption, phase E0 (v1.1, no protocol bump): the directory of device keys.
//!
//! Every device of every member has an X25519 identity key (made and kept by the app, see the
//! `vox-e2ee` crate; the hub never links any of that crypto). The hub only keeps the public halves:
//!
//! - `GET /api/e2ee/devices` (authenticated): every member's devices, `Vec<`[`E2eeDevice`]`>`.
//! - `PUT /api/e2ee/devices/{device_id}` (authenticated) with [`E2eeDeviceRegister`]: register this
//!   device for the caller (or say it is still here: same key = `200`, `last_seen_ms` refreshed, the
//!   label may change). Answers the [`E2eeDevice`]. `400` a bad device id ([`is_e2ee_device_id`]),
//!   key ([`E2EE_KEY_B64_LEN`] characters of base64url for 32 bytes) or label
//!   ([`clean_e2ee_label`]); `409` [`ERR_E2EE_KEY_CHANGED`] when the id is registered with another
//!   key (a registered (id, key) pair never changes: a reinstall that reused the id picks a new
//!   id), `409` [`ERR_E2EE_TOO_MANY_DEVICES`] past [`E2EE_MAX_DEVICES`]; `429` too many changes.
//! - `DELETE /api/e2ee/devices/{device_id}` (authenticated): remove one of MY devices. `200`
//!   [`crate::OkResponse`], `404` when I have no such device (somebody else's device is never
//!   touched), `429` too many changes.
//! - Every change goes to everybody as [`crate::ServerEvent::E2eeDevicesChanged`] (the full new
//!   list of that member's devices), so every app sees a new device of anybody at once ("um aparelho
//!   novo foi adicionado à sua conta").
//! - Compatibility: a hub with the directory sets [`crate::ServerEvent::Ready`]'s `e2ee`; an app
//!   registers and asks only then (an older hub would answer `404`). Older apps ignore the flag and
//!   log and skip the event.

use serde::{Deserialize, Serialize};

use crate::UserId;

/// Most devices one member may register.
pub const E2EE_MAX_DEVICES: usize = 5;
/// Most characters of a device's label (the computer's name).
pub const E2EE_LABEL_MAX: usize = 40;
/// Characters of a public key on the wire: 32 bytes of base64url without padding.
pub const E2EE_KEY_B64_LEN: usize = 43;
/// `409` of `PUT /api/e2ee/devices/{id}`: that id is registered with another key.
pub const ERR_E2EE_KEY_CHANGED: &str = "device registered with another key";
/// `409` of `PUT /api/e2ee/devices/{id}`: the member already has [`E2EE_MAX_DEVICES`] devices.
pub const ERR_E2EE_TOO_MANY_DEVICES: &str = "too many devices";

/// One device in the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eeDevice {
    pub user_id: UserId,
    /// 16 random bytes, 32 lowercase hex digits ([`is_e2ee_device_id`]).
    pub device_id: String,
    /// The X25519 public key, base64url without padding (32 bytes).
    pub public_key: String,
    /// The computer's name, as the app sent it ([`clean_e2ee_label`]).
    pub label: String,
    pub created_at_ms: i64,
}

/// `PUT /api/e2ee/devices/{device_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eeDeviceRegister {
    pub public_key: String,
    pub label: String,
}

/// A device id on the wire: exactly 32 lowercase hex digits.
pub fn is_e2ee_device_id(s: &str) -> bool {
    crate::is_server_id(s)
}

/// A device label as kept: control characters and the invisible / direction-changing ones
/// ([`crate::is_forbidden_char`]: the label is shown in the "aparelho novo" alert, so it must not
/// disguise itself) removed, trimmed. `None` when nothing is left or it is longer than
/// [`E2EE_LABEL_MAX`] characters.
pub fn clean_e2ee_label(label: &str) -> Option<String> {
    let s: String = label.chars().filter(|c| !crate::is_forbidden_char(*c)).collect();
    let s = s.trim();
    (!s.is_empty() && s.chars().count() <= E2EE_LABEL_MAX).then(|| s.to_string())
}

/// A computer name cut to fit a label (what the app sends): the characters [`clean_e2ee_label`]
/// drops removed, trimmed, at most [`E2EE_LABEL_MAX`] characters, "Computador" when nothing is
/// left.
pub fn fit_e2ee_label(name: &str) -> String {
    let s: String = name.chars().filter(|c| !crate::is_forbidden_char(*c)).collect();
    let s: String = s.trim().chars().take(E2EE_LABEL_MAX).collect();
    let s = s.trim();
    if s.is_empty() { "Computador".to_string() } else { s.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ServerEvent, User};

    /// `Ready` and the events exactly as v1.1 before E0 had them (what an older app parses).
    mod pre_e0 {
        use crate::{Account, Channel, ChatFeatures, User, UserId, VoiceMember};
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        pub enum ServerEvent {
            Ready {
                user: User,
                channels: Vec<Channel>,
                online: Vec<User>,
                voice: Vec<VoiceMember>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                account: Option<Account>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                chat: Option<ChatFeatures>,
                #[serde(default, skip_serializing_if = "Option::is_none")]
                server_id: Option<String>,
                #[serde(default, skip_serializing_if = "std::ops::Not::not")]
                stream_previews: bool,
            },
            UserOffline {
                user_id: UserId,
            },
        }
    }

    fn ready(e2ee: bool) -> ServerEvent {
        ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: None, server_id: Some("00112233445566778899aabbccddeeff".into()), stream_previews: true, e2ee }
    }

    fn device() -> E2eeDevice {
        E2eeDevice { user_id: 3, device_id: "00112233445566778899aabbccddeeff".into(), public_key: "A".repeat(43), label: "PC da sala".into(), created_at_ms: 1_700_000_000_000 }
    }

    #[test]
    fn the_capability_is_additive_both_ways() {
        // A new hub's snapshot reads in an older app (which ignores the flag).
        let json = serde_json::to_string(&ready(true)).unwrap();
        assert!(json.contains(r#""e2ee":true"#), "{json}");
        assert!(matches!(serde_json::from_str::<pre_e0::ServerEvent>(&json), Ok(pre_e0::ServerEvent::Ready { stream_previews: true, .. })));
        // Off: byte-identical to before.
        let old = pre_e0::ServerEvent::Ready { user: User::new(1, "a"), channels: vec![], online: vec![], voice: vec![], account: None, chat: None, server_id: Some("00112233445566778899aabbccddeeff".into()), stream_previews: true };
        assert_eq!(serde_json::to_string(&ready(false)).unwrap(), serde_json::to_string(&old).unwrap());
        // An older hub's snapshot: no directory.
        let parsed: ServerEvent = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert!(matches!(parsed, ServerEvent::Ready { e2ee: false, .. }));
    }

    #[test]
    fn the_event_is_new_and_old_apps_skip_it() {
        let ev = ServerEvent::E2eeDevicesChanged { user_id: 3, devices: vec![device()] };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.starts_with(r#"{"type":"e2ee_devices_changed","user_id":3,"devices":[{"user_id":3,"device_id":"00112233445566778899aabbccddeeff""#), "{json}");
        match serde_json::from_str::<ServerEvent>(&json).unwrap() {
            ServerEvent::E2eeDevicesChanged { user_id, devices } => assert_eq!((user_id, devices), (3, vec![device()])),
            other => panic!("{other:?}"),
        }
        // An older app does not know the variant: an error it logs and skips (its event loop goes
        // on with the next frame, `lib.rs` of the client); the events it knows still parse.
        assert!(serde_json::from_str::<pre_e0::ServerEvent>(&json).is_err());
        assert!(serde_json::from_str::<pre_e0::ServerEvent>(r#"{"type":"user_offline","user_id":3}"#).is_ok());
        // Nobody left: an empty list.
        let none = serde_json::to_string(&ServerEvent::E2eeDevicesChanged { user_id: 3, devices: vec![] }).unwrap();
        assert_eq!(none, r#"{"type":"e2ee_devices_changed","user_id":3,"devices":[]}"#);
    }

    #[test]
    fn wire_shapes() {
        let reg = E2eeDeviceRegister { public_key: "k".into(), label: "PC".into() };
        assert_eq!(serde_json::to_string(&reg).unwrap(), r#"{"public_key":"k","label":"PC"}"#);
        let d: E2eeDevice = serde_json::from_str(&serde_json::to_string(&device()).unwrap()).unwrap();
        assert_eq!(d, device());
        assert!(is_e2ee_device_id("00112233445566778899aabbccddeeff"));
        assert!(!is_e2ee_device_id("00112233445566778899AABBCCDDEEFF"));
        assert!(!is_e2ee_device_id("0011"));
    }

    #[test]
    fn labels_are_cleaned_and_capped() {
        assert_eq!(clean_e2ee_label("  PC\u{7} da\nsala  ").as_deref(), Some("PC dasala"));
        assert_eq!(clean_e2ee_label(&"x".repeat(E2EE_LABEL_MAX)).map(|s| s.len()), Some(E2EE_LABEL_MAX));
        assert_eq!(clean_e2ee_label(&"x".repeat(E2EE_LABEL_MAX + 1)), None);
        assert_eq!(clean_e2ee_label(&format!("  {}  ", "é".repeat(E2EE_LABEL_MAX))).map(|s| s.chars().count()), Some(E2EE_LABEL_MAX), "characters, not bytes, after trimming");
        assert_eq!(clean_e2ee_label(" \t\u{1b} "), None);
        // Invisible and direction-changing characters cannot disguise a label in the alert.
        assert_eq!(clean_e2ee_label("PC\u{202E}gpj.exe\u{200B}").as_deref(), Some("PCgpj.exe"));
        assert_eq!(clean_e2ee_label("\u{202E}\u{FEFF}\u{2066}"), None);
        assert_eq!(fit_e2ee_label("\u{202E}"), "Computador");
        assert_eq!(fit_e2ee_label(&"y".repeat(100)).chars().count(), E2EE_LABEL_MAX);
        assert_eq!(fit_e2ee_label("\u{0}\u{1}"), "Computador");
        assert_eq!(fit_e2ee_label(" DESKTOP-1A2B3C "), "DESKTOP-1A2B3C");
        for s in ["DESKTOP-1A2B3C", &"z".repeat(300), "\n\n", "  a  "] {
            assert!(clean_e2ee_label(&fit_e2ee_label(s)).is_some(), "what the app sends is always valid: {s:?}");
        }
    }
}
