//! Go Live: publish a window/monitor (or test source) and watch someone else's share.
//!
//! Watching is driven by [`crate::voice::VoiceSession::watch`]; this module holds the pieces.

#[cfg(windows)]
pub mod d3d;
pub mod camera;
pub mod encoders;
pub mod h264;
#[cfg(windows)]
pub mod host;
pub mod pattern;
pub mod publish;
pub mod share_preview;
pub mod thumbnail;
pub mod viewer;
#[cfg(windows)]
mod viewer_native;
#[cfg(target_os = "macos")]
pub mod viewer_tex;

pub use publish::{audio_failure_pt, AUDIO_ONLY_HIGH_BPS, AUDIO_ONLY_NORMAL_BPS, AUDIO_ONLY_TRACK_NAME, capture_call, capture_failure_pt, screenshare_bitrate, ActiveShare, SwitchReport, SwitchRequest, DegradationPreference, EncoderChoice, H264FileSource, ShareOptions, SharePublication, ShareSource, StartError, VideoPath};
pub use share_preview::{SharePreview, SELF_SHARE_KEY};
pub use viewer::{StreamState, TileStyle, Viewer, ViewerNotice, ViewerStream};

