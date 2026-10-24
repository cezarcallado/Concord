//! Go Live hover pictures, both sides (v1.1, see `vox_proto::stream_preview`).
//!
//! Streaming: shortly after the live starts (and after a source switch, or a reconnect to the
//! hub), a task takes one frame from the running capture's preview tap
//! (`ShareSnapshots::take`, no second capture), encodes a ~320 px JPEG on a low-priority worker
//! and sends it with what is streamed (the program's name, whether it is a game, its 32 px icon).
//! Then every [`EVERY`] it takes another frame and sends it only when the picture changed (an 8 x 8
//! luma signature). Only to a hub whose `Ready` announced the feature, and only while the
//! setting "Prévia da minha transmissão ao passar o mouse" is on (`Cmd::StreamPreviewUpload`;
//! turning it off mid-live drops the hub's copy). `VOX_NO_STREAM_PREVIEW=1` turns it off too (A/B
//! measurements).
//!
//! Viewing: `Cmd::StreamPreview` asks the hub for someone's preview (the UI asks only after the
//! pointer rested on a live, see `stream_cards.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use vox_client_core::golive::thumbnail::{self, Signature};
use vox_client_core::golive::ShareSource;
use vox_client_core::hub::{HubClient, UploadReply};
use vox_client_core::ShareSnapshots;
use vox_proto::{StreamApp, UserId};

use super::{env_flag, Controller, Ev, Out};
use crate::stream_cards::CardEv;

/// The first picture this long after the live starts (the capture's first frames are in by then).
const FIRST_AFTER: Duration = Duration::from_millis(1500);
/// Then one look every this long (a picture is sent only when it changed).
const EVERY: Duration = Duration::from_secs(17);
/// How long a snapshot may take to arrive.
const SNAPSHOT_WAIT: Duration = Duration::from_millis(1500);

/// What is streamed, as sent with the picture.
#[derive(Debug, Clone)]
struct ShareApp {
    app: StreamApp,
    icon_png: Option<Vec<u8>>,
}

#[derive(Default)]
pub(super) struct ThumbState {
    /// The hub keeps hover previews (its `Ready` said so).
    pub supported: bool,
    /// The person allows the upload (the setting; nothing is sent until the app said so).
    allowed: bool,
    /// What the running live shows (to start again after a reconnect).
    source: Option<ShareSource>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// What `source` shows (blocking: reads the exe's version resource and icon). `None`: nothing to
/// say (test sources, audio-only lives).
fn share_app(source: &ShareSource) -> Option<ShareApp> {
    match source {
        ShareSource::Window(hwnd) => {
            let info = vox_capture::window_app(*hwnd)?;
            let icon_png = vox_capture::window_icon(*hwnd, thumbnail::ICON_SIDE).ok().and_then(|i| thumbnail::icon_png(&i)).filter(|p| p.len() <= vox_proto::STREAM_PREVIEW_MAX_ICON);
            Some(ShareApp { app: StreamApp { name: StreamApp::clean_name(&info.name), is_game: info.is_game, screen: false }, icon_png })
        }
        ShareSource::Monitor(_) => Some(ShareApp { app: StreamApp { name: "Tela inteira".into(), is_game: false, screen: true }, icon_png: None }),
        _ => None,
    }
}

impl Controller {
    /// A hub snapshot arrived: does it keep previews? A live going on gets its picture (again).
    pub(super) fn thumbs_on_ready(&mut self, supported: bool) {
        self.thumbs.supported = supported;
        self.out.send(Ev::StreamCard(CardEv::Supported(supported)));
        if self.streaming && !self.share_audio_only && self.thumbs.source.is_some() {
            self.thumbs_spawn(FIRST_AFTER * 2);
        } else {
            self.thumbs_stop();
        }
    }

    /// The live started, or now shows `source` (a switch).
    pub(super) fn thumbs_start(&mut self, source: &ShareSource) {
        if source.is_audio_only() {
            self.thumbs.source = None;
            self.thumbs_stop();
            return;
        }
        self.thumbs.source = Some(source.clone());
        self.thumbs_spawn(FIRST_AFTER);
    }

    /// The live ended (or voice / the hub went away).
    pub(super) fn thumbs_stop(&mut self) {
        if let Some(t) = self.thumbs.task.take() {
            t.abort();
        }
    }

    /// The setting changed (or the app started): off stops the uploads and drops the hub's
    /// picture of a live going on; on starts them for a live going on.
    pub(super) fn thumbs_allow(&mut self, on: bool) {
        if self.thumbs.allowed == on {
            return;
        }
        self.thumbs.allowed = on;
        info!(on, "stream preview uploads");
        let live = self.streaming && !self.share_audio_only && self.thumbs.source.is_some();
        if on {
            if live {
                self.thumbs_spawn(FIRST_AFTER);
            }
            return;
        }
        let task = self.thumbs.task.take();
        let (Some(hub), true) = (self.hub.clone(), self.thumbs.supported) else {
            if let Some(t) = task {
                t.abort();
            }
            return;
        };
        tokio::spawn(async move {
            // The loop is gone before the hub is told, so no new upload starts; the second ask
            // covers one that was already on the wire.
            if let Some(t) = task {
                t.abort();
                let _ = t.await;
            }
            for wait in [Duration::ZERO, Duration::from_secs(2)] {
                tokio::time::sleep(wait).await;
                if let Err(e) = hub.delete_stream_preview().await {
                    warn!(error = %format!("{e:#}"), "stream preview: could not drop the hub's copy");
                }
            }
        });
    }

    fn thumbs_spawn(&mut self, first: Duration) {
        self.thumbs_stop();
        if !self.thumbs.supported || !self.thumbs.allowed || env_flag("VOX_NO_STREAM_PREVIEW") {
            return;
        }
        let (Some(hub), Some(v), Some(source)) = (self.hub.clone(), self.voice.as_ref(), self.thumbs.source.clone()) else { return };
        // v1.1 (E1): a live in an end-to-end encrypted room never leaves a picture with the hub;
        // the hover card shows only the name and icon ("Prévia desligada: sala criptografada").
        let pictures = !v.is_e2ee();
        self.thumbs.task = Some(tokio::spawn(run(hub, v.share_snapshots(), source, first, pictures)));
    }

    /// Someone's preview, for the hover card (answer: `Ev::StreamCard`).
    pub(super) fn stream_card_fetch(&self, user: UserId, have_icon: Option<String>) {
        let out: Out = self.out.clone();
        let Some(hub) = self.hub.clone() else {
            out.send(Ev::StreamCard(CardEv::Reply { user, reply: vox_client_core::hub::StreamPreviewReply::Failed("offline".into()) }));
            return;
        };
        tokio::spawn(async move {
            let reply = hub.stream_preview(user, have_icon.as_deref()).await;
            out.send(Ev::StreamCard(CardEv::Reply { user, reply }));
        });
    }
}

/// The streamer's loop (aborted when the live ends, switches or the hub reconnects). Without
/// `pictures` (an encrypted room) it only sends what is streamed, once, and never takes a frame.
async fn run(hub: Arc<HubClient>, snaps: ShareSnapshots, source: ShareSource, first: Duration, pictures: bool) {
    let src = source.clone();
    let mut app = tokio::task::spawn_blocking(move || share_app(&src)).await.ok().flatten();
    if let Some(a) = &app {
        info!(game = a.app.is_game, screen = a.app.screen, icon_bytes = a.icon_png.as_ref().map_or(0, Vec::len), pictures, "stream preview: what is streamed");
    }
    if !pictures {
        info!("stream preview: end-to-end encrypted room, name and icon only (no picture)");
        let mut wait = first;
        while let Some(a) = app.as_ref() {
            tokio::time::sleep(wait).await;
            match hub.upload_stream_preview(None, Some((&a.app, a.icon_png.as_deref()))).await {
                UploadReply::Stored => {
                    info!("stream preview sent (name and icon, no picture)");
                    app = None;
                }
                UploadReply::TooSoon | UploadReply::NotLive => wait = Duration::from_secs(4),
                UploadReply::Unsupported => return,
                UploadReply::Refused(why) => {
                    warn!(%why, "stream preview refused");
                    return;
                }
            }
        }
        return;
    }
    let mut last: Option<Signature> = None;
    let mut wait = first;
    let (mut sent, mut skipped, mut encode_us_total) = (0u64, 0u64, 0u64);
    loop {
        tokio::time::sleep(wait).await;
        wait = EVERY;
        let t = Instant::now();
        let Some(frame) = snaps.take(SNAPSHOT_WAIT).await else {
            debug!("stream preview: no frame from the capture this time");
            if let Some(a) = app.as_ref() {
                // The name and icon can go on their own.
                if hub.upload_stream_preview(None, Some((&a.app, a.icon_png.as_deref()))).await == UploadReply::Stored {
                    app = None;
                }
            }
            continue;
        };
        let snapshot_ms = t.elapsed().as_millis() as u64;
        let (fw, fh) = (frame.width, frame.height);
        let started = Instant::now();
        let Ok(Some(thumb)) = tokio::task::spawn_blocking(move || thumbnail::with_low_priority(|| thumbnail::encode(&frame))).await else {
            warn!("stream preview: encoding failed");
            continue;
        };
        let encode_us = started.elapsed().as_micros() as u64;
        encode_us_total += encode_us;
        let changed = last.is_none_or(|l| l.differs(&thumb.sig));
        if !changed && app.is_none() {
            skipped += 1;
            debug!(skipped, "stream preview: picture unchanged, not sent");
            continue;
        }
        let jpeg = changed.then_some(&thumb.jpeg[..]);
        match hub.upload_stream_preview(jpeg, app.as_ref().map(|a| (&a.app, a.icon_png.as_deref()))).await {
            UploadReply::Stored => {
                sent += 1;
                if changed {
                    last = Some(thumb.sig);
                }
                info!(
                    bytes = if changed { thumb.jpeg.len() } else { 0 },
                    w = thumb.width,
                    h = thumb.height,
                    from = %format!("{fw}x{fh}"),
                    encode_us,
                    snapshot_ms,
                    with_app = app.is_some(),
                    sent,
                    skipped,
                    encode_ms_total = encode_us_total / 1000,
                    "stream preview sent"
                );
                app = None;
            }
            UploadReply::TooSoon | UploadReply::NotLive => wait = Duration::from_secs(4),
            UploadReply::Unsupported => {
                info!("stream preview: the hub has no such route; stopping");
                return;
            }
            UploadReply::Refused(why) => warn!(%why, "stream preview refused"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_and_test_sources() {
        let s = share_app(&ShareSource::Monitor(0)).unwrap();
        assert_eq!((s.app.name.as_str(), s.app.screen, s.app.is_game), ("Tela inteira", true, false));
        assert!(s.icon_png.is_none());
        assert!(share_app(&ShareSource::TestPattern).is_none());
        assert!(share_app(&ShareSource::AudioSystem).is_none());
        // A window that does not exist: nothing (never a guess).
        assert!(share_app(&ShareSource::Window(0)).is_none());
    }
}
