//! Audio plumbing shared by voice and Go Live: device I/O, playback mixer, helpers.

pub mod alerts;
pub mod denoise;
pub mod dfn;
#[cfg(test)]
mod denoise_eval;
pub mod device;
pub mod fake_mic;
pub mod gate;
mod mic_route;
pub mod mic_test;
pub mod mixer;
pub mod prefs;
pub mod resample;
pub mod sine;
pub mod speaking;
pub mod ui_player;
pub mod voicefx;
#[cfg(test)]
mod voicefx_eval;

pub use mixer::{Gain, Mixer, SoundTag, SourceCtl, SourceId, MAX_GAIN, MIX_10MS, MIX_CHANNELS, MIX_RATE, NO_TAG};
pub use alerts::AlertKind;
pub use prefs::{AudioPrefs, UserAudio};
pub use denoise::{NoiseControl, NoiseLevel};
pub use gate::{GateConfig, GateMode, VoiceGate};
pub use mic_route::MicStatus;
pub use mic_test::{MicTest, MicTestConfig};
pub use speaking::Speakers;
pub use ui_player::UiPlayer;
pub use voicefx::{VoiceEffect, VoiceFxConfig, VoiceFxControl};
