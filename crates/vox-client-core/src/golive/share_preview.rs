//! The sharer's own preview (v0.3, item 8a): while we are live, our stream tile in the call view
//! shows a small, low-rate picture of what we share, so we can check that the right window is on.
//!
//! The picture comes from the capture itself (`vox_capture::ScreenPreview`: a GPU downscale of the
//! frame the encoder just got, a small readback, at most a few frames a second); our own encoded
//! stream is never decoded and our own screen-share audio is never subscribed (we are not a
//! remote participant of our own room). Frames go to a local viewer tile ([`SELF_SHARE_KEY`]) that
//! the app places in the call view.
//!
//! A source switch ("Trocar tela") runs a second capture next to the first for a moment; only the
//! capture that owns the stream shows its frames (`active` generation), the retired one is turned
//! off.
//!
//! v1.1: the same tap also gives the hover preview its still picture ([`SharePreview::request_snapshot`]):
//! one frame, copied on the capture thread and handed to whoever asked. With the tile's preview
//! off, the tap is switched on just for that frame at [`SNAPSHOT_CFG`] (a re-tap of the
//! converter's current picture, so a static screen gives one too) and off again right after; with
//! it on, the tile's next frame is taken (and the tap nudged so a static screen sends one).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use vox_capture::{FrameCallback, PreviewConfig, ScreenPreview};

use super::thumbnail::{Nv12Image, THUMB_MAX_H, THUMB_MAX_W};
use super::viewer::ViewerStream;
use crate::stats::StreamCounters;

/// How the tap runs for one snapshot while the tile's preview is off: once, at the picture's size.
pub const SNAPSHOT_CFG: PreviewConfig = PreviewConfig { on: true, fps: 1, max_width: THUMB_MAX_W, max_height: THUMB_MAX_H };

#[derive(Default)]
struct Snap {
    /// Someone waits for a frame.
    want: bool,
    frame: Option<Nv12Image>,
}

/// Viewer key of our own Go Live preview tile.
pub const SELF_SHARE_KEY: &str = "live:self";

struct State {
    sink: ViewerStream,
    counters: Arc<StreamCounters>,
    cfg: Mutex<PreviewConfig>,
    /// Generation of the capture whose frames are shown.
    active: AtomicU64,
    next: AtomicU64,
    /// The active capture's tap (snapshots switch it on and off).
    active_sp: Mutex<Option<ScreenPreview>>,
    snap: Mutex<Snap>,
}

/// Where a share's preview frames go and how the preview is set (shared by the share's captures).
#[derive(Clone)]
pub struct SharePreview(Arc<State>);

impl SharePreview {
    /// `sink` is the tile [`SELF_SHARE_KEY`]; `counters` count the frames shown.
    pub fn new(sink: ViewerStream, counters: Arc<StreamCounters>, cfg: PreviewConfig) -> Self {
        Self(Arc::new(State { sink, counters, cfg: Mutex::new(cfg), active: AtomicU64::new(0), next: AtomicU64::new(1), active_sp: Mutex::new(None), snap: Mutex::new(Snap::default()) }))
    }

    pub fn config(&self) -> PreviewConfig {
        *self.0.cfg.lock()
    }

    pub fn counters(&self) -> Arc<StreamCounters> {
        self.0.counters.clone()
    }

    pub(crate) fn set_config(&self, cfg: PreviewConfig) {
        *self.0.cfg.lock() = cfg;
    }

    /// Ask the running capture for one frame (see the module docs); [`SharePreview::take_snapshot`]
    /// hands it over once it came. Cheap: a flag and a settings change of the tap.
    pub fn request_snapshot(&self) {
        self.0.snap.lock().want = true;
        let user = self.config();
        let cfg = if user.on {
            // A different size makes the tap show the current picture again (a static screen
            // sends nothing otherwise); the frame's callback puts the tile's own size back.
            PreviewConfig { max_width: user.max_width.saturating_sub(2).max(32), ..user }
        } else {
            SNAPSHOT_CFG
        };
        if let Some(sp) = self.0.active_sp.lock().as_ref() {
            sp.set(cfg);
        }
    }

    /// The frame asked for with [`SharePreview::request_snapshot`], once it came.
    pub fn take_snapshot(&self) -> Option<Nv12Image> {
        self.0.snap.lock().frame.take()
    }

    /// Nobody waits any more (no frame came in time): the tap goes back to the tile's settings.
    pub fn cancel_snapshot(&self) {
        let mut s = self.0.snap.lock();
        s.want = false;
        s.frame = None;
        drop(s);
        self.0.restore();
    }

    /// The tap of one new capture: off until [`CaptureTap::activate`].
    pub(crate) fn attach(&self) -> Option<CaptureTap> {
        let sp = match ScreenPreview::new() {
            Ok(sp) => sp,
            Err(e) => {
                tracing::warn!(error = %e, "screen preview unavailable");
                return None;
            }
        };
        let gen = self.0.next.fetch_add(1, Ordering::Relaxed);
        Some(CaptureTap { sp, gen, owner: self.clone() })
    }
}

impl State {
    /// The active tap back to the tile's own settings.
    fn restore(&self) {
        let cfg = *self.cfg.lock();
        if let Some(sp) = self.active_sp.lock().as_ref() {
            sp.set(cfg);
        }
    }
}

/// One capture's preview tap.
pub(crate) struct CaptureTap {
    pub(crate) sp: ScreenPreview,
    gen: u64,
    owner: SharePreview,
}

impl CaptureTap {
    /// The callback for `vox_capture::start_video*_with_preview`.
    pub(crate) fn callback(&self) -> FrameCallback {
        let (st, gen) = (self.owner.0.clone(), self.gen);
        Box::new(move |f| {
            // A retired capture may still deliver a frame or two while it shuts down.
            if st.active.load(Ordering::Acquire) != gen {
                return;
            }
            let mut snap = st.snap.lock();
            // (A frame the tap made only for a snapshot, with the tile's preview off, is not shown.)
            let snapshot_only = snap.want && !st.cfg.lock().on;
            if !snapshot_only {
                st.counters.frame(f.width, f.height);
                st.sink.submit_nv12(f);
            }
            if snap.want {
                snap.want = false;
                snap.frame = Nv12Image::copy_of(f);
                drop(snap);
                st.restore();
            }
        })
    }

    /// This capture now owns the stream: show its frames, with the share's current settings.
    pub(crate) fn activate(&self) {
        self.owner.0.active.store(self.gen, Ordering::Release);
        *self.owner.0.active_sp.lock() = Some(self.sp.clone());
        self.sp.set(self.owner.config());
    }

    /// This capture was retired: no more frames, no more GPU work.
    pub(crate) fn deactivate(&self) {
        let mut c = self.sp.config();
        c.on = false;
        self.sp.set(c);
    }

    pub(crate) fn apply(&self, cfg: PreviewConfig) {
        if self.owner.0.active.load(Ordering::Acquire) == self.gen {
            self.sp.set(cfg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn frame_of(cb: &mut FrameCallback) {
        let (y, uv) = (vec![0u8; 16 * 16], vec![128u8; 16 * 8]);
        cb(&vox_capture::Nv12Frame { width: 16, height: 16, timestamp_us: 0, y: &y, y_stride: 16, uv: &uv, uv_stride: 16 });
    }

    #[test]
    fn only_the_active_capture_shows_frames() {
        let counters = Arc::new(StreamCounters::default());
        let p = SharePreview::new(ViewerStream::null(), counters.clone(), PreviewConfig::default());
        let (old, new) = (p.attach().unwrap(), p.attach().unwrap());
        let (mut cb_old, mut cb_new) = (old.callback(), new.callback());
        // Nobody owns the stream yet.
        frame_of(&mut cb_old);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 0);
        old.activate();
        frame_of(&mut cb_old);
        frame_of(&mut cb_new);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 1);
        // A source switch hands the stream over; the retired capture is quiet and switched off.
        p.set_config(PreviewConfig { on: true, fps: 15, max_width: 960, max_height: 540 });
        new.activate();
        old.deactivate();
        frame_of(&mut cb_old);
        frame_of(&mut cb_new);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 2);
        assert!(!old.sp.config().on);
        assert_eq!(new.sp.config(), p.config());
        // Settings changes reach only the capture that owns the stream.
        let cfg = PreviewConfig { on: false, ..p.config() };
        old.apply(cfg);
        assert!(!old.sp.config().on);
        new.apply(cfg);
        assert_eq!(new.sp.config(), cfg);
    }

    #[test]
    fn a_snapshot_takes_one_frame_and_puts_the_tap_back() {
        let counters = Arc::new(StreamCounters::default());
        let p = SharePreview::new(ViewerStream::null(), counters.clone(), PreviewConfig::default());
        let tap = p.attach().unwrap();
        let mut cb = tap.callback();
        tap.activate();
        assert!(!tap.sp.config().on, "the tile's preview is off");
        // Off: the tap runs at the snapshot size for one frame, which the tile does not show.
        p.request_snapshot();
        assert_eq!(tap.sp.config(), SNAPSHOT_CFG);
        frame_of(&mut cb);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 0, "not shown in the tile");
        assert!(!tap.sp.config().on, "off again right after");
        let f = p.take_snapshot().expect("the frame");
        assert_eq!((f.width, f.height), (16, 16));
        assert!(p.take_snapshot().is_none(), "handed over once");
        // (The real tap delivers nothing while it is off; a stray frame here is the tile's.)
        frame_of(&mut cb);
        assert!(p.take_snapshot().is_none(), "nobody asked");
        assert_eq!(counters.frames.load(Ordering::Relaxed), 1);
        // On: the tile's next frame, after a nudge of the size; the tile keeps its settings.
        let tile = PreviewConfig { on: true, fps: 5, max_width: 640, max_height: 360 };
        p.set_config(tile);
        tap.apply(tile);
        p.request_snapshot();
        assert_eq!(tap.sp.config().max_width, 638);
        frame_of(&mut cb);
        assert_eq!(counters.frames.load(Ordering::Relaxed), 2, "the tile shows it too");
        assert!(p.take_snapshot().is_some());
        assert_eq!(tap.sp.config(), tile);
        // Given up: the tap goes back to the tile's settings and a late frame is not kept.
        p.request_snapshot();
        p.cancel_snapshot();
        assert_eq!(tap.sp.config(), tile);
        frame_of(&mut cb);
        assert!(p.take_snapshot().is_none());
    }
}
