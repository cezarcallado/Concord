//! Link-preview cards under chat messages (roadmap item 10).
//!
//! The hub does the fetching (see `hub/src/linkpreview`); this module keeps what it answered,
//! decides what to ask next, and draws the cards. The store knows nothing about the network or
//! the backend: [`PreviewStore::take_requests`] hands out what should be asked, and the app feeds
//! the answers back with [`PreviewStore::on_preview`] / [`PreviewStore::on_image`]. That keeps the
//! rules (queueing, retries, the "old hub" fallback, texture freeing) unit-testable.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Margin, RichText, Sense, TextureHandle, TextureOptions, Vec2};
use vox_client_core::hub::PreviewReply;
use vox_proto::LinkPreview;

/// Cards shown under one message.
pub const MAX_CARDS_PER_MESSAGE: usize = 2;
/// Requests in flight at once (previews and thumbnails together).
const MAX_IN_FLIGHT: usize = 3;
/// After an error, ask again no sooner than this...
const RETRY_FAILED: Duration = Duration::from_secs(120);
/// ...or this after "too many lookups" (the hub's allowance refills at 1 per 2 s).
const RETRY_BUSY: Duration = Duration::from_secs(8);
/// A texture nobody drew for this long is freed (the JPEG bytes stay, a few KB).
const TEXTURE_IDLE: Duration = Duration::from_secs(45);
/// Previews remembered per session; the least recently seen go first.
const MAX_ENTRIES: usize = 200;
/// Textures decoded per frame, so a screen full of new cards doesn't hitch.
const DECODES_PER_FRAME: u32 = 2;
/// Frames a chat must have been on screen before it asks for anything: the first frames lay the
/// list out from the top, and "visible" would then mean the oldest messages, not the newest.
const SETTLE_FRAMES: u32 = 3;

pub const CARD_MAX_W: f32 = 420.0;
const CARD_MARGIN: i8 = 10;
const ACCENT_W: f32 = 4.0;
/// The hub promises thumbnails of at most 400x225; a decoded picture bigger than this is refused.
const MAX_THUMB_SIDE: u32 = 800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Preview(String),
    Image(String),
}

enum State {
    /// Asked, no answer yet.
    Loading,
    /// The hub has nothing worth a card for this link.
    NoCard,
    /// Failed or busy; asked again once `at` has passed (and the link is on screen).
    Retry { at: Instant },
    Ready(LinkPreview),
}

enum Img {
    /// No thumbnail (or not wanted yet).
    None,
    Loading,
    Bytes(Vec<u8>),
    Failed,
}

struct Entry {
    state: State,
    img: Img,
    tex: Option<TextureHandle>,
    last_seen: Instant,
}

pub struct PreviewStore {
    entries: HashMap<String, Entry>,
    queue: VecDeque<Request>,
    in_flight: usize,
    /// The hub answered 404: it predates link previews. Nothing more is asked this session.
    unsupported: bool,
    /// (chat being shown, frames shown so far)
    view: Option<(i64, u32)>,
    decodes_left: u32,
    last_sweep: Option<Instant>,
}

impl Default for PreviewStore {
    fn default() -> Self {
        PreviewStore { entries: HashMap::new(), queue: VecDeque::new(), in_flight: 0, unsupported: false, view: None, decodes_left: DECODES_PER_FRAME, last_sweep: None }
    }
}

impl PreviewStore {
    /// A new hub session: forget everything (another server may answer differently).
    pub fn reset(&mut self) {
        *self = PreviewStore::default();
    }

    pub fn unsupported(&self) -> bool {
        self.unsupported
    }

    /// Call once per drawn frame of a chat. Returns whether the chat has settled enough to start
    /// asking for previews of what is on screen.
    pub fn tick_view(&mut self, chat: i64) -> bool {
        self.decodes_left = DECODES_PER_FRAME;
        match &mut self.view {
            Some((c, frames)) if *c == chat => *frames = frames.saturating_add(1),
            v => *v = Some((chat, 0)),
        }
        self.view.is_some_and(|(_, f)| f >= SETTLE_FRAMES)
    }

    /// Housekeeping, cheap to call every frame: sweeps at most every 5 seconds.
    pub fn maintain(&mut self, now: Instant) {
        if self.last_sweep.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5)) {
            self.last_sweep = Some(now);
            self.sweep(now);
        }
    }

    /// `url` is on screen: make sure it gets asked for. Cheap to call every frame.
    pub fn want(&mut self, url: &str, now: Instant) {
        if self.unsupported {
            return;
        }
        match self.entries.get_mut(url) {
            None => {
                self.entries.insert(url.to_string(), Entry { state: State::Loading, img: Img::None, tex: None, last_seen: now });
                self.queue.push_back(Request::Preview(url.to_string()));
            }
            Some(e) => {
                e.last_seen = now;
                if matches!(e.state, State::Retry { at } if now >= at) {
                    e.state = State::Loading;
                    self.queue.push_back(Request::Preview(url.to_string()));
                }
            }
        }
    }

    /// What to ask the hub right now (at most [`MAX_IN_FLIGHT`] outstanding in total).
    pub fn take_requests(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        while self.in_flight < MAX_IN_FLIGHT {
            let Some(r) = self.queue.pop_front() else { break };
            self.in_flight += 1;
            out.push(r);
        }
        out
    }

    pub fn on_preview(&mut self, url: &str, reply: PreviewReply, now: Instant) {
        self.in_flight = self.in_flight.saturating_sub(1);
        match reply {
            PreviewReply::Unsupported => {
                // An older hub: quietly no previews, and no further requests.
                self.unsupported = true;
                self.queue.clear();
                self.entries.retain(|_, e| !matches!(e.state, State::Loading));
            }
            reply => {
                let Some(e) = self.entries.get_mut(url).filter(|e| matches!(e.state, State::Loading)) else { return };
                match reply {
                    PreviewReply::Ready(p) if p.is_empty() => e.state = State::NoCard,
                    PreviewReply::Ready(p) => {
                        if p.has_image {
                            e.img = Img::Loading;
                            self.queue.push_back(Request::Image(url.to_string()));
                        }
                        e.state = State::Ready(p);
                    }
                    PreviewReply::Busy => e.state = State::Retry { at: now + RETRY_BUSY },
                    PreviewReply::Failed(_) | PreviewReply::Unsupported => e.state = State::Retry { at: now + RETRY_FAILED },
                }
            }
        }
    }

    pub fn on_image(&mut self, url: &str, bytes: Option<Vec<u8>>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if let Some(e) = self.entries.get_mut(url).filter(|e| matches!(e.img, Img::Loading)) {
            e.img = bytes.map_or(Img::Failed, Img::Bytes);
        }
    }

    /// The card to draw for `url`, if there is one; marks it as seen and (re)creates its texture
    /// when the thumbnail is there but the texture was freed.
    pub fn card(&mut self, ctx: &egui::Context, url: &str, now: Instant) -> Option<Card<'_>> {
        let e = self.entries.get_mut(url)?;
        e.last_seen = now;
        let State::Ready(p) = &e.state else { return None };
        if e.tex.is_none() && matches!(e.img, Img::Bytes(_)) {
            if self.decodes_left > 0 {
                self.decodes_left -= 1;
                let decoded = match &e.img {
                    Img::Bytes(bytes) => decode_thumb(bytes),
                    _ => None,
                };
                match decoded {
                    Some(img) => e.tex = Some(ctx.load_texture(format!("link-preview-{url}"), img, TextureOptions::LINEAR)),
                    None => e.img = Img::Failed,
                }
            } else {
                ctx.request_repaint(); // out of decode budget this frame: the rest next frame
            }
        }
        let image = match (&e.img, &e.tex) {
            (_, Some(t)) => ImageView::Texture(t),
            (Img::Loading | Img::Bytes(_), None) if p.image_w > 0 && p.image_h > 0 => ImageView::Placeholder(p.image_w, p.image_h),
            _ => ImageView::None,
        };
        Some(Card { preview: p, image })
    }

    /// Housekeeping, run now and then: free textures that were not drawn for a while and trim the
    /// store to [`MAX_ENTRIES`].
    pub fn sweep(&mut self, now: Instant) {
        for e in self.entries.values_mut() {
            if e.tex.is_some() && now.duration_since(e.last_seen) > TEXTURE_IDLE {
                e.tex = None;
            }
        }
        if self.entries.len() > MAX_ENTRIES {
            let mut idle: Vec<(Instant, String)> = self
                .entries
                .iter()
                .filter(|(_, e)| !matches!(e.state, State::Loading) && !matches!(e.img, Img::Loading))
                .map(|(k, e)| (e.last_seen, k.clone()))
                .collect();
            idle.sort();
            for (_, k) in idle.into_iter().take(self.entries.len() - MAX_ENTRIES) {
                self.entries.remove(&k);
            }
        }
    }

    /// (links remembered, cards ready to draw, textures alive) for the autotest log.
    pub fn stats(&self) -> (usize, usize, usize) {
        let ready = self.entries.values().filter(|e| matches!(e.state, State::Ready(_))).count();
        (self.entries.len(), ready, self.entries.values().filter(|e| e.tex.is_some()).count())
    }

    #[cfg(test)]
    fn has_texture(&self, url: &str) -> bool {
        self.entries.get(url).is_some_and(|e| e.tex.is_some())
    }
}

pub struct Card<'a> {
    pub preview: &'a LinkPreview,
    pub image: ImageView<'a>,
}

pub enum ImageView<'a> {
    None,
    /// The thumbnail is on its way: keep its space (pixel size) so nothing jumps when it lands.
    Placeholder(u32, u32),
    Texture(&'a TextureHandle),
}

/// Decode the hub's JPEG thumbnail. Anything but a small JPEG is refused.
pub fn decode_thumb(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    if img.width() == 0 || img.height() == 0 || img.width() > MAX_THUMB_SIDE || img.height() > MAX_THUMB_SIDE {
        return None;
    }
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

// ------------------------------------------------------------------ layout

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardLayout {
    /// Width of the whole card, margins included.
    pub card_w: f32,
    /// Width of the content column.
    pub inner_w: f32,
    /// Where the thumbnail goes (points), if the card has one.
    pub image: Option<Vec2>,
}

/// Size of a card in `avail_w` points of chat width, for a thumbnail of `image` pixels. The card
/// is at most [`CARD_MAX_W`] wide and shrinks with the window; the thumbnail keeps its aspect,
/// fills the column but is never enlarged past its own pixels.
pub fn layout(avail_w: f32, image: Option<(u32, u32)>) -> CardLayout {
    let card_w = avail_w.clamp(120.0, CARD_MAX_W);
    let inner_w = (card_w - 2.0 * f32::from(CARD_MARGIN) - ACCENT_W).max(60.0);
    let image = image.filter(|(w, h)| *w > 0 && *h > 0).map(|(w, h)| {
        let (w, h) = (w as f32, h as f32);
        let scale = (inner_w / w).min(1.0);
        Vec2::new((w * scale).round().max(1.0), (h * scale).round().max(1.0))
    });
    CardLayout { card_w, inner_w, image }
}

// ------------------------------------------------------------------ drawing

fn clamped_job(text: &str, size: f32, color: Color32, rows: usize) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(FontId::proportional(size), color));
    job.wrap.max_rows = rows;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    job
}

/// Draw one card. Clicking the title or the thumbnail opens `url` (through the same
/// http(s)-only opener as every other chat link).
pub fn show_card(ui: &mut egui::Ui, url: &str, card: &Card<'_>) {
    let p = card.preview;
    let natural = match &card.image {
        ImageView::Texture(t) => Some((t.size()[0] as u32, t.size()[1] as u32)),
        ImageView::Placeholder(w, h) => Some((*w, *h)),
        ImageView::None => None,
    };
    let lay = layout(ui.available_width(), natural);
    let visuals = ui.visuals().clone();
    let frame = egui::Frame::new()
        .fill(crate::theme::PAREDE)
        .stroke(egui::Stroke::new(crate::theme::stroke::HAIRLINE, crate::theme::RISCO))
        .corner_radius(crate::theme::radius::BUTTON)
        .inner_margin(Margin { left: CARD_MARGIN + ACCENT_W as i8, right: CARD_MARGIN, top: CARD_MARGIN - 2, bottom: CARD_MARGIN });
    let out = frame.show(ui, |ui| {
        ui.set_width(lay.inner_w);
        ui.spacing_mut().item_spacing.y = 3.0;
        if !p.site_name.is_empty() {
            ui.add(egui::Label::new(RichText::new(&p.site_name).small().weak()).truncate());
        }
        if !p.title.is_empty() {
            let job = clamped_job(&p.title, 14.5, visuals.hyperlink_color, 2);
            let r = ui.add(egui::Label::new(job).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.clicked() {
                crate::links::open_url(url);
            }
        }
        if !p.description.is_empty() {
            ui.add(egui::Label::new(clamped_job(&p.description, 13.0, visuals.weak_text_color(), 2)));
        }
        if let Some(size) = lay.image {
            ui.add_space(3.0);
            match &card.image {
                ImageView::Texture(t) => {
                    let r = ui
                        .add(egui::Image::new(egui::load::SizedTexture::new(t.id(), size)).corner_radius(6.0).sense(Sense::click()))
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if r.clicked() {
                        crate::links::open_url(url);
                    }
                }
                _ => {
                    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                    ui.painter().rect_filled(rect, 4.0, visuals.faint_bg_color);
                }
            }
        }
    });
    // The coloured bar down the left edge, as on Discord's embeds.
    let r = out.response.rect;
    // A right click on the card: its message's menu offers this link (Copiar link / Abrir link).
    if ui.rect_contains_pointer(r) && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary)) {
        crate::links::note_right_clicked(ui.ctx(), url);
    }
    let bar = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + ACCENT_W, r.max.y));
    ui.painter().rect_filled(bar, egui::CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 }, crate::theme::LINK.gamma_multiply(0.7));
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img).unwrap();
        out
    }

    fn preview(url: &str, title: &str, image: bool) -> LinkPreview {
        LinkPreview {
            url: url.into(),
            title: title.into(),
            description: "d".into(),
            site_name: "s".into(),
            has_image: image,
            image_w: if image { 400 } else { 0 },
            image_h: if image { 225 } else { 0 },
        }
    }

    fn ready_with_image(s: &mut PreviewStore, url: &str, now: Instant) {
        s.want(url, now);
        assert_eq!(s.take_requests(), vec![Request::Preview(url.into())]);
        s.on_preview(url, PreviewReply::Ready(preview(url, "T", true)), now);
        assert_eq!(s.take_requests(), vec![Request::Image(url.into())]);
        s.on_image(url, Some(jpeg(400, 225)));
    }

    #[test]
    fn card_width_follows_the_window_and_the_image_keeps_its_aspect() {
        let wide = layout(900.0, Some((400, 225)));
        assert_eq!(wide.card_w, CARD_MAX_W);
        let img = wide.image.unwrap();
        // Column = 420 - 2*10 - 4 = 396: the 400 px picture shrinks a hair to fit.
        assert_eq!(wide.inner_w, 396.0);
        assert!(img.x <= wide.inner_w && (img.x / img.y - 400.0 / 225.0).abs() < 0.02, "{img:?}");

        let narrow = layout(260.0, Some((400, 225)));
        assert_eq!(narrow.card_w, 260.0);
        assert!(narrow.image.unwrap().x <= narrow.inner_w);

        // A small picture is not blown up; tiny windows still get a usable card.
        assert_eq!(layout(900.0, Some((120, 80))).image, Some(Vec2::new(120.0, 80.0)));
        let tiny = layout(30.0, Some((400, 225)));
        assert!(tiny.card_w >= 120.0 && tiny.image.unwrap().x <= tiny.inner_w);
        assert_eq!(layout(500.0, None).image, None);
        assert_eq!(layout(500.0, Some((0, 10))).image, None);
        // Tall pictures keep their shape.
        let tall = layout(900.0, Some((100, 225))).image.unwrap();
        assert_eq!((tall.x, tall.y), (100.0, 225.0));
    }

    #[test]
    fn nothing_is_asked_twice_and_at_most_three_run_at_once() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        for i in 0..6 {
            s.want(&format!("https://a/{i}"), now);
            s.want(&format!("https://a/{i}"), now); // every frame
        }
        let first = s.take_requests();
        assert_eq!(first.len(), 3);
        assert!(s.take_requests().is_empty(), "the window is full");
        s.on_preview("https://a/0", PreviewReply::Ready(preview("https://a/0", "T", false)), now);
        assert_eq!(s.take_requests(), vec![Request::Preview("https://a/3".into())]);
    }

    #[test]
    fn a_ready_preview_then_asks_for_its_image() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        ready_with_image(&mut s, "https://a/x", now);
        let ctx = egui::Context::default();
        let c = s.card(&ctx, "https://a/x", now).unwrap();
        assert!(matches!(c.image, ImageView::Texture(_)));
        assert_eq!(c.preview.title, "T");
    }

    #[test]
    fn space_is_kept_for_a_thumbnail_that_is_still_loading() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        s.want("https://a/x", now);
        s.take_requests();
        let ctx = egui::Context::default();
        assert!(s.card(&ctx, "https://a/x", now).is_none(), "no card before the answer");
        s.on_preview("https://a/x", PreviewReply::Ready(preview("https://a/x", "T", true)), now);
        assert!(matches!(s.card(&ctx, "https://a/x", now).unwrap().image, ImageView::Placeholder(400, 225)));
        // The thumbnail fails to load: the card stays, without the picture.
        s.on_image("https://a/x", None);
        assert!(matches!(s.card(&ctx, "https://a/x", now).unwrap().image, ImageView::None));
    }

    #[test]
    fn empty_previews_make_no_card_and_no_image_request() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        s.want("https://a/x", now);
        s.take_requests();
        let nothing = LinkPreview { url: "https://a/x".into(), title: String::new(), description: String::new(), site_name: "a".into(), has_image: false, image_w: 0, image_h: 0 };
        s.on_preview("https://a/x", PreviewReply::Ready(nothing), now);
        assert!(s.take_requests().is_empty());
        assert!(s.card(&egui::Context::default(), "https://a/x", now).is_none());
        s.want("https://a/x", now + Duration::from_secs(3600));
        assert!(s.take_requests().is_empty(), "'nothing to show' is final for the session");
    }

    #[test]
    fn an_old_hub_means_no_previews_and_no_more_requests() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        for i in 0..5 {
            s.want(&format!("https://a/{i}"), now);
        }
        assert_eq!(s.take_requests().len(), 3);
        s.on_preview("https://a/0", PreviewReply::Unsupported, now); // the 404
        assert!(s.unsupported());
        assert!(s.take_requests().is_empty(), "queued lookups are dropped");
        s.want("https://a/9", now);
        s.on_preview("https://a/1", PreviewReply::Unsupported, now); // late replies of the first batch
        assert!(s.take_requests().is_empty());
        assert!(s.card(&egui::Context::default(), "https://a/9", now).is_none());
        // A different server later: asked again.
        s.reset();
        assert!(!s.unsupported());
        s.want("https://a/0", now);
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn failures_and_busy_hubs_are_retried_later_not_in_a_loop() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        s.want("https://a/x", now);
        s.take_requests();
        s.on_preview("https://a/x", PreviewReply::Busy, now);
        s.want("https://a/x", now + Duration::from_secs(2));
        assert!(s.take_requests().is_empty(), "too soon");
        s.want("https://a/x", now + RETRY_BUSY + Duration::from_secs(1));
        assert_eq!(s.take_requests().len(), 1);
        s.on_preview("https://a/x", PreviewReply::Failed("boom".into()), now + RETRY_BUSY);
        s.want("https://a/x", now + RETRY_BUSY + Duration::from_secs(60));
        assert!(s.take_requests().is_empty());
        s.want("https://a/x", now + RETRY_BUSY + RETRY_FAILED + Duration::from_secs(1));
        assert_eq!(s.take_requests().len(), 1);
    }

    #[test]
    fn the_chat_must_settle_before_it_asks() {
        let mut s = PreviewStore::default();
        let seen: Vec<bool> = (0..5).map(|_| s.tick_view(7)).collect();
        assert_eq!(seen, [false, false, false, true, true]);
        assert!(!s.tick_view(8), "another chat starts again");
        assert!(!s.tick_view(8));
        assert!(!s.tick_view(8));
        assert!(s.tick_view(8));
    }

    #[test]
    fn idle_textures_are_freed_and_come_back_from_the_kept_bytes() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        ready_with_image(&mut s, "https://a/x", now);
        let ctx = egui::Context::default();
        assert!(matches!(s.card(&ctx, "https://a/x", now).unwrap().image, ImageView::Texture(_)));
        assert!(s.has_texture("https://a/x"));
        s.sweep(now + Duration::from_secs(10));
        assert!(s.has_texture("https://a/x"), "recently drawn: kept");
        s.sweep(now + TEXTURE_IDLE + Duration::from_secs(1));
        assert!(!s.has_texture("https://a/x"), "scrolled far away: texture freed");
        // Scrolled back: the card is still there and the texture is rebuilt, with no new request.
        let later = now + TEXTURE_IDLE + Duration::from_secs(5);
        assert!(matches!(s.card(&ctx, "https://a/x", later).unwrap().image, ImageView::Texture(_)));
        assert!(s.take_requests().is_empty());
    }

    #[test]
    fn the_store_is_bounded() {
        let now = Instant::now();
        let mut s = PreviewStore::default();
        for i in 0..(MAX_ENTRIES + 50) {
            let u = format!("https://a/{i}");
            s.want(&u, now + Duration::from_millis(i as u64));
            s.on_preview(&u, PreviewReply::Ready(preview(&u, "T", false)), now);
        }
        s.sweep(now + Duration::from_secs(1));
        assert_eq!(s.entries.len(), MAX_ENTRIES);
        assert!(!s.entries.contains_key("https://a/0"), "the oldest went first");
        assert!(s.entries.contains_key(&format!("https://a/{}", MAX_ENTRIES + 49)));
    }

    #[test]
    fn thumbnails_are_checked_before_they_become_textures() {
        assert!(decode_thumb(&jpeg(400, 225)).is_some());
        assert!(decode_thumb(&jpeg(900, 100)).is_none(), "bigger than the hub ever sends");
        assert!(decode_thumb(b"<html>").is_none());
        assert!(decode_thumb(&[]).is_none());
    }
}
