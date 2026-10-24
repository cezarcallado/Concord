//! Go Live hover cards (v1.1): resting the pointer on someone's live (the "ao vivo" tag in the
//! sidebar, a live in the member list, a stream card not being watched) shows a small card with a
//! still picture of what they stream, the app or game ("Jogando Counter-Strike 2") and its icon.
//! It works for people outside that voice room too, and costs nothing while nobody hovers.
//!
//! The picture comes from the hub (`vox_proto::stream_preview`): asked only after the pointer rested
//! [`DEBOUNCE`] on a live, kept [`FRESH`] (asked again only while still hovering), and only from a
//! hub that announced the feature (an older hub: the plain tooltip as before). The store knows
//! nothing about the network: [`StreamCards::hover`] says what to ask, the app sends it, and the
//! backend's answer comes back through [`StreamCards::on_event`]; that keeps the rules testable.

use std::collections::HashMap;
use std::io::Cursor;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, ColorImage, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2};
use vox_client_core::hub::{StreamPreviewData, StreamPreviewReply};
use vox_proto::{User, UserId, STREAM_ICON_MAX_SIDE, STREAM_PREVIEW_MAX_SIDE};

use crate::app::App;
use crate::backend::Cmd;
use crate::theme::{self, ph, radius};

/// The pointer rests this long on a live before anything is asked.
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// v1.1 (E1): the hover card of a live in an end-to-end encrypted room (no picture is ever sent).
pub const E2EE_NO_PREVIEW: &str = "Prévia desligada: sala criptografada";
/// An answer is reused this long (then asked again, only if the pointer is still there).
pub const FRESH: Duration = Duration::from_secs(5);
/// A lookup without an answer for this long is asked again.
const LOST_AFTER: Duration = Duration::from_secs(10);
/// After "too many lookups", wait this long.
const BUSY_RETRY: Duration = Duration::from_secs(2);
/// Entries nobody hovered for this long are forgotten (textures freed).
const FORGET_AFTER: Duration = Duration::from_secs(60);
/// The card's width (points); the picture is 16:9 inside it.
const CARD_W: f32 = 264.0;
const PAD: f32 = 10.0;

/// The backend's answers.
#[derive(Debug)]
pub enum CardEv {
    /// The hub keeps hover previews (from its `Ready`).
    Supported(bool),
    Reply { user: UserId, reply: StreamPreviewReply },
}

/// What to ask the backend for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub user: UserId,
    /// The icon hash already cached (its bytes are then left out of the answer).
    pub have_icon: Option<String>,
}

#[derive(Debug)]
enum State {
    Loading,
    Ready(StreamPreviewData),
    /// Not live, nothing sent yet, or a room I may not see.
    Missing,
    Busy,
    Failed,
}

struct Entry {
    state: State,
    /// When the state was last set.
    at: Instant,
    last_hover: Instant,
    tex: Option<TextureHandle>,
    /// The picture's bytes the texture was made from.
    tex_of: usize,
}

/// A decoded icon (by hash).
struct Icon {
    png: Vec<u8>,
    tex: Option<TextureHandle>,
}

/// How a hovered live is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hover {
    /// Still within the debounce: nothing to show yet (repaint after this).
    Wait(Duration),
    /// Show the card; `Some`: ask the backend for this first.
    Show(Option<Request>),
}

#[derive(Default)]
pub struct StreamCards {
    supported: bool,
    entries: HashMap<UserId, Entry>,
    icons: HashMap<String, Icon>,
    /// (user, since, last frame) of the live under the pointer.
    hover: Option<(UserId, Instant, u64)>,
    /// Where each live was drawn last (user, place) -> (rect, frame): for the autotest's pointer.
    anchors: HashMap<(UserId, &'static str), (Rect, u64)>,
    /// Lookups asked so far (stats and the autotest).
    pub asked: u64,
}

impl StreamCards {
    pub fn supported(&self) -> bool {
        self.supported
    }

    pub fn on_event(&mut self, ev: CardEv, now: Instant) {
        match ev {
            CardEv::Supported(on) => {
                // Every hub snapshot (a reconnect, or another server whose user ids mean other
                // people) starts with nothing: no picture is ever shown for the wrong person.
                self.entries.clear();
                self.icons.clear();
                self.hover = None;
                self.supported = on;
            }
            CardEv::Reply { user, reply } => {
                let Some(e) = self.entries.get_mut(&user) else { return };
                e.at = now;
                e.state = match reply {
                    StreamPreviewReply::Ready(mut d) => {
                        if let (Some(h), Some(png)) = (d.icon_hash.clone(), d.icon_png.take()) {
                            self.icons.insert(h, Icon { png, tex: None });
                        }
                        State::Ready(d)
                    }
                    StreamPreviewReply::NotFound => State::Missing,
                    StreamPreviewReply::Busy => State::Busy,
                    StreamPreviewReply::Failed(why) => {
                        tracing::debug!(user, %why, "stream preview lookup failed");
                        State::Failed
                    }
                };
            }
        }
    }

    /// The pointer is on `user`'s live this frame (`frame`: the context's frame number). Cheap;
    /// call every frame it is.
    pub fn hover(&mut self, user: UserId, frame: u64, now: Instant) -> Hover {
        let since = match self.hover {
            Some((u, since, last)) if u == user && last + 1 >= frame => since,
            _ => now,
        };
        self.hover = Some((user, since, frame));
        let rested = now.duration_since(since);
        if rested < DEBOUNCE {
            return Hover::Wait(DEBOUNCE - rested);
        }
        self.sweep(now);
        let have_icon = self.entries.get(&user).and_then(|e| match &e.state {
            State::Ready(d) => d.icon_hash.clone().filter(|h| self.icons.contains_key(h)),
            _ => None,
        });
        let new = !self.entries.contains_key(&user);
        let e = self.entries.entry(user).or_insert_with(|| Entry { state: State::Loading, at: now, last_hover: now, tex: None, tex_of: 0 });
        e.last_hover = now;
        let age = now.duration_since(e.at);
        let due = new
            || match e.state {
                // (An answer that never came: ask again.)
                State::Loading => age >= LOST_AFTER,
                State::Busy => age >= BUSY_RETRY,
                _ => age >= FRESH,
            };
        if !due {
            return Hover::Show(None);
        }
        // The picture on screen stays while a fresher one comes.
        if !matches!(e.state, State::Ready(_)) {
            e.state = State::Loading;
        }
        e.at = now;
        self.asked += 1;
        Hover::Show(Some(Request { user, have_icon }))
    }

    fn sweep(&mut self, now: Instant) {
        self.entries.retain(|_, e| now.duration_since(e.last_hover) < FORGET_AFTER);
        let used: Vec<String> = self.entries.values().filter_map(|e| if let State::Ready(d) = &e.state { d.icon_hash.clone() } else { None }).collect();
        self.icons.retain(|h, _| used.contains(h));
    }

    /// `user`'s live was drawn at `rect` (`place`: which list) this frame.
    pub fn note_anchor(&mut self, user: UserId, place: &'static str, rect: Rect, frame: u64) {
        self.anchors.insert((user, place), (rect, frame));
    }

    /// Where `user`'s live was drawn in `place` (if it was drawn in the last few frames).
    pub fn anchor(&self, user: UserId, place: &'static str, frame: u64) -> Option<Rect> {
        self.anchors.get(&(user, place)).filter(|(_, f)| f + 3 >= frame).map(|(r, _)| *r)
    }

    /// What is known of `user`'s live: (picture size, app name, is a game, has an icon) when an
    /// answer with something in it arrived (the autotest's checks).
    pub fn known(&self, user: UserId) -> Option<(Option<(u32, u32)>, Option<String>, bool, bool)> {
        let e = self.entries.get(&user)?;
        let State::Ready(d) = &e.state else { return None };
        let size = d.jpeg.as_deref().and_then(jpeg_size);
        let has_icon = d.icon_hash.as_ref().is_some_and(|h| self.icons.contains_key(h));
        Some((size, d.app.as_ref().map(|a| a.name.clone()), d.app.as_ref().is_some_and(|a| a.is_game), has_icon))
    }

    /// The answer for `user` said there is no preview (or no access).
    pub fn missing(&self, user: UserId) -> bool {
        self.entries.get(&user).is_some_and(|e| matches!(e.state, State::Missing))
    }

    /// Ask for `user`'s preview without a hover (the autotest asks about a live it cannot see).
    pub fn probe(&mut self, user: UserId) -> Request {
        let now = Instant::now();
        self.entries.insert(user, Entry { state: State::Loading, at: now, last_hover: now, tex: None, tex_of: 0 });
        self.asked += 1;
        Request { user, have_icon: None }
    }

    /// Forget `user` (the autotest asks again from scratch).
    pub fn forget(&mut self, user: UserId) {
        self.entries.remove(&user);
    }

    /// The JPEG bytes and size of `user`'s picture, when one arrived (the autotest saves it).
    pub fn picture(&self, user: UserId) -> Option<&[u8]> {
        match &self.entries.get(&user)?.state {
            State::Ready(d) => d.jpeg.as_deref(),
            _ => None,
        }
    }
}

/// Width and height from a JPEG's header (no decode).
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::with_format(Cursor::new(b), image::ImageFormat::Jpeg).into_dimensions().ok()
}

/// Decode an untrusted picture of at most `max` pixels a side.
fn decode(bytes: &[u8], format: image::ImageFormat, max: u32) -> Option<ColorImage> {
    let (w, h) = image::ImageReader::with_format(Cursor::new(bytes), format).into_dimensions().ok()?;
    if w == 0 || h == 0 || w > max || h > max {
        return None;
    }
    let img = image::load_from_memory_with_format(bytes, format).ok()?.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

impl App {
    /// The live of `user` is drawn at `anchor` (`place`: "sidebar", "online", "card"); `hovered`:
    /// the pointer is on it. With a hub that keeps previews, a resting pointer gets the hover card
    /// (with `hint`, what a click does, at its foot) and `true` is returned; otherwise `false` (the
    /// caller shows its plain tooltip).
    pub fn stream_hover(&mut self, ui: &egui::Ui, place: &'static str, anchor: Rect, hovered: bool, user: &User, hint: Option<&str>) -> bool {
        if !self.stream_cards.supported() {
            return false;
        }
        let ctx = ui.ctx().clone();
        let frame = ctx.cumulative_frame_nr();
        self.stream_cards.note_anchor(user.id, place, anchor, frame);
        if !hovered {
            return true;
        }
        match self.stream_cards.hover(user.id, frame, Instant::now()) {
            Hover::Wait(left) => {
                ctx.request_repaint_after(left);
                return true;
            }
            Hover::Show(Some(r)) => self.backend.send(Cmd::StreamPreview { user: r.user, have_icon: r.have_icon }),
            Hover::Show(None) => {}
        }
        self.paint_stream_card(&ctx, anchor, user, hint);
        true
    }

    fn paint_stream_card(&mut self, ctx: &egui::Context, anchor: Rect, user: &User, hint: Option<&str>) {
        let cards = &mut self.stream_cards;
        let Some(e) = cards.entries.get_mut(&user.id) else { return };
        // The picture's texture (made once per picture).
        if let State::Ready(d) = &e.state {
            let len = d.jpeg.as_ref().map_or(0, Vec::len);
            if len > 0 && (e.tex.is_none() || e.tex_of != len) {
                e.tex = d.jpeg.as_deref().and_then(|j| decode(j, image::ImageFormat::Jpeg, STREAM_PREVIEW_MAX_SIDE)).map(|img| ctx.load_texture(format!("live-preview-{}", user.id), img, TextureOptions::LINEAR));
                e.tex_of = len;
            }
        }
        let loading = matches!(e.state, State::Loading);
        // v1.1 (E1): a live in an end-to-end encrypted room has no picture, on purpose.
        let e2ee_room = match &e.state {
            State::Ready(d) => self.channels.iter().any(|c| c.id == d.channel_id && c.e2ee),
            _ => false,
        };
        let tex = e.tex.clone().filter(|_| matches!(e.state, State::Ready(_)));
        let (app, icon_hash) = match &e.state {
            State::Ready(d) => (d.app.clone(), d.icon_hash.clone()),
            _ => (None, None),
        };
        let icon_tex = icon_hash.and_then(|h| {
            let icon = cards.icons.get_mut(&h)?;
            if icon.tex.is_none() {
                icon.tex = decode(&icon.png, image::ImageFormat::Png, STREAM_ICON_MAX_SIDE).map(|img| ctx.load_texture(format!("live-icon-{h}"), img, TextureOptions::LINEAR));
            }
            icon.tex.clone()
        });
        if loading {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // Beside the anchor (right, else left), kept on screen.
        let screen = ctx.content_rect();
        // The picture: 16:9 across the card's inner width.
        let pic_h = ((CARD_W - 2.0 * PAD) * 9.0 / 16.0).round();
        let card_h = PAD + 22.0 + pic_h + 8.0 + 24.0 + if hint.is_some() { 18.0 } else { 0.0 } + PAD - 4.0;
        let mut pos = Pos2::new(anchor.right() + 10.0, anchor.top() - 8.0);
        if pos.x + CARD_W > screen.right() - 4.0 {
            pos.x = anchor.left() - 10.0 - CARD_W;
        }
        pos.x = pos.x.clamp(screen.left() + 4.0, (screen.right() - CARD_W - 4.0).max(screen.left() + 4.0));
        pos.y = pos.y.clamp(screen.top() + 4.0, (screen.bottom() - card_h - 4.0).max(screen.top() + 4.0));
        let rect = Rect::from_min_size(pos, Vec2::new(CARD_W, card_h));
        egui::Area::new(egui::Id::new(("stream_card", user.id)))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos)
            .interactable(false)
            .show(ctx, |ui| {
                let (r, _) = ui.allocate_exact_size(rect.size(), Sense::hover());
                let p = ui.painter();
                p.rect_filled(r.expand(1.0), radius::POPUP, Color32::from_black_alpha(90));
                p.rect(r, radius::POPUP, theme::MOVEL, Stroke::new(1.0, theme::RISCO), StrokeKind::Inside);
                // Who, and "ao vivo".
                let mut y = r.top() + PAD;
                let live = crate::ui_kit::live_tag_size(p);
                let name_w = CARD_W - 2.0 * PAD - live.x - 8.0;
                let name = crate::app::notify::fit_name(p, user.shown_name(), theme::semibold(13.5), theme::GIZ, name_w);
                p.galley(Pos2::new(r.left() + PAD, y + (18.0 - name.size().y) / 2.0), name, theme::GIZ);
                crate::ui_kit::paint_live_tag(p, Rect::from_min_size(Pos2::new(r.right() - PAD - live.x, y + (18.0 - live.y) / 2.0), live));
                y += 22.0;
                // The picture (letterboxed in 16:9), or what there is instead.
                let pic = Rect::from_min_size(Pos2::new(r.left() + PAD, y), Vec2::new(CARD_W - 2.0 * PAD, pic_h));
                p.rect_filled(pic, radius::ROW, Color32::BLACK);
                match &tex {
                    Some(t) => {
                        let s = t.size_vec2();
                        let k = (pic.width() / s.x).min(pic.height() / s.y);
                        let shown = Rect::from_center_size(pic.center(), s * k);
                        p.image(t.id(), shown, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    None => {
                        let (glyph, line) = if loading {
                            (ph::MONITOR, "Carregando prévia…")
                        } else if e2ee_room {
                            (ph::LOCK_SIMPLE, E2EE_NO_PREVIEW)
                        } else {
                            (ph::MONITOR, "Sem prévia por enquanto")
                        };
                        p.text(pic.center() - Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, glyph, theme::body(26.0), theme::NEVOA);
                        p.text(pic.center() + Vec2::new(0.0, 18.0), Align2::CENTER_CENTER, line, theme::body(12.0), theme::NEVOA);
                    }
                }
                y = pic.bottom() + 8.0;
                // What is streamed: the app's icon (or a monitor), "Jogando <name>" for a game.
                let icon_rect = Rect::from_min_size(Pos2::new(r.left() + PAD, y + 2.0), Vec2::splat(20.0));
                match (&icon_tex, &app) {
                    (Some(t), _) => {
                        p.image(t.id(), icon_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    (None, Some(a)) if a.is_game => {
                        p.text(icon_rect.center(), Align2::CENTER_CENTER, ph::GAME_CONTROLLER, theme::body(18.0), theme::ABAJUR);
                    }
                    _ => {
                        p.text(icon_rect.center(), Align2::CENTER_CENTER, ph::MONITOR, theme::body(18.0), theme::NEVOA);
                    }
                }
                if let (Some(a), Some(_)) = (&app, &icon_tex) {
                    if a.is_game {
                        // The controller badge on the icon's corner.
                        let c = icon_rect.right_bottom() - Vec2::new(1.0, 1.0);
                        p.circle_filled(c, 7.5, theme::ABAJUR);
                        p.text(c, Align2::CENTER_CENTER, ph::GAME_CONTROLLER, theme::body(11.0), theme::ABAJUR_TINTA);
                    }
                }
                let text_x = icon_rect.right() + 8.0;
                let text_w = r.right() - PAD - text_x;
                let (lead, what) = match &app {
                    Some(a) if a.is_game => ("Jogando ", a.name.clone()),
                    Some(a) if a.screen => ("", "Tela inteira".to_string()),
                    Some(a) => ("", a.name.clone()),
                    None if loading => ("", String::new()),
                    None => ("", "Transmissão".to_string()),
                };
                let mut job = egui::text::LayoutJob::default();
                if !lead.is_empty() {
                    job.append(lead, 0.0, egui::TextFormat::simple(theme::body(13.0), theme::NEVOA));
                }
                job.append(&what, 0.0, egui::TextFormat::simple(theme::semibold(13.0), theme::GIZ));
                job.wrap.max_width = text_w;
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                job.wrap.overflow_character = Some('…');
                let g = p.layout_job(job);
                p.galley(Pos2::new(text_x, icon_rect.center().y - g.size().y / 2.0), g, theme::GIZ);
                y += 24.0;
                if let Some(h) = hint {
                    p.text(Pos2::new(r.left() + PAD, y + 9.0), Align2::LEFT_CENTER, h, theme::body(12.0), theme::NEVOA);
                }
            });
    }

    pub fn on_stream_card_event(&mut self, ev: CardEv) {
        self.stream_cards.on_event(ev, Instant::now());
        self.repaint_ctx().request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_proto::StreamApp;

    fn data(user: UserId, icon: Option<&str>) -> StreamPreviewData {
        StreamPreviewData {
            user_id: user,
            channel_id: 1,
            jpeg: Some(vec![0xFF, 0xD8, 0xFF]),
            age_ms: 0,
            app: Some(StreamApp { name: "Jogo".into(), is_game: true, screen: false }),
            icon_hash: icon.map(Into::into),
            icon_png: icon.map(|_| vec![1, 2, 3]),
        }
    }

    #[test]
    fn nothing_is_asked_before_the_pointer_rests() {
        let mut c = StreamCards::default();
        c.on_event(CardEv::Supported(true), Instant::now());
        let t0 = Instant::now();
        assert!(matches!(c.hover(7, 1, t0), Hover::Wait(_)));
        assert!(matches!(c.hover(7, 2, t0 + Duration::from_millis(100)), Hover::Wait(_)));
        // The pointer left (a frame without hover) and came back: the wait starts over.
        assert!(matches!(c.hover(7, 10, t0 + Duration::from_millis(300)), Hover::Wait(_)));
        // Rested long enough: ask once.
        let t1 = t0 + Duration::from_millis(300) + DEBOUNCE;
        assert_eq!(c.hover(7, 11, t1), Hover::Show(Some(Request { user: 7, have_icon: None })));
        assert_eq!(c.hover(7, 12, t1 + Duration::from_millis(16)), Hover::Show(None), "in flight");
        c.on_event(CardEv::Reply { user: 7, reply: StreamPreviewReply::Ready(data(7, Some("abc"))) }, t1 + Duration::from_millis(50));
        let k = c.known(7).unwrap();
        assert_eq!((k.1.as_deref(), k.2, k.3), (Some("Jogo"), true, true));
        // Fresh: nothing asked while it lasts...
        assert_eq!(c.hover(7, 13, t1 + Duration::from_secs(2)), Hover::Show(None));
        // ...then asked again (still hovering), saying which icon is cached.
        assert_eq!(c.hover(7, 14, t1 + Duration::from_millis(50) + FRESH), Hover::Show(Some(Request { user: 7, have_icon: Some("abc".into()) })));
        assert_eq!(c.asked, 2);
    }

    #[test]
    fn missing_busy_and_old_hubs() {
        let mut c = StreamCards::default();
        let t0 = Instant::now();
        // An older hub: never anything.
        assert!(!c.supported());
        c.on_event(CardEv::Supported(true), t0);
        c.hover(3, 1, t0);
        assert!(matches!(c.hover(3, 2, t0 + DEBOUNCE), Hover::Show(Some(_))));
        c.on_event(CardEv::Reply { user: 3, reply: StreamPreviewReply::NotFound }, t0 + DEBOUNCE);
        assert!(c.missing(3) && c.known(3).is_none());
        c.hover(4, 3, t0);
        assert!(matches!(c.hover(4, 4, t0 + DEBOUNCE), Hover::Show(Some(_))));
        c.on_event(CardEv::Reply { user: 4, reply: StreamPreviewReply::Busy }, t0 + DEBOUNCE);
        assert_eq!(c.hover(4, 5, t0 + DEBOUNCE + Duration::from_millis(500)), Hover::Show(None));
        assert!(matches!(c.hover(4, 6, t0 + DEBOUNCE + BUSY_RETRY), Hover::Show(Some(_))));
        // A reply for something nobody asked about is ignored.
        c.on_event(CardEv::Reply { user: 99, reply: StreamPreviewReply::NotFound }, t0);
        assert!(!c.missing(99));
        // Entries nobody hovers are forgotten.
        c.hover(5, 7, t0 + FORGET_AFTER * 2);
        c.hover(5, 8, t0 + FORGET_AFTER * 2 + DEBOUNCE);
        assert!(!c.missing(3));
        // Another snapshot (a reconnect, or another server with the same flag): everything is
        // dropped, so user ids of one server never show another's pictures.
        assert!(!c.entries.is_empty());
        c.on_event(CardEv::Supported(true), t0);
        assert!(c.supported() && c.entries.is_empty());
        c.hover(5, 9, t0);
        c.hover(5, 10, t0 + DEBOUNCE);
        c.on_event(CardEv::Supported(false), t0);
        assert!(!c.supported() && c.entries.is_empty());
    }

    #[test]
    fn pictures_are_decoded_within_limits() {
        let img = image::RgbImage::from_pixel(320, 180, image::Rgb([10, 20, 30]));
        let mut jpeg = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).unwrap();
        assert_eq!(jpeg_size(&jpeg), Some((320, 180)));
        assert_eq!(decode(&jpeg, image::ImageFormat::Jpeg, 640).unwrap().size, [320, 180]);
        assert!(decode(&jpeg, image::ImageFormat::Jpeg, 200).is_none(), "too large a side");
        assert!(decode(&[1, 2, 3], image::ImageFormat::Jpeg, 640).is_none());
    }
}
