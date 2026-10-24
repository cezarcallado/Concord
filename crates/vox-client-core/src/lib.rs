//! UI-agnostic client core. The future desktop UI is a thin shell over this crate.
//! Implemented by the client-core agent; see docs/PLANO.md.
//!
//! - [`hub`]: HTTP API + reconnecting event WebSocket (`vox-proto`).
//! - [`images`]: compression of chat images before they are sent.
//! - [`voice`]: LiveKit room = voice channel; mic publish, playback mixer, mute/deafen,
//!   active speakers, Go Live publish/watch entry points.
//! - [`golive`]: screen/window share publishing (vox-capture or test sources) and the viewer.
//! - [`stats`]: periodic per-stream and process CPU/RAM statistics.
//! - [`token`]: local LiveKit token minting for direct (hub-less) testing.

pub mod audio;
pub mod golive;
pub mod hub;
pub mod images;
pub mod media_e2ee;
pub mod soundboard;
pub mod stats;
pub mod testmode;
pub mod tls;
pub mod token;
pub mod voice;

pub use golive::{ShareOptions, ShareSource};
pub use hub::{HubClient, HubEvent, HubSender};
pub use audio::{AlertKind, GateConfig, GateMode, MicTest, MicTestConfig, NoiseControl, NoiseLevel, UiPlayer, UserAudio, VoiceEffect, VoiceFxConfig, VoiceGate};
pub use media_e2ee::{MediaDirectory, MediaE2ee, MediaE2eeState};
pub use voice::{JoinError, JoinReport, JoinStep, ShareSnapshots, VoiceConfig, VoiceEvent, VoiceSession, WatchLimit, MAX_WATCHED};
