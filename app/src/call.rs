//! The call stage (v1.0, "Sala de voz" in docs/PLANO-v1.0.md): the cards of the people in our
//! voice room, a second card for each one who is live, the voice dock at the bottom and, in the
//! stage fullscreen, a header of its own. Watching a stream draws its video *inside* its card.
//!
//! - **Pins**: clicking a card (or "Fixar" in its menu) pins it; any number can be pinned. The
//!   pinned cards share the big area, the others go to a strip below (`stage_layout.rs`, the pure
//!   layout). Clicking a pinned card (or "Desafixar") unpins it. Pins are kept per person / stream
//!   while we are in the room, and cleared when we leave.
//! - **Hidden cards**: a card without video can be hidden from its menu ("Ocultar este card",
//!   "Ocultar todos sem vídeo"); "mostrar ocultos" in the header brings them back.
//! - **Stage fullscreen**: the header button, or a double-click on the stage's empty space, puts
//!   the window in OS fullscreen with only the stage (`App::stage_fullscreen`; the shell draws
//!   nothing else). Esc or the button leave it.
//!
//! Video is not drawn by egui. Each watched stream gets a [`VideoHost`]: a child window that the
//! core's viewer thread presents into with D3D11 (GPU YUV->RGB, no CPU conversion, no texture
//! upload into egui). Every frame the UI records where each video should be (`want`), then
//! [`App::sync_video`] moves the child windows there (only on change) and hides them when the call
//! view is not shown. Native windows always draw on top of egui ("airspace"), so the part of a
//! video under an egui window, menu, popup or tooltip is cut out of its child window (see
//! `video_clip.rs`): the popup shows, the rest of the video keeps playing. A video covered
//! entirely is hidden. Cards draw nothing over their video: the name tag sits under it.
//!
//! Fullscreen of one watched stream (on the monitor the app is on): a double-click on its video,
//! the card's fullscreen button, its menu, or F11; Esc, F11 or a double-click leave it and the
//! previous view comes back (the stage fullscreen too, when it was on). A popped-out stream does
//! the same in its own window.
//!
//! Several streams (item 9): "Assistir transmissão" on more live cards adds them (at most
//! [`MAX_WATCHED`], then a message), each playing in its own card, each with its own audio
//! volume / mute. The watched streams in the strip are drawn at [`STRIP_FPS`] (single-layer
//! streams: the SFU cannot send a smaller version, so the decode cost stays; only the drawing is
//! cut). "Abrir em janela" pops out one stream at a time, each in its own window. A watched
//! stream that is nowhere on screen (chat view, app minimized, another one fullscreen, scrolled
//! out of the strip) is paused at the SFU after a short grace (no network, no decode), and its
//! audio keeps playing. Optionally ("Ouvir só as transmissões fixadas") only the pinned streams
//! play sound.
//!
//! Webcams (item 11): a member whose camera is on shows it in their own card (like Discord), with
//! the name tag below and the speaking border around it. My own card shows the local preview,
//! mirrored, straight from the camera (never decoded from the network).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, Color32, Layout, Pos2, Rect, RichText, Sense, Vec2};
use vox_client_core::golive::Viewer;
use vox_client_core::voice::{camera_key, SELF_CAMERA_KEY};
use vox_client_core::MAX_WATCHED;

use crate::backend::Cmd;
use crate::context_menu::{Lead, Origin};
#[cfg(windows)]
use vox_client_core::golive::host::{PxRect, VideoHost};
use vox_proto::{User, UserId, VoiceMember};

use crate::app::{App, Center, ICON_TV};
use crate::theme::{self, ph, radius};
use crate::ui_kit::{self, IconState, Kind, Size};

#[path = "video_clip.rs"]
pub mod clip;
use clip::Clip;

#[path = "stage_layout.rs"]
pub mod stage;
pub use stage::{Hidden, TileKey};

#[path = "voice_dock.rs"]
pub mod dock;

const TOAST_SECS: u64 = 5;
/// The video inside a card, inset so the speaking border around the card stays visible.
const CARD_INSET: f32 = 4.0;
/// A card lower than this is "small" (the strip): compact tag, no buttons on it.
const SMALL_H: f32 = 130.0;
/// Height of the stage's own header (stage fullscreen).
const HEADER_H: f32 = 52.0;
/// Space around the cards.
const STAGE_PAD: f32 = 12.0;
/// Frame rate of the preview of our own live in the grid, and when it is pinned (item 8a).
pub const PREVIEW_FPS: u32 = 5;
pub const PREVIEW_FPS_FOCUSED: u32 = 15;
const _: () = assert!(PREVIEW_FPS < PREVIEW_FPS_FOCUSED);

/// Drawing rate of a watched stream in the strip under the pinned cards (item 9).
pub const STRIP_FPS: u32 = 15;
/// A pressed "Assistir" the core has not confirmed within this time is forgotten.
pub const WATCH_PENDING_TIMEOUT: Duration = Duration::from_secs(8);

/// May one more stream be watched, with `watching` confirmed and `pending` requested?
pub fn watch_allowed(watching: usize, pending: usize) -> bool {
    watching + pending < MAX_WATCHED
}

/// The message when the limit is reached.
pub fn watch_limit_text() -> String {
    format!("{ICON_TV} Você já está assistindo {MAX_WATCHED} transmissões, o máximo ao mesmo tempo. Pare de assistir uma para abrir outra.")
}

/// v1.1 (E1): the call header's badge in an end-to-end encrypted room.
pub const E2EE_BADGE: &str = "Criptografada de ponta a ponta";
/// v1.1 (E1): its explanation (hover).
pub const E2EE_BADGE_HOVER: &str = "Voz, câmeras e transmissões são cifradas no computador de quem envia, com chaves que só quem está na sala recebe. Nem o servidor consegue ver ou ouvir. Ele ainda vê quem está na sala e quem fala.";

/// v1.1 (E1): a DM call that stayed unencrypted because the other person's app is older.
pub fn e2ee_plain_call_text(name: &str) -> String {
    format!("Chamada sem criptografia de ponta a ponta: {name} usa uma versão antiga do concord.")
}

/// v1.1 (E1, audit): a DM call that stayed unencrypted for a reason this computer cannot tell
/// (the other person has a device with keys, so it is not their app's version; or the device
/// list was not read here): never blame their version then.
pub const E2EE_PLAIN_CALL_GENERIC: &str = "Chamada sem criptografia de ponta a ponta.";

/// v1.1 (E1, audit): which of the two to show for a plain DM call: the "old version" one only
/// when the whole device list was read here and the other person has no device in it.
pub fn e2ee_plain_call_note(name: &str, directory_read: bool, peer_has_device: bool) -> String {
    if directory_read && !peer_has_device {
        e2ee_plain_call_text(name)
    } else {
        E2EE_PLAIN_CALL_GENERIC.to_string()
    }
}

/// v1.1 (E1): what a person's card says while their media does not decrypt here.
pub fn e2ee_tile_text(state: vox_client_core::MediaE2eeState, name: &str) -> Option<String> {
    use vox_client_core::MediaE2eeState as S;
    match state {
        S::MissingKey => Some(format!("Aguardando a chave de {name}…")),
        S::DecryptionFailed => Some(format!("Não deu para descriptografar o áudio de {name}")),
        _ => None,
    }
}

/// Preview size for a tile that is `tile_px` physical pixels wide: the smallest class that covers
/// it, so a small tile costs less and a focused one is sharp enough to check details.
pub fn preview_size_for(tile_px: i32) -> (u32, u32) {
    const CLASSES: [(u32, u32); 4] = [(480, 270), (640, 360), (960, 540), (1280, 720)];
    CLASSES.into_iter().find(|(w, _)| *w as i32 >= tile_px).unwrap_or(CLASSES[CLASSES.len() - 1])
}

/// One card of the stage this frame.
#[derive(Clone)]
struct Tile {
    key: TileKey,
    member: VoiceMember,
    /// It shows a picture (a camera, a watched stream, my live's preview).
    video: bool,
}

/// What the stage drew for one card last frame (the autotest compares it with the video windows).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawnCard {
    pub key: TileKey,
    pub rect: Rect,
    /// Where its video was asked to go (points).
    pub video: Option<Rect>,
    pub pinned: bool,
}

#[derive(Default)]
pub struct CallUi {
    /// Borderless fullscreen of one watched stream (the whole app window shows only the video).
    pub fullscreen: bool,
    /// The stream shown fullscreen: the first pinned one, or the one double-clicked.
    pub fullscreen_key: Option<String>,
    /// Leave fullscreen at the end of this frame (popped out from fullscreen).
    pub fullscreen_off_pending: bool,
    /// Short message ("transmissão encerrada"), shown for a few seconds.
    pub toast: Option<(String, Instant)>,
    /// A single click on a watched stream card, applied (pin / unpin) once it is clearly not the
    /// first half of a double-click (which goes fullscreen instead).
    pub pending_click: Option<(String, Instant)>,
    /// Pinned cards, in the order they were pinned (v1.0: any number).
    pub pins: Vec<TileKey>,
    /// Cards hidden by the user (cards without video only).
    pub hidden: Hidden,
    /// The first card shown in the strip (it scrolls one card at a time).
    pub strip_first: usize,
    /// The window's OS fullscreen as last asked (a stream's fullscreen or the stage's).
    os_fullscreen: bool,
    /// The card the open person menu was opened from, and whether it shows video.
    pub menu_card: Option<(TileKey, bool)>,
    /// The card whose "Ocultar" items the open menu drew last (the autotest).
    pub card_menu_drawn: Option<TileKey>,
    /// Last frame: cards hidden, pinned cards on the stage, cards on the stage.
    pub hidden_count: usize,
    pub pinned_count: usize,
    pub card_count: usize,
    /// The cards as drawn last frame, and the area they were laid out in.
    pub drawn: Vec<DrawnCard>,
    pub stage_rect: Option<Rect>,
    /// How many times the layout was computed (it is only when its inputs change).
    pub layouts: u64,
    /// The last layout and what it was computed from.
    layout: Option<(stage::Input, stage::Layout)>,
    /// Buffers reused frame after frame.
    tiles: Vec<Tile>,
    keys: Vec<(TileKey, bool)>,
    pinned_idx: Vec<usize>,
    rest_idx: Vec<usize>,
    /// Width of the dock's middle group last frame (to centre it).
    dock_center_w: f32,
    /// The frame (`cumulative_frame_nr`) the dock last drew the microphone level in: the meter's
    /// repaint thread only runs while it is on screen (`App::meter_visible`), not while only the
    /// chat shows.
    pub meter_frame: Option<u64>,
    /// I6: last frame the window had no room for the chat docked beside the cards (with the
    /// stage's minimum): "Cards com o chat ao lado" shows the cards alone until it has
    /// (`layout::dock_range`, `App::effective_mode`).
    pub dock_squeezed: bool,
}

impl CallUi {
    /// Leaving the room: pins, hidden cards and the strip's scroll are forgotten.
    pub fn leave_room(&mut self) {
        self.pins.clear();
        self.hidden.clear();
        self.strip_first = 0;
        self.menu_card = None;
        self.pending_click = None;
        self.drawn.clear();
        self.hidden_count = 0;
        self.pinned_count = 0;
        self.card_count = 0;
    }
}

/// egui's double-click window.
const DOUBLE_CLICK: Duration = Duration::from_millis(300);

/// Inline video surfaces, one per watched stream.
#[derive(Default)]
pub struct VideoUi {
    /// The app's own window (the parent of the video child windows).
    pub parent: Option<isize>,
    #[cfg(windows)]
    hosts: HashMap<String, VideoHost>,
    /// Where each stream's video should be this frame (points), filled while drawing.
    pub want: HashMap<String, Rect>,
    /// Videos drawn inside an egui window (the camera preview in Settings): that window itself
    /// does not cover them.
    pub want_layer: HashMap<String, egui::LayerId>,
    /// Some video has an egui window / menu / popup over it (cut out, or hidden when covered
    /// entirely).
    pub covered: bool,
    /// What the overlays did to each placed video last frame (absent: nothing, or not placed).
    /// Logged on change; read by the autotest.
    pub clips: HashMap<String, Clip>,
    /// Placed rects (physical pixels) by stream, for logs and the autotest.
    pub placed: HashMap<String, (i32, i32, i32, i32)>,
    #[cfg_attr(not(windows), allow(dead_code))]
    host_failed: bool,
    /// Watched streams drawn in the strip under the pinned cards this frame (drawn at
    /// [`STRIP_FPS`]).
    pub strip: HashSet<String>,
    /// Watched streams inside the app's window as of the last frame (tile, strip, fullscreen).
    pub shown: HashSet<String>,
    /// What the core was last told about each watched stream (on screen or not).
    pub visible_sent: HashMap<String, bool>,
    /// Watched streams silenced because others are pinned.
    pub focus_muted_sent: HashSet<String>,
    /// macOS: the videos are textures the app draws itself (`video_gl.rs`).
    #[cfg(target_os = "macos")]
    pub mac: crate::video_gl::MacVideo,
}

impl VideoUi {
    /// The video `key` goes in `rect` this frame, drawn with `painter` (its layer and clip).
    pub fn put(&mut self, key: String, rect: Rect, painter: &egui::Painter) {
        #[cfg(target_os = "macos")]
        self.mac.paint_at.insert(key.clone(), (painter.layer_id(), painter.clip_rect()));
        #[cfg(not(target_os = "macos"))]
        let _ = painter;
        self.want.insert(key, rect);
    }
}

/// Points -> physical pixels of the client area (rounded edges, so neighbours never overlap).
pub fn to_px(r: Rect, ppp: f32) -> (i32, i32, i32, i32) {
    let x0 = (r.min.x * ppp).round() as i32;
    let y0 = (r.min.y * ppp).round() as i32;
    let x1 = (r.max.x * ppp).round() as i32;
    let y1 = (r.max.y * ppp).round() as i32;
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// Names in a case-insensitive order, without building lowercase copies.
fn cmp_names(a: &str, b: &str) -> std::cmp::Ordering {
    a.chars().flat_map(char::to_lowercase).cmp(b.chars().flat_map(char::to_lowercase))
}

/// "3 pessoas · 2 em foco · 1 card oculto".
pub fn stage_meta_text(people: usize, pinned: usize, hidden: usize) -> String {
    let mut s = if people == 1 { "1 pessoa".to_string() } else { format!("{people} pessoas") };
    if pinned > 0 {
        s.push_str(&format!(" · {pinned} em foco"));
    }
    match hidden {
        0 => {}
        1 => s.push_str(" · 1 card oculto"),
        n => s.push_str(&format!(" · {n} cards ocultos")),
    }
    s
}

impl App {
    // ------------------------------------------------------------ actions

    /// Show the call view of the channel we are in.
    pub fn show_call(&mut self) {
        self.center = Center::Call;
    }

    /// "Assistir" from the sidebar: the stage, with that stream pinned (next to the cards
    /// already pinned).
    pub fn watch_focused(&mut self, identity: &str) {
        self.center = Center::Call;
        if self.watch(identity) && !self.identity_is_audio_only(identity) {
            // (An audio-only live has no picture to enlarge: "Ouvir" just starts the sound.)
            if let Ok(id) = identity.parse::<UserId>() {
                self.pin(TileKey::Stream(id));
            }
        }
    }

    // ------------------------------------------------------------ pins and hidden cards

    pub fn is_pinned(&self, key: TileKey) -> bool {
        self.call.pins.contains(&key)
    }

    pub fn pin(&mut self, key: TileKey) {
        if !self.is_pinned(key) {
            self.call.pins.push(key);
            self.pins_changed();
        }
    }

    pub fn unpin(&mut self, key: TileKey) {
        if self.is_pinned(key) {
            self.call.pins.retain(|k| *k != key);
            self.pins_changed();
        }
    }

    pub fn toggle_pin(&mut self, key: TileKey) {
        if self.is_pinned(key) {
            self.unpin(key);
        } else {
            self.pin(key);
        }
    }

    /// Exactly this card pinned (`None`: nothing pinned). The old "focus" (autotests).
    pub fn pin_only(&mut self, key: Option<TileKey>) {
        self.call.pins.clear();
        self.call.pins.extend(key);
        self.pins_changed();
    }

    fn pins_changed(&mut self) {
        tracing::info!(pins = ?self.call.pins, "stage pins");
        self.repaint.wake();
    }

    /// The first pinned stream of somebody else that we watch (the old single "focus").
    pub fn focused_stream(&self) -> Option<String> {
        let me = self.my_id();
        self.call.pins.iter().find_map(|k| match k {
            TileKey::Stream(u) if Some(*u) != me => Some(u.to_string()).filter(|s| self.is_watching(s)),
            _ => None,
        })
    }

    /// The preview of my own live is pinned (it runs faster and bigger).
    pub fn self_preview_focused(&self) -> bool {
        self.my_id().is_some_and(|me| self.is_pinned(TileKey::Stream(me)))
    }

    /// A stream we stopped watching (or that ended) is no longer pinned.
    pub(crate) fn unpin_stream(&mut self, identity: &str) {
        if let Ok(id) = identity.parse::<UserId>() {
            self.unpin(TileKey::Stream(id));
        }
    }

    /// A camera that went off is no longer pinned (the card stays, as an avatar).
    pub(crate) fn unpin_camera(&mut self, identity: &str) {
        if let Ok(id) = identity.parse::<UserId>() {
            self.unpin(TileKey::Member(id));
        }
    }

    /// The preview of my live was switched off: its card is no longer pinned.
    pub(crate) fn unpin_self_stream(&mut self) {
        if let Some(me) = self.my_id() {
            self.unpin(TileKey::Stream(me));
        }
    }

    /// "Ocultar este card" (a card without video).
    pub fn hide_card(&mut self, user: UserId) {
        self.call.hidden.cards.insert(user);
        self.unpin(TileKey::Member(user));
        tracing::info!(user, hidden = ?self.call.hidden, "stage: card hidden");
        self.repaint.wake();
    }

    /// "Ocultar todos sem vídeo" (people who join later included).
    pub fn hide_cards_without_video(&mut self) {
        self.call.hidden.all_without_video = true;
        tracing::info!("stage: every card without video hidden");
        self.repaint.wake();
    }

    /// "Mostrar ocultos".
    pub fn show_hidden_cards(&mut self) {
        self.call.hidden.clear();
        tracing::info!("stage: hidden cards shown again");
        self.repaint.wake();
    }

    // ------------------------------------------------------------ fullscreen

    /// The watched streams that have a picture (all but the audio-only ones).
    fn watching_video(&self) -> Vec<String> {
        self.voice.watching.iter().filter(|k| !self.identity_is_audio_only(k)).cloned().collect()
    }

    /// Stream fullscreen on / off. Turning it on without a chosen stream takes the first pinned
    /// one (else the first watched one that is not in its own window).
    pub fn set_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        if on && self.call.fullscreen_key.is_none() {
            match self.fullscreen_target() {
                Some(k) => self.call.fullscreen_key = Some(k),
                None => return,
            }
        }
        if !on {
            self.call.fullscreen_key = None;
        }
        if self.call.fullscreen != on {
            self.call.fullscreen = on;
            tracing::info!(on, key = ?self.call.fullscreen_key, "stream fullscreen");
        }
        self.sync_os_fullscreen(ctx);
    }

    /// The stage fullscreen on / off (only in a call).
    pub fn set_stage_fullscreen(&mut self, ctx: &egui::Context, on: bool) {
        let on = on && self.voice.channel.is_some();
        if self.stage_fullscreen != on {
            self.stage_fullscreen = on;
            tracing::info!(on, "stage fullscreen");
        }
        self.sync_os_fullscreen(ctx);
        ctx.request_repaint();
    }

    /// The window is OS fullscreen while a stream or the stage is.
    fn sync_os_fullscreen(&mut self, ctx: &egui::Context) {
        let want = self.call.fullscreen || self.stage_fullscreen;
        if want == self.call.os_fullscreen {
            return;
        }
        self.call.os_fullscreen = want;
        tracing::info!(on = want, stream = self.call.fullscreen, stage = self.stage_fullscreen, "window fullscreen");
        match self.video.parent.filter(|_| vox_client_core::testmode::background()) {
            // Autotests: the same fullscreen without taking the foreground (`testmode`).
            Some(hwnd) => vox_client_core::testmode::fullscreen_behind(hwnd, want),
            None => ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(want)),
        }
    }

    /// Fullscreen of this watched stream (a double-click on it, its "Tela cheia").
    pub fn fullscreen_stream(&mut self, ctx: &egui::Context, key: &str) {
        self.call.fullscreen_key = Some(key.to_string());
        self.set_fullscreen(ctx, true);
    }

    /// The stream F11 / "Tela cheia" without a target shows: the first pinned one, else the first
    /// watched one inside the app.
    fn fullscreen_target(&self) -> Option<String> {
        let inline = |k: &String| self.is_watching(k) && !self.identity_is_audio_only(k) && !self.stream_popped(k);
        self.focused_stream().filter(inline).or_else(|| self.voice.watching.iter().find(|k| inline(k)).cloned())
    }

    /// Move one watched stream into its own window (`out`) or back inside the app. Each stream
    /// has its own window.
    pub fn pop_out_stream(&mut self, key: &str, out: bool) {
        if !self.is_watching(key) {
            return;
        }
        if let Ok(v) = Viewer::get() {
            let name = key.parse::<UserId>().map(|id| self.name_of(id)).unwrap_or_else(|_| key.to_string());
            v.set_title(key, &format!("concord - transmissão de {name}"));
            v.pop_out(key, out);
        }
        if out && self.call.fullscreen_key.as_deref() == Some(key) {
            self.call.fullscreen_off_pending = true;
        }
    }

    /// Whether this watched stream is in its own window right now.
    pub fn stream_popped(&self, key: &str) -> bool {
        Viewer::get().ok().and_then(|v| v.state(key)).is_some_and(|s| s.popped_out)
    }

    /// In its own window, and that window is not minimized (someone can see it).
    fn stream_in_own_window(&self, key: &str) -> bool {
        let Some(st) = Viewer::get().ok().and_then(|v| v.state(key)) else { return false };
        if !st.popped_out {
            return false;
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::IsIconic;
            // SAFETY: read-only query; a stale handle just answers false.
            st.popout_hwnd == 0 || unsafe { IsIconic(st.popout_hwnd as _) } == 0
        }
        #[cfg(not(windows))]
        true
    }

    /// The egui context (for actions taken outside a frame callback, e.g. the autotest).
    pub fn repaint_ctx(&self) -> egui::Context {
        self.repaint.ctx().expect("egui context is set before the first frame")
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.call.toast = Some((text.into(), Instant::now()));
    }

    /// A streamer we watch is no longer live (or left): close that stream with a message; the
    /// other watched streams go on.
    pub fn check_watched_alive(&mut self) {
        if !self.ready {
            return;
        }
        let mut ended = Vec::new();
        for w in self.voice.watching.clone() {
            let Ok(id) = w.parse::<UserId>() else { continue };
            // (In a DM call the members come from the room, not from the hub's voice state.)
            let live = if self.voice.channel == Some(crate::app::dms::DM_CALL_CHANNEL) {
                self.dms.call_members.iter().any(|m| m.user.id == id && m.streaming)
            } else {
                self.voice_members.get(&id).is_some_and(|m| m.streaming && m.channel_id.is_some() && m.channel_id == self.voice.channel)
            };
            if !live {
                let name = self.name_of(id);
                tracing::info!(streamer = %name, still_watching = self.voice.watching.len() - 1, "watched stream ended (hub state)");
                self.unwatch(&w);
                self.voice.watching.retain(|x| *x != w);
                self.unpin_stream(&w);
                ended.push(name);
            }
        }
        match ended.as_slice() {
            [] => {}
            [one] => self.toast(format!("{ICON_TV} A transmissão de {one} foi encerrada.")),
            many => self.toast(format!("{ICON_TV} As transmissões de {} foram encerradas.", many.join(", "))),
        }
    }

    // ------------------------------------------------------------ drawing

    pub fn toast_bar(&mut self, ui: &mut egui::Ui) {
        let Some((text, at)) = self.call.toast.clone() else { return };
        let left = Duration::from_secs(TOAST_SECS).saturating_sub(at.elapsed());
        if left.is_zero() {
            self.call.toast = None;
            return;
        }
        // A kit notice floating over the top of the window (it covers nothing for long, and goes
        // by itself).
        let ctx = ui.ctx().clone();
        let mut close = false;
        egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 64.0))
            .interactable(true)
            .show(&ctx, |ui| {
                ui_kit::notice_frame(ui_kit::Notice::Info).fill(theme::MOVEL).shadow(ctx.global_style().visuals.popup_shadow).show(ui, |ui| {
                    ui.set_max_width(460.0);
                    ui.horizontal(|ui| {
                        ui_kit::notice_text(ui, ui_kit::Notice::Info, &text);
                        close = ui_kit::icon_button(ui, ph::X, "Fechar o aviso", IconState::Normal).clicked();
                    });
                });
            });
        if close {
            self.call.toast = None;
        }
        ctx.request_repaint_after(left);
    }

    /// Stream fullscreen: nothing but one watched video (the other watched streams are hidden, so
    /// they get paused). Esc or a double-click leaves.
    pub fn fullscreen_view(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::no_frame().show(ui, |ui| self.fullscreen_video(ui));
    }

    /// The fullscreen video in `ui`'s whole rect (Esc, F11 or a double-click leave).
    fn fullscreen_video(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let key = self.call.fullscreen_key.clone();
        let mut exit = ctx.input(|i| i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::F11));
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let resp = ui.interact(rect, egui::Id::new("fullscreen_video"), Sense::click());
        exit |= resp.double_clicked();
        if let Some(k) = key {
            self.video.put(k, rect, ui.painter());
        }
        if exit {
            self.set_fullscreen(&ctx, false);
        }
    }

    /// The whole stage in `ui.max_rect()`: the cards, the voice dock under them and, in the stage
    /// fullscreen, a header above them. (The shell calls it as `call_stage_ui`.)
    pub fn call_view(&mut self, ui: &mut egui::Ui) {
        if self.call.fullscreen {
            // A stream's fullscreen while the stage is fullscreen: the stream wins.
            self.fullscreen_video(ui);
            return;
        }
        let ctx = ui.ctx().clone();
        let full = ui.max_rect();
        ui.allocate_rect(full, Sense::hover());
        let mut area = full;
        if self.stage_fullscreen {
            // (The shell's panel has no frame then: the stage paints its own night.)
            ui.painter().rect_filled(full, 0.0, theme::NOITE);
            let head = Rect::from_min_size(full.min, Vec2::new(full.width(), HEADER_H));
            self.stage_header(ui, head);
            area.min.y = head.bottom();
            // Esc leaves (a menu or popup that is open takes the Esc first).
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !egui::Popup::is_any_open(&ctx) && self.cmenu.open.is_none() {
                self.set_stage_fullscreen(&ctx, false);
            }
        }
        let dock = Rect::from_min_max(Pos2::new(area.left(), (area.bottom() - dock::DOCK_H).max(area.top())), area.max);
        area.max.y = dock.top();
        self.voice_dock(ui, dock);
        let cards = area.shrink(STAGE_PAD);
        if self.voice.channel.is_none() {
            let text = if self.voice.joining.is_some() { "Conectando à voz..." } else { "Você não está em um canal de voz." };
            ui.painter().text(cards.center(), Align2::CENTER_CENTER, text, theme::body(15.0), theme::NEVOA);
            return;
        }
        if cards.width() > 1.0 && cards.height() > 1.0 {
            self.stage_cards(ui, cards);
        }
    }

    /// The stage's header in the stage fullscreen (the shell's center header is not drawn then):
    /// the room, the counts and the stage's buttons.
    fn stage_header(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.hline(rect.x_range(), rect.bottom() - 0.5, egui::Stroke::new(theme::stroke::HAIRLINE, theme::RISCO));
        let inner = rect.shrink2(Vec2::new(16.0, 0.0));
        // The buttons first (right to left); the room's name and the counts get what is left.
        let mut right = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::right_to_left(Align::Center)));
        self.stage_tools(&mut right, false);
        // (The right-to-left cursor's right edge: where the buttons end, on their left.)
        let room = Rect::from_min_max(inner.min, Pos2::new((right.cursor().max.x - 12.0).max(inner.left()), inner.bottom()));
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(room).layout(Layout::left_to_right(Align::Center)));
        left.set_clip_rect(room.intersect(ui.clip_rect()));
        left.label(RichText::new(ph::SPEAKER_HIGH).size(19.0).color(theme::NEVOA));
        left.add(egui::Label::new(RichText::new(self.room_name()).font(theme::display(17.0)).color(theme::GIZ)).truncate().selectable(false));
        self.e2ee_header_badge(&mut left);
        ui_kit::header_extra(&mut left, Some(self.stage_meta()), 13.0);
        crate::layout::probe(ui.ctx(), "stage_title", left.min_rect());
        crate::layout::probe(ui.ctx(), "stage_tools", Rect::from_min_max(Pos2::new(right.cursor().max.x, inner.top()), inner.max));
    }

    /// v1.1 (E1): "Criptografada de ponta a ponta" beside the room's name (or, for a DM call that
    /// could not be, that it is not).
    pub fn e2ee_header_badge(&self, ui: &mut egui::Ui) {
        if self.voice.channel.is_none() {
            return;
        }
        if self.voice.e2ee {
            let r = ui.add(egui::Label::new(RichText::new(format!("{}  {E2EE_BADGE}", ph::LOCK_SIMPLE)).font(theme::body(12.5)).color(theme::ONLINE)).truncate().selectable(false)).on_hover_text(E2EE_BADGE_HOVER);
            crate::layout::probe(ui.ctx(), "call_e2ee_badge", r.rect);
        } else if self.voice.e2ee_plain_call {
            let hover = self.voice.warning.clone().unwrap_or_default();
            ui.add(egui::Label::new(RichText::new(format!("{}  Sem criptografia de ponta a ponta", ph::LOCK_SIMPLE_OPEN)).font(theme::body(12.5)).color(theme::AVISO)).truncate().selectable(false)).on_hover_text(hover);
        }
    }

    /// The name of the room we are in (a private call's title for a DM call).
    pub fn room_name(&self) -> String {
        match self.voice.channel.or(self.voice.joining) {
            Some(crate::app::dms::DM_CALL_CHANNEL) => self.dm_call_title(),
            Some(ch) => self.channels.iter().find(|c| c.id == ch).map(|c| c.name.clone()).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// "3 pessoas · 2 em foco · 1 card oculto" (as of the last frame).
    pub fn stage_meta(&self) -> String {
        stage_meta_text(self.members_here().len(), self.call.pinned_count, self.call.hidden_count)
    }

    /// The stage's own buttons, laid out by `ui` (the shell's center header is right to left):
    /// the stage fullscreen, "mostrar ocultos" (with the count, while cards are hidden) and, with
    /// `meta`, the counts.
    pub fn stage_tools(&mut self, ui: &mut egui::Ui, meta: bool) {
        let ctx = ui.ctx().clone();
        let full = self.stage_fullscreen;
        let (icon, hover, state) = if full {
            (ph::CORNERS_IN, "Sair da tela cheia (Esc)", IconState::On)
        } else {
            (ph::CORNERS_OUT, "Tela cheia dos cards (ou clique duplo no fundo da sala)", IconState::Normal)
        };
        if ui_kit::icon_button(ui, icon, hover, state).clicked() {
            self.set_stage_fullscreen(&ctx, !full);
        }
        let hidden = self.call.hidden_count;
        if hidden > 0 {
            let text = hidden.to_string();
            let r = ui_kit::button_sized(ui, Kind::Ghost, Size::Small, Some(ph::EYE_SLASH), &text);
            if r.on_hover_text("Mostrar os cards ocultos").clicked() {
                self.show_hidden_cards();
            }
        }
        if meta {
            ui.label(RichText::new(self.stage_meta()).size(13.0).color(theme::NEVOA));
        }
    }

    /// The cards of the room, in `area`.
    fn stage_cards(&mut self, ui: &mut egui::Ui, area: Rect) {
        let ctx = ui.ctx().clone();
        // A single click on a watched stream (no second click came): pin it / unpin it.
        if let Some((key, at)) = self.call.pending_click.clone() {
            if at.elapsed() >= DOUBLE_CLICK {
                self.call.pending_click = None;
                if self.is_watching(&key) {
                    self.unpin_or_pin_stream(&key);
                }
            } else {
                ctx.request_repaint_after(DOUBLE_CLICK.saturating_sub(at.elapsed()));
            }
        }
        // F11: the first pinned (else the first) watched stream fullscreen (a popped-out stream
        // handles F11 in its own window).
        if !self.voice.watching.is_empty() && ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            self.set_fullscreen(&ctx, true);
        }
        // The stage's empty space: a double-click toggles the stage fullscreen. (First, so every
        // card on top of it gets its own clicks.)
        let bg = ui.interact(area, egui::Id::new("stage_background"), Sense::click());

        let mut tiles = std::mem::take(&mut self.call.tiles);
        let mut keys = std::mem::take(&mut self.call.keys);
        let mut pinned = std::mem::take(&mut self.call.pinned_idx);
        let mut rest = std::mem::take(&mut self.call.rest_idx);
        self.build_tiles(&mut tiles, &mut keys);
        let hidden = stage::arrange(&keys, &self.call.pins, &self.call.hidden, &mut pinned, &mut rest);
        let counts = (hidden, pinned.len(), pinned.len() + rest.len());
        if counts != (self.call.hidden_count, self.call.pinned_count, self.call.card_count) {
            (self.call.hidden_count, self.call.pinned_count, self.call.card_count) = counts;
            // The header (drawn before the stage) shows these.
            ctx.request_repaint();
        }
        let input = stage::Input { pinned: pinned.len(), rest: rest.len(), area, strip_first: self.call.strip_first };
        let lay = match self.call.layout.take() {
            Some((i, l)) if i == input => l,
            _ => {
                self.call.layouts += 1;
                let l = stage::layout(input);
                tracing::debug!(pinned = input.pinned, rest = input.rest, area = ?area, n = self.call.layouts, "stage layout");
                l
            }
        };
        self.call.strip_first = lay.strip_first;
        self.call.drawn.clear();
        self.call.stage_rect = Some(area);

        if tiles.is_empty() {
            ui.painter().text(area.center(), Align2::CENTER_CENTER, "Ninguém na sala.", theme::body(15.0), theme::NEVOA);
        } else if pinned.is_empty() && rest.is_empty() {
            // Every card is hidden.
            ui.painter().text(area.center() - Vec2::new(0.0, 22.0), Align2::CENTER_CENTER, "Todos os cards estão ocultos.", theme::body(15.0), theme::NEVOA);
            let r = Rect::from_center_size(area.center() + Vec2::new(0.0, 14.0), Vec2::new(240.0, 32.0));
            let mut c = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Center)));
            if ui_kit::button(&mut c, Kind::Secondary, Some(ph::EYE), "Mostrar ocultos").clicked() {
                self.show_hidden_cards();
            }
        }
        let with_pins = !pinned.is_empty();
        let main = if with_pins { &pinned } else { &rest };
        for (k, &i) in main.iter().enumerate() {
            if let Some(r) = lay.main.get(k) {
                self.draw_tile(ui, &tiles[i], *r);
            }
        }
        if with_pins {
            for (k, &i) in rest.iter().enumerate() {
                let Some(Some(r)) = lay.strip.get(k) else { continue };
                self.draw_tile(ui, &tiles[i], *r);
                let t = &tiles[i];
                if t.video && matches!(t.key, TileKey::Stream(_)) && Some(t.key.user()) != self.my_id() {
                    // Still playing, at its small size: drawn less often.
                    self.video.strip.insert(t.key.user().to_string());
                }
            }
            self.strip_scroll(ui, &lay);
        }
        if bg.double_clicked() {
            let on = !self.stage_fullscreen;
            self.set_stage_fullscreen(&ctx, on);
        }

        self.call.layout = Some((input, lay));
        self.call.tiles = tiles;
        self.call.keys = keys;
        self.call.pinned_idx = pinned;
        self.call.rest_idx = rest;
    }

    /// The single click on a watched stream card, applied.
    fn unpin_or_pin_stream(&mut self, key: &str) {
        if let Ok(id) = key.parse::<UserId>() {
            self.toggle_pin(TileKey::Stream(id));
        }
    }

    /// The strip's arrows and the mouse wheel over it.
    fn strip_scroll(&mut self, ui: &mut egui::Ui, lay: &stage::Layout) {
        let (back, fwd) = lay.can_scroll();
        let mut step = 0i32;
        if let Some(r) = lay.prev {
            if strip_arrow(ui, r, ph::CARET_LEFT, back, "Cards anteriores") {
                step = -1;
            }
        }
        if let Some(r) = lay.next {
            if strip_arrow(ui, r, ph::CARET_RIGHT, fwd, "Mais cards") {
                step = 1;
            }
        }
        if let Some(band) = lay.band.filter(|_| back || fwd) {
            if ui.rect_contains_pointer(band) {
                let d = ui.input(|i| {
                    i.events.iter().fold(Vec2::ZERO, |acc, e| match e {
                        egui::Event::MouseWheel { delta, .. } => acc + *delta,
                        _ => acc,
                    })
                });
                // Content moves with the wheel: up / left shows the cards before.
                let along = if d.x.abs() > d.y.abs() { d.x } else { d.y };
                if along > 0.0 && back {
                    step = -1;
                } else if along < 0.0 && fwd {
                    step = 1;
                }
            }
        }
        if step != 0 {
            self.call.strip_first = (self.call.strip_first as i64 + step as i64).max(0) as usize;
            ui.ctx().request_repaint();
        }
    }

    /// The cards of the room in their natural order (people by name, each one's stream right
    /// after them), into `tiles` and `keys` (cleared first).
    fn build_tiles(&self, tiles: &mut Vec<Tile>, keys: &mut Vec<(TileKey, bool)>) {
        tiles.clear();
        keys.clear();
        let mut members = self.members_here();
        members.sort_by(|a, b| cmp_names(a.user.shown_name(), b.user.shown_name()));
        let me = self.my_id();
        for m in members {
            let id = m.user.id;
            let cam = self.camera_key_of(id).is_some();
            tiles.push(Tile { key: TileKey::Member(id), member: m.clone(), video: cam });
            if m.streaming {
                let key = id.to_string();
                let video = if Some(id) == me {
                    self.my_preview_live()
                } else {
                    self.is_watching(&key) && !self.stream_is_audio_only(m) && !self.stream_popped(&key)
                };
                tiles.push(Tile { key: TileKey::Stream(id), member: m.clone(), video });
            }
        }
        keys.extend(tiles.iter().map(|t| (t.key, t.video)));
    }

    /// A right click on a card: the person's menu (with the card's own items first).
    fn card_menu_hit(&mut self, resp: &egui::Response, tile: &Tile, lead: Lead) {
        self.menu_hit(resp, &tile.member.user, Origin::Tile, lead);
        if resp.secondary_clicked() {
            self.call.menu_card = Some((tile.key, tile.video));
        }
    }

    /// Open a stage card's menu at `pos` as a right click on it does (the autotest).
    pub(crate) fn open_card_menu(&mut self, ctx: &egui::Context, key: TileKey, pos: Pos2) -> bool {
        let Some(t) = self.call.tiles.iter().find(|t| t.key == key).cloned() else { return false };
        let lead = match key {
            TileKey::Stream(_) => Lead::Stream,
            TileKey::Member(_) if t.video => Lead::Camera,
            TileKey::Member(_) => Lead::None,
        };
        self.call.card_menu_drawn = None;
        self.open_person_menu(ctx, &t.member.user, Origin::Tile, lead, pos);
        self.call.menu_card = Some((t.key, t.video));
        true
    }

    fn draw_tile(&mut self, ui: &mut egui::Ui, tile: &Tile, rect: Rect) {
        let painter = ui.painter_at(rect);
        ui_kit::paint_card(&painter, rect, false);
        let pinned = self.is_pinned(tile.key);
        let video = match tile.key {
            TileKey::Member(_) => self.draw_member_card(ui, &painter, tile, rect, pinned),
            TileKey::Stream(_) => self.draw_stream_card(ui, &painter, tile, rect, pinned),
        };
        if self.call.drawn.len() < 64 {
            self.call.drawn.push(DrawnCard { key: tile.key, rect, video, pinned });
        }
    }

    /// A person's card: their camera, or their picture with the speaking ring. Returns where its
    /// video goes.
    fn draw_member_card(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, tile: &Tile, rect: Rect, pinned: bool) -> Option<Rect> {
        let m = &tile.member;
        let uid = m.user.id;
        let me = self.my_id() == Some(uid);
        let small = rect.height() < SMALL_H;
        let id = egui::Id::new(("stage_card", tile.key));
        // (Not `!m.muted`: a muted mic sends nothing, so it never puts anyone in the speaking set,
        // but a sound played from the soundboard does, muted or not. A server-muted member can do
        // neither: the hub refuses their sounds.)
        let speaking = !m.server_muted && if me { self.sending_voice() } else { self.voice.speaking.contains(&uid.to_string()) };
        let cam = self.camera_key_of(uid);
        self.paint_sound_flash(painter, rect, uid, cam.is_none());
        let lead = if cam.is_some() { Lead::Camera } else { Lead::None };
        let base = ui.interact(rect, id, Sense::click()).on_hover_text(card_hover(pinned, false));
        self.card_menu_hit(&base, tile, lead);
        if base.clicked() {
            self.toggle_pin(tile.key);
        }
        let mut name = m.user.shown_name().to_string();
        if me {
            name.push_str(" (você)");
        }
        let marks = [
            m.muted.then_some((ph::MICROPHONE_SLASH, theme::AO_VIVO)),
            m.deafened.then_some((ph::HEADPHONES, theme::AO_VIVO)),
            (cam.is_none() && self.camera_flag_of(m)).then_some((ph::VIDEO_CAMERA, theme::NEVOA)),
        ];
        let tag = Tag { pin: pinned, icon: None, name: &name, live: None, marks, viewers: None };
        let mut placed = None;
        let tag_rect = match cam {
            Some(vkey) => {
                let body = video_body(rect, small);
                painter.rect_filled(body, radius::ROW, Color32::BLACK);
                self.video.put(vkey, body, painter);
                placed = Some(body);
                let (band, at) = tag_band(rect, small);
                let actions = if me && !small && rect.width() >= 280.0 { 1 } else { 0 };
                let t = paint_tag(painter, at, band.width() - actions as f32 * 34.0, &tag, small);
                if actions > 0 {
                    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(band).layout(Layout::right_to_left(Align::Center)));
                    if ui_kit::icon_button(&mut row, ph::VIDEO_CAMERA_SLASH, "Desligar câmera", IconState::Normal).clicked() {
                        self.stop_camera();
                    }
                }
                t
            }
            None => {
                let c = rect.center() - Vec2::new(0.0, if small { 8.0 } else { 14.0 });
                // (Never so big that the speaking ring would leave the card, which clips it: a
                // short strip card in a small window.)
                let room = c.y - rect.top() - (ui_kit::speaking_ring_reach(0.0) + 2.0);
                let r = (rect.height().min(rect.width()) * 0.2).clamp(12.0, 64.0).min(room).max(6.0);
                // The person's picture, or the coloured initial while it loads / when there is none.
                crate::avatars::draw_avatar(&mut self.avatars.store, painter, ui.ctx(), c, r, &m.user, crate::avatars::Surface::Tile);
                if speaking {
                    ui_kit::paint_speaking_ring(painter, c, r, theme::MOVEL);
                }
                let inset = if small { 6.0 } else { 10.0 };
                paint_tag(painter, Pos2::new(rect.left() + inset, rect.bottom() - inset), rect.width() - 2.0 * inset, &tag, small)
            }
        };
        if speaking {
            ui_kit::paint_card_speaking(painter, rect);
        }
        // v1.1 (E1): their media does not decrypt here (yet).
        if let Some(text) = self.voice.e2ee_states.get(&uid.to_string()).and_then(|s| e2ee_tile_text(*s, m.user.shown_name())).filter(|_| self.voice.e2ee && !me) {
            let inset = if small { 6.0 } else { 10.0 };
            let font = theme::body(if small { 11.0 } else { 12.0 });
            let galley = painter.layout(format!("{}  {text}", ph::LOCK_SIMPLE), font, theme::AVISO, (rect.width() - 2.0 * inset).max(10.0));
            let at = Pos2::new(rect.left() + inset, rect.top() + inset);
            painter.rect_filled(Rect::from_min_size(at, galley.size()).expand(4.0), radius::ROW, Color32::from_black_alpha(170));
            painter.galley(at, galley, theme::AVISO);
        }
        // The name tag opens the person's profile card (after the card's own area, so it is the
        // one under the pointer).
        let name_resp = ui.interact(tag_rect, id.with("name"), Sense::click());
        self.card_menu_hit(&name_resp, tile, lead);
        if crate::profile::hit(name_resp, &m.user) {
            let ctx = ui.ctx().clone();
            self.open_profile(&ctx, &m.user);
        }
        placed
    }

    /// A live person's stream card. Returns where its video goes.
    fn draw_stream_card(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, tile: &Tile, rect: Rect, pinned: bool) -> Option<Rect> {
        let m = &tile.member;
        let uid = m.user.id;
        let me = self.my_id() == Some(uid);
        let small = rect.height() < SMALL_H;
        let id = egui::Id::new(("stage_card", tile.key));
        let key = uid.to_string();
        let watching = self.is_watching(&key);
        let audio_only = self.stream_is_audio_only(m);
        let viewers = if self.viewers_shown() { Some(self.viewers_of(uid).len()) } else { None };
        let ctx = ui.ctx().clone();
        // Right click anywhere on the card: the stream's menu first, then the person's.
        let base = ui.interact(rect, id, Sense::click());
        self.card_menu_hit(&base, tile, Lead::Stream);
        let (band, at) = tag_band(rect, small);
        // Everything above the name tag.
        let content = Rect::from_min_max(rect.min + Vec2::splat(CARD_INSET), Pos2::new(rect.right() - CARD_INSET, band.top()));
        let big = !small && rect.width() >= 300.0;
        let mut placed = None;
        let mut actions = 0;
        let pending = self.voice.watch_pending.iter().any(|(w, at)| *w == key && at.elapsed() < WATCH_PENDING_TIMEOUT);
        let same = self.voice.channel.is_some() && m.channel_id == self.voice.channel;
        if audio_only {
            // "Só áudio": no picture at all, a music card instead.
            let state = if me {
                if matches!(self.voice.share, crate::app::Share::Off) { "" } else { "Você está transmitindo só o áudio" }
            } else if watching {
                "Ouvindo"
            } else {
                "Só o som, sem imagem"
            };
            audio_card(painter, content, small, state);
            let at_btn = content.center() + Vec2::new(0.0, if small { 18.0 } else { 44.0 });
            if me {
                // Nothing to listen to: you never hear your own share.
            } else if watching {
                let row = Rect::from_center_size(at_btn, Vec2::new(content.width(), 30.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(row).layout(Layout::left_to_right(Align::Center)));
                let w = if small { 110.0 } else { 170.0 };
                child.add_space(((row.width() - w) / 2.0).max(0.0));
                self.stream_audio_menu(&mut child, &m.user);
                if ui_kit::card_button(&mut child, false, Size::Small, Some(ph::STOP), if small { "Parar" } else { "Parar de ouvir" }).clicked() {
                    self.unwatch(&key);
                }
            } else if pending {
                painter.text(at_btn, Align2::CENTER_CENTER, "Abrindo...", theme::body(if small { 11.0 } else { 14.0 }), theme::NEVOA);
                ctx.request_repaint_after(Duration::from_secs(1));
            } else if centered_card_button(ui, at_btn, true, if small { Size::Small } else { Size::Normal }, Some(ph::SPEAKER_HIGH), if small { "Ouvir" } else { "Ouvir o áudio" }, (!same).then_some("Entre neste canal de voz para ouvir")).clicked() {
                self.watch(&key);
            }
            if base.clicked() {
                self.toggle_pin(tile.key);
            }
        } else if me && self.my_preview_live() {
            // The preview comes from our own capture (never decoded from the network); it is only
            // produced while this card is on screen.
            let body = video_body(rect, small);
            painter.rect_filled(body, radius::ROW, Color32::BLACK);
            self.video.put(vox_client_core::golive::SELF_SHARE_KEY.to_string(), body, painter);
            placed = Some(body);
            if base.on_hover_text(card_hover(pinned, false)).clicked() {
                self.toggle_pin(tile.key);
            }
            if big {
                actions = 1;
                let mut row = ui.new_child(egui::UiBuilder::new().max_rect(band).layout(Layout::right_to_left(Align::Center)));
                let label = if self.voice.switching_to.is_some() { "Trocando..." } else { "Trocar tela" };
                let r = row.add_enabled_ui(self.voice.switching_to.is_none(), |ui| ui_kit::icon_button(ui, ph::ARROWS_CLOCKWISE, label, IconState::Normal)).inner;
                if r.clicked() {
                    self.open_golive_switch();
                }
            }
        } else if me {
            // The preview is off (Settings or the menu): just "ao vivo" and who is watching.
            painter.text(content.center() - Vec2::new(0.0, if small { 6.0 } else { 16.0 }), Align2::CENTER_CENTER, "Prévia desligada", theme::body(if small { 11.0 } else { 15.0 }), theme::NEVOA);
            if let Some(n) = viewers {
                let line = if n == 1 { "1 assistindo".to_string() } else { format!("{n} assistindo") };
                painter.text(content.center() + Vec2::new(0.0, if small { 8.0 } else { 4.0 }), Align2::CENTER_CENTER, line, theme::body(if small { 10.0 } else { 13.0 }), theme::NEVOA);
            }
            if !small && centered_card_button(ui, content.center() + Vec2::new(0.0, 34.0), false, Size::Small, Some(ph::EYE), "Mostrar prévia", None).clicked() {
                self.set_self_preview(true);
            }
            if base.clicked() {
                self.toggle_pin(tile.key);
            }
        } else if watching && self.stream_popped(&key) {
            painter.text(content.center() - Vec2::new(0.0, if small { 0.0 } else { 14.0 }), Align2::CENTER_CENTER, "Aberta em outra janela", theme::body(if small { 11.0 } else { 14.0 }), theme::NEVOA);
            if !small && centered_card_button(ui, content.center() + Vec2::new(0.0, 16.0), false, Size::Small, Some(ph::ARROW_SQUARE_OUT), "Trazer de volta", None).clicked() {
                self.pop_out_stream(&key, false);
            }
            if base.clicked() {
                self.toggle_pin(tile.key);
            }
        } else if watching {
            let body = video_body(rect, small);
            painter.rect_filled(body, radius::ROW, Color32::BLACK);
            self.video.put(key.clone(), body, painter);
            placed = Some(body);
            let resp = base.on_hover_text(card_hover(pinned, true));
            if resp.double_clicked() {
                // Fullscreen of this stream; leaving it comes back to this same view.
                self.call.pending_click = None;
                self.fullscreen_stream(&ctx, &key);
            } else if resp.clicked() {
                self.call.pending_click = Some((key.clone(), Instant::now()));
                ctx.request_repaint_after(DOUBLE_CLICK);
            }
            if big {
                actions = 4;
                let mut row = ui.new_child(egui::UiBuilder::new().max_rect(band.shrink2(Vec2::new(4.0, 0.0))).layout(Layout::right_to_left(Align::Center)));
                if ui_kit::icon_button(&mut row, ph::X, "Parar de assistir", IconState::Normal).clicked() {
                    self.unwatch(&key);
                }
                if ui_kit::icon_button(&mut row, ph::CORNERS_OUT, "Tela cheia (Esc ou clique duplo para sair)", IconState::Normal).clicked() {
                    self.fullscreen_stream(&ctx, &key);
                }
                if ui_kit::icon_button(&mut row, ph::ARROW_SQUARE_OUT, "Abrir em janela (fechar a janela traz a transmissão de volta)", IconState::Normal).clicked() {
                    self.pop_out_stream(&key, true);
                }
                self.stream_audio_menu(&mut row, &m.user);
            }
        } else {
            let label = if small { "Assistir" } else { "Assistir transmissão" };
            if pending {
                painter.text(content.center(), Align2::CENTER_CENTER, "Abrindo...", theme::body(if small { 11.0 } else { 14.0 }), theme::NEVOA);
                ctx.request_repaint_after(Duration::from_secs(1));
            } else {
                if !small {
                    painter.text(content.center() - Vec2::new(0.0, 30.0), Align2::CENTER_CENTER, ph::MONITOR, theme::body(30.0), theme::NEVOA);
                }
                let at_btn = content.center() + Vec2::new(0.0, if small { 0.0 } else { 10.0 });
                if centered_card_button(ui, at_btn, true, if small { Size::Small } else { Size::Normal }, Some(ph::PLAY), label, (!same).then_some("Entre neste canal de voz para assistir")).clicked() {
                    // Next to the streams already watched (a message past the limit).
                    self.watch(&key);
                }
            }
            // v1.1: resting the pointer on a live not watched yet shows its still picture.
            let hint = if same { "Clique em Assistir para ver ao vivo" } else { "Entre neste canal de voz para assistir" };
            let hovered = base.hovered() && !base.is_pointer_button_down_on();
            self.stream_hover(ui, "card", content, hovered, &m.user, Some(hint));
            if base.clicked() {
                self.toggle_pin(tile.key);
            }
        }
        let name = if me { "Sua transmissão" } else { m.user.shown_name() };
        let tag = Tag {
            pin: pinned,
            icon: Some((if audio_only { ph::MUSIC_NOTES } else { ph::MONITOR }, theme::NEVOA)),
            name,
            live: Some(if audio_only { "ao vivo · só áudio" } else { "ao vivo" }),
            marks: [None; 3],
            viewers,
        };
        let tag_rect = paint_tag(painter, at, band.width() - actions as f32 * 34.0, &tag, small);
        // The tag opens the streamer's profile card (after the card's own area).
        let name_resp = ui.interact(tag_rect, id.with("name"), Sense::click());
        self.card_menu_hit(&name_resp, tile, Lead::Stream);
        if crate::profile::hit(name_resp, &m.user) {
            self.open_profile(&ctx, &m.user);
        }
        placed
    }

    /// The card's own items at the top of the right-click menu opened from a card: "Fixar" /
    /// "Desafixar" (the menu's only pin item: the person's own pin items are left out under it,
    /// `context_menu::card_rows`) and, for a card without video, "Ocultar este card" and "Ocultar
    /// todos sem vídeo". `None`: the menu was not opened from one of this person's cards;
    /// `Some(true)`: an item was clicked (the caller closes the menu).
    pub(crate) fn card_menu_section(&mut self, ui: &mut egui::Ui, user: &User, _lead: Lead) -> Option<bool> {
        let (key, video) = self.call.menu_card.filter(|(k, _)| k.user() == user.id)?;
        let hide = Hidden::can_hide(key, video);
        let mut done = false;
        let text = if self.is_pinned(key) { "Desafixar" } else { "Fixar" };
        if ui_kit::menu_row(ui, ph::PUSH_PIN, text, false).clicked() {
            self.toggle_pin(key);
            done = true;
        }
        if hide {
            self.call.card_menu_drawn = Some(key);
            if ui_kit::menu_row(ui, ph::EYE_SLASH, "Ocultar este card", false).clicked() {
                self.hide_card(key.user());
                done = true;
            }
            if ui_kit::menu_row(ui, ph::USERS_THREE, "Ocultar todos sem vídeo", false).clicked() {
                self.hide_cards_without_video();
                done = true;
            }
        }
        ui_kit::menu_separator(ui);
        Some(done)
    }

    /// The viewer key of `id`'s camera when it is on (mine: the local preview).
    pub fn camera_key_of(&self, id: UserId) -> Option<String> {
        if self.my_id() == Some(id) {
            self.camera_on().then(|| SELF_CAMERA_KEY.to_string())
        } else {
            let identity = id.to_string();
            // (A camera I hid for myself is not received nor drawn.)
            (self.voice.cameras.contains(&identity) && !self.camera_hidden_for_me(id)).then(|| camera_key(&identity))
        }
    }

    /// Every camera tile key that can be on screen now.
    fn camera_keys(&self) -> Vec<String> {
        let hidden = |i: &String| i.parse::<UserId>().is_ok_and(|u| self.camera_hidden_for_me(u));
        let mut keys: Vec<String> = self.voice.cameras.iter().filter(|i| !hidden(i)).map(|i| camera_key(i)).collect();
        if self.camera_on() {
            keys.push(SELF_CAMERA_KEY.to_string());
        }
        if self.camera_window_preview() && !self.camera_on() {
            keys.push(vox_client_core::golive::camera::PREVIEW_KEY.to_string());
        }
        keys
    }

    // ------------------------------------------------------------ video placement

    /// After everything is drawn: create/drop the child windows, attach them to their streams
    /// and put each where its tile is (or hide it).
    pub fn sync_video(&mut self, ctx: &egui::Context) {
        if self.call.fullscreen_off_pending {
            self.call.fullscreen_off_pending = false;
            self.set_fullscreen(ctx, false);
        }
        let watched: Vec<String> = if self.screen == crate::app::Screen::Main { self.watching_video() } else { Vec::new() };
        let mut keys = watched.clone();
        if self.screen == crate::app::Screen::Main {
            keys.extend(self.camera_keys());
            if self.my_preview_live() {
                keys.push(vox_client_core::golive::SELF_SHARE_KEY.to_string());
            }
        }
        if self.call.fullscreen {
            let ok = self.call.fullscreen_key.as_ref().is_some_and(|k| watched.contains(k) && !self.stream_popped(k));
            if !ok || self.center != Center::Call {
                self.set_fullscreen(ctx, false);
            }
        }
        // The stage fullscreen only lasts while the stage is what the center shows.
        if self.stage_fullscreen && !(self.screen == crate::app::Screen::Main && self.voice.channel.is_some() && self.center == Center::Call) {
            self.set_stage_fullscreen(ctx, false);
        }
        if self.center != Center::Call && !self.call.fullscreen {
            // Only videos drawn inside a dialog (the Settings camera preview) stay.
            let layers = &self.video.want_layer;
            self.video.want.retain(|k, _| layers.contains_key(k));
        }
        // What egui draws over each video. macOS: egui draws the videos itself, under its own
        // popups, so nothing is cut out or hidden (the overlays are only known: logs, autotests).
        let overlays = self.video_clips(ctx);
        let clips = if cfg!(target_os = "macos") { HashMap::new() } else { overlays.clone() };
        // Only a video covered entirely is hidden (and the preview of my own live stops).
        let hidden: Vec<String> = clips.iter().filter(|(_, c)| **c == Clip::Hidden).map(|(k, _)| k.clone()).collect();
        self.sync_share_preview(ctx, &hidden);
        self.note_clips(&overlays);
        self.video.covered = !clips.is_empty();
        if self.video.covered {
            // Re-check soon: the overlay may close without any further input.
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        // Item 9: which watched streams are on screen (a dialog over one does not count as
        // hidden: it is brief, and pausing costs the streamer a keyframe), and which are small.
        self.video.shown = watched.iter().filter(|k| self.video.want.contains_key(*k)).cloned().collect();
        self.sync_strip_caps(&watched);
        self.sync_streams_visible(ctx, true);
        self.sync_focus_audio();
        #[cfg(windows)]
        self.place_hosts(ctx, &keys, &clips);
        #[cfg(target_os = "macos")]
        self.paint_videos(ctx, &keys);
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = keys;
        self.video.clips = overlays;
        self.video.want.clear();
        self.video.want_layer.clear();
        self.video.strip.clear();
    }

    /// Airspace: what the egui windows, menus, popups and tooltips drawn this frame do to each
    /// video (only the videos something is over are listed). A video drawn inside an egui window
    /// (the Settings camera preview) is only affected by what is above that window.
    fn video_clips(&self, ctx: &egui::Context) -> HashMap<String, Clip> {
        if self.video.want.is_empty() {
            return HashMap::new();
        }
        let ppp = ctx.pixels_per_point();
        let own = &self.video.want_layer;
        // Back-to-front rank of every layer, so "above" also holds between two windows.
        let (overlays, own_rank) = ctx.memory(|m| {
            let visible = m.areas().visible_layer_ids();
            let mut overlays: Vec<(egui::LayerId, usize, Rect)> = Vec::new();
            let mut own_rank: HashMap<egui::LayerId, usize> = HashMap::new();
            for (i, l) in m.layer_ids().enumerate() {
                if own.values().any(|o| *o == l) {
                    own_rank.insert(l, i);
                }
                if matches!(l.order, egui::Order::Middle | egui::Order::Foreground | egui::Order::Tooltip) && visible.contains(&l) {
                    if let Some(r) = m.area_rect(l.id) {
                        overlays.push((l, i, r));
                    }
                }
            }
            (overlays, own_rank)
        });
        if overlays.is_empty() {
            return HashMap::new();
        }
        let mut out = HashMap::new();
        for (key, rect) in &self.video.want {
            let inside = own.get(key).map(|l| (*l, l.order, own_rank.get(l).copied()));
            let cuts: Vec<clip::Px> = overlays
                .iter()
                .filter(|(l, i, _)| match inside {
                    None => true,
                    Some((layer, order, rank)) => *l != layer && (l.order, Some(*i)) > (order, rank),
                })
                .map(|(_, _, r)| clip::cut_px(*r, clip::PAD, ppp))
                .collect();
            let c = clip::clip_for(to_px(*rect, ppp), &cuts);
            if c != Clip::Whole {
                out.insert(key.clone(), c);
            }
        }
        out
    }

    /// One log line per video when its clip state changes (clipped / unclipped / hidden).
    fn note_clips(&self, clips: &HashMap<String, Clip>) {
        let was = |k: &String| self.video.clips.get(k).map(Clip::state).unwrap_or("unclipped");
        for (k, c) in clips {
            if was(k) != c.state() {
                let cuts = match c {
                    Clip::Cut(r) => format!("{r:?}"),
                    _ => String::new(),
                };
                tracing::info!(key = %k, state = c.state(), cuts, "inline video clip");
            }
        }
        for k in self.video.clips.keys().filter(|k| !clips.contains_key(*k)) {
            // Still placed (the overlay closed), or gone from the screen altogether.
            if self.video.want.contains_key(k) {
                tracing::info!(key = %k, state = "unclipped", "inline video clip");
            }
        }
    }

    /// Watched streams in the strip are drawn at [`STRIP_FPS`]; the others at full rate.
    fn sync_strip_caps(&mut self, watched: &[String]) {
        let Ok(v) = Viewer::get() else { return };
        for k in watched {
            let Some(st) = v.state(k) else { continue };
            // A stream in its own window is drawn at full rate wherever its tile is.
            let cap = if self.video.strip.contains(k) && !st.popped_out { STRIP_FPS } else { 0 };
            if st.max_fps != cap {
                tracing::info!(key = %k, max_fps = cap, "watched stream drawing rate");
                v.set_max_fps(k, cap);
            }
        }
    }

    /// Tell the core which watched streams can be seen: inside the app (as of the last frame,
    /// and the window is not minimized) or in their own window. The others get paused at the
    /// SFU after a short grace. Also called from `logic()` (`from_ui == false`), which runs while
    /// minimized; a stream that was not drawn yet is left to the next `ui()` pass.
    pub fn sync_streams_visible(&mut self, ctx: &egui::Context, from_ui: bool) {
        // (Hidden in the tray counts as minimized: nobody sees the picture.)
        let minimized = self.notify.hidden || crate::app::window_minimized(ctx);
        // (An audio-only stream has no video to pause.)
        let watched = self.watching_video();
        self.video.visible_sent.retain(|k, _| watched.contains(k));
        for k in watched {
            if !from_ui && !self.video.visible_sent.contains_key(&k) {
                continue;
            }
            let inside = self.screen == crate::app::Screen::Main && !minimized && self.video.shown.contains(&k);
            let visible = inside || self.stream_in_own_window(&k);
            if self.video.visible_sent.get(&k) != Some(&visible) {
                tracing::info!(key = %k, visible, minimized, center = ?self.center, "watched stream visibility");
                self.video.visible_sent.insert(k.clone(), visible);
                self.backend.send(Cmd::StreamVisible { identity: k, visible });
            }
        }
    }

    /// "Ouvir só as transmissões fixadas": with several watched streams and some pinned (or one
    /// fullscreen), the others are silenced; otherwise every watched stream plays.
    fn sync_focus_audio(&mut self) {
        let video = self.watching_video();
        let me = self.my_id();
        let focus: Vec<&String> = if self.call.fullscreen {
            self.call.fullscreen_key.iter().filter(|f| self.is_watching(f)).collect()
        } else {
            video
                .iter()
                .filter(|k| k.parse::<UserId>().is_ok_and(|u| Some(u) != me && self.is_pinned(TileKey::Stream(u))))
                .collect()
        };
        // Only streams with a picture can be put aside: the sound of an audio-only live (the
        // music) is the whole point of it and keeps playing.
        let want: HashSet<String> = if !focus.is_empty() && self.settings.stream_focus_audio && self.voice.watching.len() > 1 {
            video.iter().filter(|k| !focus.contains(k)).cloned().collect()
        } else {
            HashSet::new()
        };
        if want == self.video.focus_muted_sent {
            return;
        }
        for k in want.difference(&self.video.focus_muted_sent) {
            self.backend.send(Cmd::StreamFocusMuted { identity: k.clone(), muted: true });
        }
        for k in self.video.focus_muted_sent.difference(&want) {
            if self.voice.watching.contains(k) {
                self.backend.send(Cmd::StreamFocusMuted { identity: k.clone(), muted: false });
            }
        }
        tracing::info!(silenced = ?want, "stream audio focus");
        self.video.focus_muted_sent = want;
    }

    #[cfg(windows)]
    fn place_hosts(&mut self, ctx: &egui::Context, keys: &[String], clips: &HashMap<String, Clip>) {
        let ppp = ctx.pixels_per_point();
        self.video.hosts.retain(|k, _| keys.contains(k));
        self.video.placed.retain(|k, _| keys.contains(k));
        for key in keys {
            if !self.video.hosts.contains_key(key) {
                let Some(parent) = self.video.parent else { continue };
                if self.video.host_failed {
                    continue;
                }
                match VideoHost::new(parent) {
                    Ok(h) => {
                        if let Ok(v) = Viewer::get() {
                            v.attach(key, Some(h.hwnd()));
                        }
                        self.video.hosts.insert(key.clone(), h);
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "inline video unavailable; streams open in their own window");
                        self.video.host_failed = true;
                        if let Ok(v) = Viewer::get() {
                            v.pop_out(key, true);
                        }
                        continue;
                    }
                }
            }
            let clip = clips.get(key);
            let covered = clip == Some(&Clip::Hidden);
            let rect = if covered { None } else { self.video.want.get(key).copied() };
            let px = rect.map(|r| to_px(r, ppp));
            let cuts: Vec<PxRect> = match clip {
                Some(Clip::Cut(c)) => c.iter().map(|&(x, y, w, h)| PxRect { x, y, w, h }).collect(),
                _ => Vec::new(),
            };
            let host = self.video.hosts.get_mut(key).expect("inserted above");
            if host.place_clipped(px.map(|(x, y, w, h)| PxRect { x, y, w, h }), &cuts) {
                match px {
                    Some(p) => {
                        tracing::debug!(key, x = p.0, y = p.1, w = p.2, h = p.3, ppp, "inline video placed");
                        self.video.placed.insert(key.clone(), p);
                    }
                    None => {
                        // Info (only on change): explains a tile that went blank. (Covered by an
                        // overlay is already in the "inline video clip" line.)
                        if !covered {
                            tracing::info!(key, center = ?self.center, members = self.members_here().len(), "inline video hidden");
                        }
                        self.video.placed.remove(key);
                    }
                }
            }
        }
    }

    /// The child window of `key`'s video as the system sees it: shown, and the bounding box of
    /// its region relative to it (`None`: no region). For the autotest.
    #[cfg(windows)]
    pub fn video_window_state(&self, key: &str) -> Option<(bool, Option<clip::Px>)> {
        let (shown, region) = self.video.hosts.get(key)?.window_state();
        Some((shown, region.map(|r| (r.x, r.y, r.w, r.h))))
    }

    /// Whether `key`'s video window shows at (`x`, `y`), relative to it: `None` when it has no
    /// region (or there is no such window). For the autotest.
    #[cfg(windows)]
    pub fn video_window_hit(&self, key: &str, x: i32, y: i32) -> Option<bool> {
        self.video.hosts.get(key)?.region_contains(x, y)
    }

    /// Before the app goes away: release the child windows while the viewer is still there.
    pub fn drop_video(&mut self) {
        #[cfg(windows)]
        self.video.hosts.clear();
    }
}

/// The hover text of a card.
fn card_hover(pinned: bool, stream: bool) -> &'static str {
    match (pinned, stream) {
        (false, false) => "Clique para fixar · botão direito: mais opções",
        (true, false) => "Clique para desafixar · botão direito: mais opções",
        (false, true) => "Clique para fixar · clique duplo: tela cheia · botão direito: mais opções",
        (true, true) => "Clique para desafixar · clique duplo: tela cheia · botão direito: mais opções",
    }
}

/// Height of a name tag.
fn tag_h(small: bool) -> f32 {
    if small {
        20.0
    } else {
        22.0
    }
}

/// The band under a video card's picture (where its name tag goes), and the tag's bottom-left
/// corner.
fn tag_band(card: Rect, small: bool) -> (Rect, Pos2) {
    let margin = if small { 5.0 } else { 8.0 };
    let h = tag_h(small) + 2.0 * margin;
    let band = Rect::from_min_max(Pos2::new(card.left() + CARD_INSET, card.bottom() - h), Pos2::new(card.right() - CARD_INSET, card.bottom()));
    (band, Pos2::new(card.left() + margin + 2.0, card.bottom() - margin))
}

/// Where a video card's picture goes: inside the speaking border, above the name tag.
fn video_body(card: Rect, small: bool) -> Rect {
    let (band, _) = tag_band(card, small);
    Rect::from_min_max(card.min + Vec2::splat(CARD_INSET), Pos2::new(card.right() - CARD_INSET, band.top()))
}

/// What a card's name tag shows, left to right.
struct Tag<'a> {
    /// The lamp-coloured pin of a pinned card.
    pin: bool,
    icon: Option<(&'static str, Color32)>,
    name: &'a str,
    /// The "ao vivo" tag's text.
    live: Option<&'static str>,
    /// Small state icons after the name (microphone off, deafened, camera).
    marks: [Option<(&'static str, Color32)>; 3],
    /// How many are watching (a live card).
    viewers: Option<usize>,
}

/// Paint a name tag with its bottom-left corner at `at`, at most `max_w` wide (the name is cut
/// with "…" first). Returns its rect.
fn paint_tag(painter: &egui::Painter, at: Pos2, max_w: f32, t: &Tag<'_>, small: bool) -> Rect {
    let h = tag_h(small);
    let icon_font = theme::body(if small { 13.0 } else { 14.0 });
    let name_font = theme::body(if small { 11.5 } else { 12.5 });
    let (pad, sp) = (7.0, 5.0);
    let lay = |s: &str, c: Color32| painter.layout_no_wrap(s.to_owned(), icon_font.clone(), c);
    let mut pin = t.pin.then(|| lay(ph::PUSH_PIN, theme::GIZ));
    let mut icon = t.icon.map(|(i, c)| lay(i, c));
    let mut live = t.live.map(|s| painter.layout_no_wrap(s.to_owned(), theme::semibold(10.5), Color32::WHITE));
    let mut marks = t.marks.map(|m| m.map(|(i, c)| lay(i, c)));
    let mut viewers = t.viewers.map(|n| painter.layout_no_wrap(format!("{} {n}", ph::EYE), name_font.clone(), theme::NEVOA));
    let width = |g: &Option<std::sync::Arc<egui::Galley>>, extra: f32| g.as_ref().map_or(0.0, |g| g.size().x + extra);
    let fixed_of = |pin: &Option<_>, icon: &Option<_>, live: &Option<_>, marks: &[Option<_>; 3], viewers: &Option<_>| {
        width(pin, sp) + width(icon, sp) + width(live, 10.0 + sp) + marks.iter().map(|m| width(m, sp)).sum::<f32>() + width(viewers, sp)
    };
    // I6, a small card: the extras give way (viewers, the pin, the icon, the marks, then "ao
    // vivo") so the name keeps a few letters and nothing runs past the card.
    const NAME_MIN: f32 = 28.0;
    let mut fixed = fixed_of(&pin, &icon, &live, &marks, &viewers);
    for step in 0..5 {
        if 2.0 * pad + fixed + NAME_MIN <= max_w {
            break;
        }
        match step {
            0 => viewers = None,
            1 => pin = None,
            2 => icon = None,
            3 => marks = [None, None, None],
            _ => live = None,
        }
        fixed = fixed_of(&pin, &icon, &live, &marks, &viewers);
    }
    let name_w = (max_w - 2.0 * pad - fixed).max(0.0);
    let name = crate::app::notify::fit_name(painter, t.name, name_font, theme::GIZ, name_w);
    let w = (2.0 * pad + fixed + name.size().x.min(name_w)).min(max_w).max(0.0);
    let rect = Rect::from_min_size(Pos2::new(at.x, at.y - h), Vec2::new(w, h));
    let p = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    p.rect_filled(rect, radius::ROW, theme::ETIQUETA);
    let cy = rect.center().y;
    let mut x = rect.left() + pad;
    let mut put = |g: std::sync::Arc<egui::Galley>| {
        let gw = g.size().x;
        p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, theme::GIZ);
        x += gw + sp;
    };
    if let Some(g) = pin {
        put(g);
    }
    if let Some(g) = icon {
        put(g);
    }
    put(name);
    if let Some(g) = live {
        let r = Rect::from_min_size(Pos2::new(x, cy - 8.0), Vec2::new(g.size().x + 10.0, 16.0));
        p.rect_filled(r, radius::TAG, theme::AO_VIVO);
        p.galley(r.center() - g.size() / 2.0, g, Color32::WHITE);
        x = r.right() + sp;
    }
    let mut put = |g: std::sync::Arc<egui::Galley>| {
        let gw = g.size().x;
        p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, theme::GIZ);
        x += gw + sp;
    };
    for g in marks.into_iter().flatten() {
        put(g);
    }
    if let Some(g) = viewers {
        put(g);
    }
    rect
}

/// A [`ui_kit::card_button`] centred on `at` (`primary`: the lamp). `disabled`: `Some(why)` greys
/// it out and says why on hover (a live in another voice room).
fn centered_card_button(ui: &mut egui::Ui, at: Pos2, primary: bool, size: Size, icon: Option<&str>, text: &str, disabled: Option<&str>) -> egui::Response {
    let h = if size == Size::Small { 26.0 } else { 32.0 };
    let r = Rect::from_center_size(at, Vec2::new(320.0, h));
    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Center)));
    match disabled {
        None => ui_kit::card_button(&mut c, primary, size, icon, text),
        Some(why) => c.add_enabled_ui(false, |ui| ui_kit::card_button(ui, primary, size, icon, text)).inner.on_disabled_hover_text(why),
    }
}

/// One of the strip's scroll arrows. True when clicked.
fn strip_arrow(ui: &mut egui::Ui, rect: Rect, icon: &str, enabled: bool, hover: &str) -> bool {
    let resp = ui.interact(rect, egui::Id::new(("strip_arrow", icon)), if enabled { Sense::click() } else { Sense::hover() });
    let fill = if enabled && resp.hovered() { theme::MOVEL_HOVER } else { theme::MOVEL };
    ui.painter().rect_filled(rect, radius::BUTTON, fill);
    let fg = if enabled { theme::GIZ } else { theme::NEVOA.gamma_multiply(0.4) };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, theme::body(18.0), fg);
    enabled && resp.on_hover_text(hover).clicked()
}

/// The card of an audio-only live in place of a picture: a music note and a line about the state
/// (listening, or the card is ours). The "ao vivo · só áudio" tag is in the name tag.
fn audio_card(painter: &egui::Painter, body: Rect, small: bool, state: &str) {
    let up = if small { 10.0 } else { 26.0 };
    painter.text(body.center() - Vec2::new(0.0, up), Align2::CENTER_CENTER, ph::MUSIC_NOTES, theme::body(if small { 22.0 } else { 44.0 }), theme::GIZ);
    if !small && !state.is_empty() {
        painter.text(body.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, state, theme::body(12.5), theme::NEVOA);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_dm_call_blames_the_other_app_only_when_known() {
        let old = e2ee_plain_call_note("Bia", true, false);
        assert!(old.contains("Bia usa uma versão antiga"), "{old}");
        // Bia has a device with keys: not her version (the caller's computer was not ready).
        assert_eq!(e2ee_plain_call_note("Bia", true, true), E2EE_PLAIN_CALL_GENERIC);
        // The device list was not read here: nothing is known about Bia.
        assert_eq!(e2ee_plain_call_note("Bia", false, false), E2EE_PLAIN_CALL_GENERIC);
    }

    #[test]
    fn at_most_four_streams_counting_the_ones_on_their_way() {
        assert_eq!(MAX_WATCHED, 4);
        assert!(watch_allowed(0, 0));
        assert!(watch_allowed(3, 0));
        assert!(watch_allowed(2, 1));
        assert!(!watch_allowed(4, 0));
        assert!(!watch_allowed(3, 1), "a pending one counts");
        assert!(!watch_allowed(0, 4));
        assert!(watch_limit_text().contains("4 transmissões"));
    }

    #[test]
    fn preview_size_follows_the_tile() {
        assert_eq!(preview_size_for(300), (480, 270));
        assert_eq!(preview_size_for(480), (480, 270));
        assert_eq!(preview_size_for(481), (640, 360));
        assert_eq!(preview_size_for(900), (960, 540));
        assert_eq!(preview_size_for(1000), (1280, 720));
        assert_eq!(preview_size_for(4000), (1280, 720));
    }

    #[test]
    fn points_to_pixels_round_edges() {
        let r = Rect::from_min_max(Pos2::new(10.2, 20.6), Pos2::new(110.4, 76.9));
        assert_eq!(to_px(r, 1.0), (10, 21, 100, 56));
        assert_eq!(to_px(r, 1.5), (15, 31, 151, 84));
        // Neighbours share an edge exactly at any scale.
        let a = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(33.3, 10.0));
        let b = Rect::from_min_max(Pos2::new(33.3, 0.0), Pos2::new(66.6, 10.0));
        for ppp in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let (ax, _, aw, _) = to_px(a, ppp);
            let (bx, _, _, _) = to_px(b, ppp);
            assert_eq!(ax + aw, bx);
        }
    }

    #[test]
    fn the_video_sits_inside_the_card_above_its_tag() {
        for small in [false, true] {
            let card = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(400.0, if small { 112.0 } else { 225.0 }));
            let body = video_body(card, small);
            let (band, at) = tag_band(card, small);
            assert!(card.contains_rect(body) && card.contains_rect(band));
            // Room for the 2-point speaking border on every side, and the tag under the picture.
            assert!(body.left() - card.left() >= theme::stroke::SPEAKING && card.right() - body.right() >= theme::stroke::SPEAKING);
            assert!(body.top() - card.top() >= theme::stroke::SPEAKING);
            assert!((body.bottom() - band.top()).abs() < 0.01 && at.y - tag_h(small) >= band.top() - 0.01 && at.y <= card.bottom());
            assert!(body.height() > card.height() * 0.6, "small={small}: the picture keeps most of the card");
        }
    }

    #[test]
    fn names_sort_without_case() {
        let mut v = ["bia", "Ana", "caio", "Álvaro", "BRUNO"];
        v.sort_by(|a, b| cmp_names(a, b));
        assert_eq!(v, ["Ana", "bia", "BRUNO", "caio", "Álvaro"]);
    }

    #[test]
    fn the_header_counts_read_well() {
        assert_eq!(stage_meta_text(1, 0, 0), "1 pessoa");
        assert_eq!(stage_meta_text(4, 2, 1), "4 pessoas · 2 em foco · 1 card oculto");
        assert_eq!(stage_meta_text(3, 0, 2), "3 pessoas · 2 cards ocultos");
    }
}
