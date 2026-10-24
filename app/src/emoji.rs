//! Colored emoji (v1.1, card I8). epaint draws fonts in one colour, so the bundled emoji font
//! (NotoEmoji) turns 😂 😮 😢 into grey blobs. The app ships Twemoji pictures instead (CC-BY 4.0,
//! `app/assets/emoji/LICENSE-Twemoji.txt`, THIRD-PARTY-NOTICES.md) and puts them over the
//! monochrome glyphs everywhere text is drawn: the reaction picker and chips, the quick reactions
//! of a message's menu, message text, the soundboard, the status, the shortcuts page, tooltips,
//! combo boxes, text fields.
//!
//! **Assets.** One packed atlas, `assets/emoji/atlas.png` ([`CELL`] px per emoji, a transparent
//! [`PAD`] around each so the mipmaps do not bleed, [`COLS`] per row), saved with one palette of
//! 256 colours and alpha shared by every picture (Twemoji's flat colours survive it; a truecolour
//! atlas was four times bigger, and a palette per picture costs more than it saves), and its
//! index, `assets/emoji/index.txt` (one emoji per line, in atlas order, named like Twemoji's files:
//! the code points in lowercase hex joined by '-', without U+FE0F, see [`key`]). One
//! `include_bytes!`, one decode, one texture instead of a hundred files. The set is every emoji the
//! UI offers (the reaction grid, the quick reactions, the soundboard and status picks, the default
//! sound emoji) plus the emoji people type most; anything else keeps the font. `pack_atlas` (an
//! ignored test below) rebuilds both files from Twemoji's 72×72 PNGs.
//!
//! **Drawing.** An egui [`Plugin`](egui::plugin::Plugin), installed once by `theme::apply`, looks
//! at the frame's finished shapes ([`ColorEmoji::output_hook`], after the UI ran and before
//! tessellation). Text is laid out exactly as before, with the font (so sizes, wrapping, hit boxes,
//! selection and copying do not change); in a text shape that has emoji with a picture, their
//! glyph quads are folded to nothing and the picture is drawn over the glyphs' place, right after
//! the text and with its clip rect. Where the pictures go is found once per layout (keyed by its
//! `Arc<LayoutJob>`, which egui keeps while the text does not change, also in the copies it makes
//! to fade or tint a text), and the folded copy of a text that is the same `Arc<Galley>` frame
//! after frame is kept, so a steady frame only looks up pointers. The
//! atlas is decoded and uploaded the first time an emoji is on screen (never at startup, never
//! again after). Rotated text and emoji without a picture (or asked as text with U+FE0E) keep the
//! font.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use eframe::egui::{self, emath::GuiRounding, epaint, Color32, Pos2, Rect, Vec2};
use epaint::{text::Galley, ClippedShape, Mesh, Shape, TextShape};

/// The packed pictures and their index (see the module docs).
const ATLAS_PNG: &[u8] = include_bytes!("../assets/emoji/atlas.png");
const INDEX: &str = include_str!("../assets/emoji/index.txt");

/// Pixels per emoji in the atlas: sharp up to about 24 points on a 2× display.
pub const CELL: u32 = 48;
/// Transparent margin around each emoji in the atlas (the smaller mip levels stay inside it).
pub const PAD: u32 = 8;
/// One atlas cell with its margins.
pub const STRIDE: u32 = CELL + 2 * PAD;
/// Cells per atlas row.
pub const COLS: u32 = 16;
/// The picture's side over a glyph, as a share of the font's line height (the monochrome glyph's
/// box): a little bigger than the letters, like an emoji in any chat.
const SIZE_OF_LINE: f32 = 0.92;
/// A laid-out text not seen for this many frames leaves the cache.
const KEEP_PASSES: u64 = 120;

// ---------------------------------------------------------------- names and sequences

/// The atlas name of an emoji: its code points in lowercase hex joined by '-', without U+FE0F
/// ("❤" and "❤️" are both `2764`; "❤️‍🔥" is `2764-200d-1f525`).
pub fn key(emoji: &str) -> String {
    let parts: Vec<String> = emoji.chars().filter(|c| *c != '\u{fe0f}').map(|c| format!("{:x}", c as u32)).collect();
    parts.join("-")
}

/// Twemoji's own file name (without ".png") for an emoji: like [`key`], but U+FE0F stays in a
/// ZWJ sequence ("❤️‍🔥" is `2764-fe0f-200d-1f525.png` there). The packer accepts both spellings.
#[cfg(test)]
pub fn twemoji_name(emoji: &str) -> String {
    let zwj = emoji.contains('\u{200d}');
    let parts: Vec<String> = emoji.chars().filter(|c| zwj || *c != '\u{fe0f}').map(|c| format!("{:x}", c as u32)).collect();
    parts.join("-")
}

/// The emoji named by an atlas key or a Twemoji file name (`None` for anything else).
#[cfg(test)]
pub fn from_key(key: &str) -> Option<String> {
    key.split('-').map(|h| u32::from_str_radix(h, 16).ok().and_then(char::from_u32)).collect()
}

/// A Twemoji file name (with or without U+FE0F) as an atlas key.
#[cfg(test)]
pub fn key_of_file_name(stem: &str) -> String {
    stem.split('-').filter(|p| *p != "fe0f").collect::<Vec<_>>().join("-")
}

fn is_regional(c: u32) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&c)
}

fn is_skin_tone(c: u32) -> bool {
    (0x1F3FB..=0x1F3FF).contains(&c)
}

/// Could `c` start (or follow a ZWJ in) an emoji? Generous on purpose: only a picture in the
/// atlas turns a sequence into an image, so a symbol caught here just stays text.
fn is_pictographic(c: u32) -> bool {
    matches!(c,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x2199 | 0x21A9..=0x21AA
        | 0x231A..=0x23FF | 0x24C2 | 0x25AA..=0x25FE | 0x2600..=0x27BF | 0x2934..=0x2935
        | 0x2B05..=0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0x1F000..=0x1FAFF)
}

/// How many chars of `chars` (from the start) form one emoji: a pictograph with its variation
/// selector, skin tone and tags, ZWJ-joined to the next one; a keycap ("1️⃣"); or a flag (two
/// regional indicators). 0 when it does not start with one.
pub fn sequence_len(chars: &[char]) -> usize {
    let cp = |i: usize| chars.get(i).map(|c| *c as u32);
    let Some(first) = cp(0) else { return 0 };
    if is_regional(first) {
        return if cp(1).is_some_and(is_regional) { 2 } else { 1 };
    }
    if matches!(chars[0], '0'..='9' | '#' | '*') {
        let i = if cp(1) == Some(0xFE0F) { 2 } else { 1 };
        return if cp(i) == Some(0x20E3) { i + 1 } else { 0 };
    }
    if !is_pictographic(first) {
        return 0;
    }
    let mut i = 1;
    loop {
        if matches!(cp(i), Some(0xFE0F | 0xFE0E)) {
            i += 1;
        }
        if cp(i).is_some_and(is_skin_tone) {
            i += 1;
        }
        while cp(i).is_some_and(|c| (0xE0020..=0xE007F).contains(&c)) {
            i += 1;
        }
        if cp(i) == Some(0x200D) && cp(i + 1).is_some_and(is_pictographic) {
            i += 2;
            continue;
        }
        return i;
    }
}

/// The atlas: key -> cell.
fn index() -> &'static HashMap<String, u16> {
    static INDEX_MAP: OnceLock<HashMap<String, u16>> = OnceLock::new();
    INDEX_MAP.get_or_init(|| index_keys().enumerate().map(|(i, k)| (k.to_string(), i as u16)).collect())
}

/// The atlas keys, in cell order.
fn index_keys() -> impl Iterator<Item = &'static str> {
    INDEX.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The atlas cell of one emoji sequence (`None`: not in the set, or asked as text with U+FE0E).
fn cell_of(seq: &str) -> Option<u16> {
    if seq.contains('\u{fe0e}') {
        return None;
    }
    index().get(&key(seq)).copied()
}

/// Is `s` exactly one emoji that is drawn in colour?
pub fn has_image(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    !chars.is_empty() && sequence_len(&chars) == chars.len() && cell_of(s).is_some()
}

/// The emoji of `text` that are drawn in colour: their byte ranges and atlas cells.
#[cfg(test)]
pub fn find(text: &str) -> Vec<(std::ops::Range<usize>, u16)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let only: Vec<char> = chars.iter().map(|(_, c)| *c).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < only.len() {
        let n = sequence_len(&only[i..]);
        if n == 0 {
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let end = chars.get(i + n).map_or(text.len(), |(b, _)| *b);
        if let Some(cell) = cell_of(&text[start..end]) {
            out.push((start..end, cell));
        }
        i += n;
    }
    out
}

/// Could this text have an emoji? (Every one of them is at U+2000 or above, so its UTF-8 starts
/// with 0xE2 or more; plain text with accents never does. ©/® only as keys of nothing here.)
fn maybe_emoji(text: &str) -> bool {
    text.bytes().any(|b| b >= 0xE2)
}

// ---------------------------------------------------------------- the galley pass

/// One picture to draw over a laid-out text, in the galley's coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Draw {
    rect: Rect,
    cell: u16,
    /// The glyphs' colour as painted (`PLACEHOLDER`: the text shape's fallback colour); only its
    /// alpha is used (fades, disabled or hidden text).
    color: Color32,
}

/// Where the pictures go in a laid-out text: found once per layout (egui keeps a layout's
/// `LayoutJob` in one `Arc` for as long as the text does not change, also in the copies it makes
/// to fade or tint a text), applied to each copy that is painted.
struct Plan {
    /// The picture's place and cell, and the glyph vertex whose colour gives its alpha.
    draws: Vec<(Rect, u16, (usize, usize))>,
    /// Glyph quads to fold away: (row, first vertex).
    folds: Vec<(usize, usize)>,
}

/// The chars behind each row's glyphs (one glyph per char). They come from the text: epaint gives
/// a variation selector's glyph the char before it (so "❤\u{fe0f}" reads "❤❤" in the glyphs, and
/// U+FE0E could not be told apart). Should the glyphs not match the text one to one (an elided
/// text lost its end to a '…' of its own, even when the count still matches; right-to-left text
/// gets extra glyphs), the glyphs' chars are used, with such a zero-width repeat read as U+FE0F.
fn row_chars(galley: &Galley) -> Vec<Vec<char>> {
    let text: Vec<char> = galley.job.text.chars().collect();
    let in_rows: usize = galley.rows.iter().map(|r| r.glyphs.len() + r.ends_with_newline as usize).sum();
    if !galley.elided && in_rows == text.len() {
        let mut at = 0;
        return galley
            .rows
            .iter()
            .map(|r| {
                let chars = text[at..at + r.glyphs.len()].to_vec();
                at += r.glyphs.len() + r.ends_with_newline as usize;
                chars
            })
            .collect();
    }
    galley
        .rows
        .iter()
        .map(|r| {
            r.glyphs
                .iter()
                .enumerate()
                .map(|(i, g)| if i > 0 && g.advance_width == 0.0 && r.glyphs[i - 1].chr == g.chr { '\u{fe0f}' } else { g.chr })
                .collect()
        })
        .collect()
}

/// Find the emoji of a laid-out text that have a picture; `None` when there are none.
fn plan(galley: &Galley) -> Option<Plan> {
    if !maybe_emoji(&galley.job.text) {
        return None;
    }
    let mut plan = Plan { draws: Vec::new(), folds: Vec::new() };
    for (ri, (row, chars)) in galley.rows.iter().zip(row_chars(galley)).enumerate() {
        let mut i = 0;
        while i < chars.len() {
            let n = sequence_len(&chars[i..]);
            if n == 0 {
                i += 1;
                continue;
            }
            let seq: String = chars[i..i + n].iter().collect();
            // (The glyph must be that emoji: text and glyphs out of step draw nothing.)
            if let Some(cell) = cell_of(&seq).filter(|_| row.glyphs[i].chr == chars[i]) {
                let glyphs = &row.glyphs[i..i + n];
                let first = glyphs[0];
                let left = first.pos.x;
                let right = glyphs.iter().map(|g| g.max_x()).fold(left, f32::max);
                let top = first.pos.y - first.font_ascent;
                let center = Pos2::new((left + right) / 2.0, top + first.font_height / 2.0) + row.pos.to_vec2();
                // As big as the line allows, but within the room the font gave the glyph (less
                // half a point a side): never over the next letter, and two emoji in a row or a
                // space before one keep a visible gap.
                let side = (first.font_height * SIZE_OF_LINE).min(right - left - 1.0);
                let mut color_at = None;
                for g in glyphs {
                    let v0 = g.first_vertex as usize;
                    if g.uv_rect.is_nothing() || v0 + 4 > row.visuals.mesh.vertices.len() {
                        continue;
                    }
                    color_at.get_or_insert((ri, v0));
                    plan.folds.push((ri, v0));
                }
                // (No glyph drew anything there, e.g. a zero-width repeat: nothing to cover.)
                if let Some(at) = color_at {
                    plan.draws.push((Rect::from_center_size(center, Vec2::splat(side)), cell, at));
                }
            }
            i += n;
        }
    }
    (!plan.draws.is_empty()).then_some(plan)
}

/// A copy of `galley` with the planned glyph quads folded to a point (nothing left to draw,
/// whatever their colour), and the pictures with the colours the glyphs had.
fn apply(plan: &Plan, galley: &Arc<Galley>) -> (Arc<Galley>, Vec<Draw>) {
    let vertex_color = |(r, v): (usize, usize)| galley.rows[r].visuals.mesh.vertices[v].color;
    let draws = plan.draws.iter().map(|&(rect, cell, at)| Draw { rect, cell, color: vertex_color(at) }).collect();
    let mut out = Arc::clone(galley);
    let g = Arc::make_mut(&mut out);
    for &(r, v0) in &plan.folds {
        let mesh = &mut Arc::make_mut(&mut g.rows[r].row).visuals.mesh;
        let p = mesh.vertices[v0].pos;
        for v in &mut mesh.vertices[v0..v0 + 4] {
            v.pos = p;
        }
    }
    (out, draws)
}

/// The uv rectangle of an atlas cell (the atlas is `size` pixels).
fn uv_of(cell: u16, size: Vec2) -> Rect {
    let (col, row) = (cell as u32 % COLS, cell as u32 / COLS);
    let min = egui::pos2((col * STRIDE + PAD) as f32 / size.x, (row * STRIDE + PAD) as f32 / size.y);
    Rect::from_min_size(min, egui::vec2(CELL as f32 / size.x, CELL as f32 / size.y))
}

// ---------------------------------------------------------------- the plugin

/// The decoded atlas, uploaded once.
struct Atlas {
    tex: egui::TextureHandle,
    size: Vec2,
}

/// The last painted copy of a layout and what it became (most texts are the very same `Arc`
/// frame after frame, in the same place and colour: nothing is redone or allocated for them).
struct Last {
    src: Arc<Galley>,
    out: Arc<Galley>,
    draws: Vec<Draw>,
    /// The pictures' mesh, and what it was made for: the text's (rounded) position, override and
    /// fallback colours, and opacity.
    mesh: Option<(MeshKey, Arc<Mesh>)>,
}

type MeshKey = (Vec2, Option<Color32>, Color32, f32);

struct Entry {
    /// Kept so the pointer used as the key cannot be reused by another layout while cached.
    _job: Arc<epaint::text::LayoutJob>,
    plan: Option<Plan>,
    last: Option<Last>,
    seen: u64,
}

/// Draws emoji in colour over every text of the frame (see the module docs).
#[derive(Default)]
pub struct ColorEmoji {
    /// `None`: not needed yet; `Some(None)`: the atlas failed to decode (the font stays).
    atlas: Option<Option<Atlas>>,
    /// Layouts that may have emoji, by `Arc<LayoutJob>` pointer.
    cache: HashMap<usize, Entry>,
    pass: u64,
    /// The atlas was uploaded during this output: its texture must go out with it.
    uploaded: bool,
    /// For the tests: how many times a layout was searched for emoji.
    planned: u64,
    /// Kept between frames so a steady frame allocates nothing: the pictures to insert, and the
    /// emptied shape list of the last frame that had some.
    extras: Vec<(usize, ClippedShape)>,
    spare: Vec<ClippedShape>,
}

/// Put [`ColorEmoji`] on a context (once; again is a no-op).
pub fn install(ctx: &egui::Context) {
    ctx.add_plugin(ColorEmoji::default());
}

/// For the autotest: whether the atlas is up, and how many layouts on screen lately have pictures.
pub fn stats(ctx: &egui::Context) -> Option<(bool, usize)> {
    ctx.with_plugin(|p: &mut ColorEmoji| (p.atlas.as_ref().is_some_and(Option::is_some), p.cache.values().filter(|e| e.plan.is_some()).count()))
}

/// Decode the atlas (an indexed PNG with transparency) into an egui image.
fn decode_atlas() -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(ATLAS_PNG, image::ImageFormat::Png).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

/// The atlas texture, decoded and uploaded on first use (`uploaded`: it was, just now).
fn atlas(slot: &mut Option<Option<Atlas>>, uploaded: &mut bool, ctx: &egui::Context) -> Option<(egui::TextureId, Vec2)> {
    if slot.is_none() {
        let loaded = decode_atlas().map(|img| {
            let size = egui::vec2(img.size[0] as f32, img.size[1] as f32);
            let opts = egui::TextureOptions { mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR };
            Atlas { tex: ctx.load_texture("concord-emoji", img, opts), size }
        });
        if loaded.is_none() {
            tracing::warn!("the emoji atlas did not decode; emoji stay monochrome");
        }
        *uploaded = loaded.is_some();
        *slot = Some(loaded);
    }
    slot.as_ref()?.as_ref().map(|a| (a.tex.id(), a.size))
}

impl ColorEmoji {
    /// A text shape: its emoji folded away and the pictures to draw after it (`None`: unchanged).
    fn text(&mut self, ctx: &egui::Context, t: &mut TextShape, ppp: f32) -> Option<Shape> {
        if t.angle != 0.0 || t.opacity_factor <= 0.0 || !maybe_emoji(&t.galley.job.text) {
            return None;
        }
        let pass = self.pass;
        let planned = &mut self.planned;
        let entry = self.cache.entry(Arc::as_ptr(&t.galley.job) as usize).or_insert_with(|| {
            *planned += 1;
            Entry { _job: Arc::clone(&t.galley.job), plan: plan(&t.galley), last: None, seen: pass }
        });
        entry.seen = pass;
        let p = entry.plan.as_ref()?;
        let last = match &mut entry.last {
            Some(l) if Arc::ptr_eq(&l.src, &t.galley) => l,
            last => {
                let (out, draws) = apply(p, &t.galley);
                last.insert(Last { src: t.galley.clone(), out, draws, mesh: None })
            }
        };
        let (tex, size) = atlas(&mut self.atlas, &mut self.uploaded, ctx)?;
        // (The tessellator puts the text on a whole pixel; the pictures follow it.)
        let origin = t.pos.round_to_pixels(ppp).to_vec2();
        let key = (origin, t.override_text_color, t.fallback_color, t.opacity_factor);
        if last.mesh.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut mesh = Mesh::with_texture(tex);
            for d in &last.draws {
                let c = match t.override_text_color {
                    Some(c) => c,
                    None if d.color == Color32::PLACEHOLDER => t.fallback_color,
                    None => d.color,
                };
                let alpha = c.a() as f32 / 255.0 * t.opacity_factor.min(1.0);
                if alpha <= 0.0 {
                    continue;
                }
                mesh.add_rect_with_uv(d.rect.translate(origin), uv_of(d.cell, size), Color32::WHITE.gamma_multiply(alpha));
            }
            last.mesh = Some((key, Arc::new(mesh)));
        }
        t.galley = last.out.clone();
        last.mesh.as_ref().map(|(_, m)| Shape::Mesh(m.clone()))
    }

    /// Any shape (texts inside a `Shape::Vec` get their pictures right after them).
    fn visit(&mut self, ctx: &egui::Context, shape: &mut Shape, ppp: f32) -> Option<Shape> {
        match shape {
            Shape::Text(t) => self.text(ctx, t, ppp),
            Shape::Vec(v) => {
                let mut i = 0;
                while i < v.len() {
                    if let Some(extra) = self.visit(ctx, &mut v[i], ppp) {
                        v.insert(i + 1, extra);
                        i += 1;
                    }
                    i += 1;
                }
                None
            }
            _ => None,
        }
    }

    /// The whole pass over a frame's shapes.
    fn process(&mut self, ctx: &egui::Context, output: &mut egui::FullOutput) {
        self.pass += 1;
        let ppp = output.pixels_per_point;
        let mut extras = std::mem::take(&mut self.extras);
        for (i, cs) in output.shapes.iter_mut().enumerate() {
            if let Some(shape) = self.visit(ctx, &mut cs.shape, ppp) {
                extras.push((i, ClippedShape { clip_rect: cs.clip_rect, shape }));
            }
        }
        if !extras.is_empty() {
            let mut old = std::mem::take(&mut output.shapes);
            let mut shapes = std::mem::take(&mut self.spare);
            shapes.reserve(old.len() + extras.len());
            let mut pending = extras.drain(..).peekable();
            for (i, cs) in old.drain(..).enumerate() {
                shapes.push(cs);
                while let Some((_, extra)) = pending.next_if(|(at, _)| *at == i) {
                    shapes.push(extra);
                }
            }
            output.shapes = shapes;
            self.spare = old;
        }
        self.extras = extras;
        if std::mem::take(&mut self.uploaded) {
            // The frame's texture changes were already collected: the atlas goes out with this
            // frame, before the meshes that use it.
            let delta = ctx.tex_manager().write().take_delta();
            output.textures_delta.append(delta);
        }
        let pass = self.pass;
        self.cache.retain(|_, e| pass - e.seen <= KEEP_PASSES);
    }
}

impl egui::plugin::Plugin for ColorEmoji {
    fn debug_name(&self) -> &'static str {
        "concord color emoji"
    }

    fn output_hook(&mut self, ctx: &egui::Context, output: &mut egui::FullOutput) {
        self.process(ctx, output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_twemoji_file_names() {
        // Single code points, with and without the emoji variation selector.
        assert_eq!(key("❤"), "2764");
        assert_eq!(key("❤\u{fe0f}"), "2764");
        assert_eq!(twemoji_name("❤\u{fe0f}"), "2764");
        assert_eq!(key("👍"), "1f44d");
        assert_eq!(key("✌"), "270c");
        // Skin tones are part of the name.
        assert_eq!(key("👍🏽"), "1f44d-1f3fd");
        assert_eq!(twemoji_name("👍🏽"), "1f44d-1f3fd");
        // ZWJ sequences: Twemoji keeps FE0F in the file name, the atlas key drops it.
        let fire_heart = "❤\u{fe0f}\u{200d}🔥";
        assert_eq!(twemoji_name(fire_heart), "2764-fe0f-200d-1f525");
        assert_eq!(key(fire_heart), "2764-200d-1f525");
        assert_eq!(key_of_file_name("2764-fe0f-200d-1f525"), key(fire_heart));
        assert_eq!(key("👨\u{200d}👩\u{200d}👧"), "1f468-200d-1f469-200d-1f467");
        assert_eq!(key("🤦🏽\u{200d}♀\u{fe0f}"), "1f926-1f3fd-200d-2640");
        // Keycaps and flags.
        assert_eq!(twemoji_name("1\u{fe0f}\u{20e3}"), "31-20e3");
        assert_eq!(key("🇧🇷"), "1f1e7-1f1f7");
        // Back again.
        assert_eq!(from_key("1f44d-1f3fd").as_deref(), Some("👍🏽"));
        assert_eq!(from_key("2764-fe0f-200d-1f525").as_deref(), Some(fire_heart));
        assert_eq!(from_key("zz"), None);
        for k in index_keys() {
            assert_eq!(key(&from_key(k).unwrap()), k, "the index name {k} is not canonical");
        }
    }

    #[test]
    fn sequences_in_text() {
        let len = |s: &str| sequence_len(&s.chars().collect::<Vec<_>>());
        assert_eq!(len("👍 ok"), 1);
        assert_eq!(len("❤\u{fe0f}!"), 2);
        assert_eq!(len("👍🏽x"), 2);
        assert_eq!(len("❤\u{fe0f}\u{200d}🔥."), 4);
        assert_eq!(len("👨\u{200d}👩\u{200d}👧"), 5);
        assert_eq!(len("🤦🏽\u{200d}♀\u{fe0f}"), 5);
        assert_eq!(len("1\u{fe0f}\u{20e3}"), 3);
        assert_eq!(len("1 apenas"), 0);
        assert_eq!(len("🇧🇷🇵🇹"), 2);
        assert_eq!(len("a😂"), 0);
        assert_eq!(len("…"), 0);
        // A joiner at the end is not part of it.
        assert_eq!(len("😂\u{200d}"), 1);

        // Byte ranges of the coloured ones; FE0E asks for text, unknown ones stay font.
        let t = "oi 😂😂 ❤\u{fe0f} e ❤\u{fe0e} 👍🏽 fim";
        let found: Vec<&str> = find(t).into_iter().map(|(r, _)| &t[r]).collect();
        assert_eq!(found, ["😂", "😂", "❤\u{fe0f}"]);
        assert!(find("sem emoji — só texto…").is_empty());
        assert!(has_image("🔊") && has_image("❤") && has_image("❤\u{fe0f}"));
        assert!(!has_image("") && !has_image("😂😂") && !has_image("a") && !has_image("❤\u{fe0e}"));
    }

    /// Every emoji the UI offers has a picture: the reaction grid, the quick reactions, the
    /// soundboard's picks and default, the shortcuts page's default and the status picks.
    #[test]
    fn every_offered_emoji_is_in_the_atlas() {
        let mut offered: Vec<&str> = crate::app::chat_extras::REACTION_GRID.to_vec();
        offered.extend(crate::app::chat_extras::quick_reactions(&[]));
        offered.extend(crate::app::chat_extras::QUICK_DEFAULTS);
        offered.extend(crate::soundboard::QUICK_EMOJI);
        offered.push(crate::soundboard::DEFAULT_EMOJI);
        // profile.rs `QUICK_EMOJI` (the status picks) and keybinds_ui's "🔊".
        offered.extend(["🎮", "💤", "📚", "🎧", "📺", "🔥", "❤", "😀", "😴", "🏠", "✈", "🎵", "🔊"]);
        // Shown in the UI text: the owner's crown on the Calladex card.
        offered.push("👑");
        let missing: Vec<&str> = offered.iter().copied().filter(|e| !has_image(e)).collect();
        assert!(missing.is_empty(), "no picture for {missing:?}");
    }

    #[test]
    fn the_index_matches_the_atlas() {
        let keys: Vec<&str> = index_keys().collect();
        assert!(keys.len() >= 60, "{} emoji", keys.len());
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate names in the index");
        assert_eq!(index().len(), keys.len());
        // A real PNG, the planned grid.
        assert_eq!(&ATLAS_PNG[..8], b"\x89PNG\r\n\x1a\n");
        let img = decode_atlas().expect("the atlas decodes");
        let rows = (keys.len() as u32).div_ceil(COLS);
        assert_eq!(img.size, [(COLS * STRIDE) as usize, (rows * STRIDE) as usize]);
        // Each cell has a picture, and its margins are clear.
        for cell in 0..keys.len() as u32 {
            let (x0, y0) = ((cell % COLS) * STRIDE, (cell / COLS) * STRIDE);
            let alpha = |x: u32, y: u32| img.pixels[(y as usize) * img.size[0] + x as usize].a();
            let inside = (PAD..PAD + CELL).flat_map(|y| (PAD..PAD + CELL).map(move |x| (x, y))).filter(|(x, y)| alpha(x0 + x, y0 + y) > 0).count();
            assert!(inside > (CELL * CELL / 8) as usize, "cell {cell} ({}) is nearly empty", keys[cell as usize]);
            for k in 0..STRIDE {
                for (x, y) in [(k, 0), (k, STRIDE - 1), (0, k), (STRIDE - 1, k)] {
                    assert_eq!(alpha(x0 + x, y0 + y), 0, "cell {cell} bleeds into its margin");
                }
            }
        }
    }

    /// The emoji add at most about 100 KB to the program: the bound is 100 KiB for the atlas and
    /// its index together.
    #[test]
    fn the_embedded_emoji_stay_small() {
        let total = ATLAS_PNG.len() + INDEX.len();
        println!("emoji: atlas {} bytes + index {} bytes, {} emoji", ATLAS_PNG.len(), INDEX.len(), index().len());
        assert!(total <= 100 * 1024, "the emoji grew to {total} bytes");
    }

    /// One frame with a label: its shapes and the textures it uploaded (handed over: epaint wants
    /// every texture change taken).
    struct Frame {
        shapes: Vec<ClippedShape>,
        uploaded: Vec<egui::TextureId>,
    }

    fn run_frame(ctx: &egui::Context, text: &'static str) -> Frame {
        run_faded(ctx, text, 1.0)
    }

    fn run_faded(ctx: &egui::Context, text: &'static str, opacity: f32) -> Frame {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 200.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            ui.multiply_opacity(opacity);
            ui.label(text);
        });
        let uploaded = out.textures_delta.set.iter().map(|(id, _)| *id).collect();
        out.textures_delta.clear();
        Frame { shapes: std::mem::take(&mut out.shapes), uploaded }
    }

    fn texts(shapes: &[ClippedShape]) -> Vec<&TextShape> {
        shapes.iter().filter_map(|c| if let Shape::Text(t) = &c.shape { Some(t) } else { None }).collect()
    }

    fn meshes(shapes: &[ClippedShape], tex: Option<egui::TextureId>) -> Vec<&Mesh> {
        shapes.iter().filter_map(|c| if let Shape::Mesh(m) = &c.shape { Some(&**m) } else { None }).filter(|m| tex.is_none_or(|t| m.texture_id == t)).collect()
    }

    #[test]
    fn a_frame_gets_the_pictures_over_its_emoji() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        // Plain text: untouched, and the atlas is not even decoded.
        let out = run_frame(&ctx, "nada aqui — só texto");
        assert!(meshes(&out.shapes, None).is_empty());
        assert!(ctx.with_plugin(|p: &mut ColorEmoji| p.atlas.is_none()).unwrap());

        let out = run_frame(&ctx, "oi 😂 e 👍🏽!");
        let tex = ctx.with_plugin(|p: &mut ColorEmoji| p.atlas.as_ref().and_then(|a| a.as_ref()).map(|a| a.tex.id())).flatten().expect("the atlas was loaded");
        // The atlas goes out with the frame that first uses it.
        assert!(out.uploaded.contains(&tex));
        let m = meshes(&out.shapes, Some(tex));
        assert_eq!(m.len(), 1);
        // One picture (😂; the skin-tone thumb is not in the set), right after its text.
        assert_eq!(m[0].vertices.len(), 4);
        let at = out.shapes.iter().position(|c| matches!(&c.shape, Shape::Mesh(mm) if mm.texture_id == tex)).unwrap();
        assert!(matches!(out.shapes[at - 1].shape, Shape::Text(_)));
        // The 😂 glyph's quad is folded; the others still draw; the picture sits where it was.
        let t = texts(&out.shapes)[0];
        let row = &t.galley.rows[0];
        let gi = row.glyphs.iter().position(|g| g.chr == '😂').unwrap();
        let g = row.glyphs[gi];
        let v0 = g.first_vertex as usize;
        let quad = &row.visuals.mesh.vertices[v0..v0 + 4];
        assert!(quad.iter().all(|v| v.pos == quad[0].pos), "the monochrome glyph still draws");
        let o = row.glyphs.iter().find(|g| g.chr == 'o').unwrap();
        let oq = &row.visuals.mesh.vertices[o.first_vertex as usize..o.first_vertex as usize + 4];
        assert!(oq.iter().any(|v| v.pos != oq[0].pos));
        let pic = m[0].calc_bounds();
        let glyph_x = t.pos.x + row.pos.x + g.pos.x + g.advance_width / 2.0;
        assert!((pic.center().x - glyph_x).abs() < 1.0, "{pic:?} vs {glyph_x}");
        assert!(pic.height() > 10.0 && pic.height() <= g.font_height);

        // The next frame reuses the cached work and uploads nothing again.
        let out = run_frame(&ctx, "oi 😂 e 👍🏽!");
        assert!(!out.uploaded.contains(&tex));
        assert_eq!(meshes(&out.shapes, Some(tex)).len(), 1);
        assert_eq!(ctx.with_plugin(|p: &mut ColorEmoji| p.cache.values().filter(|e| e.plan.is_some()).count()).unwrap(), 1);
        // A steady text keeps its very galley and pictures (nothing allocated for them).
        let again = run_frame(&ctx, "oi 😂 e 👍🏽!");
        let mesh_of = |f: &Frame| f.shapes.iter().find_map(|c| if let Shape::Mesh(m) = &c.shape { Some(m.clone()) } else { None }).unwrap();
        assert!(Arc::ptr_eq(&mesh_of(&out), &mesh_of(&again)));
        assert!(Arc::ptr_eq(&texts(&out.shapes)[0].galley, &texts(&again.shapes)[0].galley));
        let planned = || ctx.with_plugin(|p: &mut ColorEmoji| p.planned).unwrap();
        let searched = planned();

        // Faded (egui paints a fresh copy of the text each frame): the same plan, the picture
        // fades with the text, and the glyph stays folded in the copy.
        for _ in 0..3 {
            let out = run_faded(&ctx, "oi 😂 e 👍🏽!", 0.5);
            let m = meshes(&out.shapes, Some(tex));
            assert_eq!(m.len(), 1);
            let a = m[0].vertices[0].color.a();
            assert!((100..=160).contains(&a), "alpha {a}");
            let t = texts(&out.shapes)[0];
            let row = &t.galley.rows[0];
            let g = row.glyphs.iter().find(|g| g.chr == '😂').unwrap();
            let quad = &row.visuals.mesh.vertices[g.first_vertex as usize..g.first_vertex as usize + 4];
            assert!(quad.iter().all(|v| v.pos == quad[0].pos));
        }
        assert_eq!(planned(), searched, "a faded copy was searched again");
    }

    /// The pictures stay inside the room the font gave the emoji: they never cover the letters,
    /// dots or spaces around them; FE0F joins its emoji, FE0E keeps the font.
    #[test]
    fn pictures_keep_to_their_glyphs() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let text = "a ❤\u{fe0f} b ❤\u{fe0e} · c 😂😂 d 👍🔥!";
        let out = run_frame(&ctx, text);
        let tex = ctx.with_plugin(|p: &mut ColorEmoji| p.atlas.as_ref().and_then(|a| a.as_ref()).map(|a| a.tex.id())).flatten().unwrap();
        let t = texts(&out.shapes)[0];
        let row = &t.galley.rows[0];
        let m = meshes(&out.shapes, Some(tex));
        let pics: Vec<Rect> = m[0].vertices.chunks_exact(4).map(|q| Rect::from_points(&q.iter().map(|v| v.pos).collect::<Vec<_>>())).collect();
        // ❤️, 😂, 😂, 👍, 🔥 (not ❤︎).
        assert_eq!(pics.len(), 5, "{pics:?}");
        let origin = t.pos + row.pos.to_vec2();
        for g in row.glyphs.iter().filter(|g| g.chr.is_ascii_graphic() || g.chr == '·') {
            let r = Rect::from_min_size(origin + egui::vec2(g.pos.x, g.pos.y - g.font_ascent), egui::vec2(g.advance_width, g.font_height)).shrink2(egui::vec2(0.5, 0.0));
            for p in &pics {
                assert!(!p.intersects(r), "the picture at {p:?} covers {:?} at {r:?}", g.chr);
            }
        }
        for w in pics.windows(2) {
            assert!(w[0].right() <= w[1].left() + 1.5, "pictures overlap: {w:?}");
        }

        // Several lines: the glyphs still line up with the text (FE0E on the second line).
        let g = ctx.fonts_mut(|f| f.layout_no_wrap("um ❤\u{fe0f}\n❤\u{fe0e} 😂\n\ndois 👍".into(), egui::FontId::proportional(14.0), Color32::WHITE));
        let p = plan(&g).unwrap();
        let rows: Vec<usize> = p.draws.iter().map(|(r, _, _)| (r.center().y / g.rows[0].height()) as usize).collect();
        assert_eq!(p.draws.len(), 3, "{rows:?}");
        let keys: Vec<&str> = p.draws.iter().map(|(_, c, _)| index_keys().nth(*c as usize).unwrap()).collect();
        assert_eq!(keys, ["2764", "1f602", "1f44d"]);

        // Every picture of the atlas gets a glyph's room in the font (an emoji newer than the
        // font would get a zero-width glyph, and a picture too small to see).
        for k in index_keys() {
            let g = ctx.fonts_mut(|f| f.layout_no_wrap(from_key(k).unwrap(), egui::FontId::proportional(14.0), Color32::WHITE));
            let sides: Vec<f32> = plan(&g).map_or(vec![], |p| p.draws.iter().map(|d| d.0.width()).collect());
            assert!(sides.len() == 1 && sides[0] >= 12.0, "{k}: pictures {sides:?}");
        }
    }

    /// A text cut short with '…' keeps its '…': the emoji it lost is not drawn over it.
    #[test]
    fn an_elided_text_keeps_its_ellipsis() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        let font = egui::FontId::proportional(14.0);
        let room = ctx.fonts_mut(|f| f.layout_no_wrap("abc …".into(), font.clone(), Color32::WHITE)).size().x;
        let mut job = epaint::text::LayoutJob::simple_singleline("abc 😂".into(), font, Color32::WHITE);
        job.wrap = epaint::text::TextWrapping { max_width: room + 0.5, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
        let g = ctx.fonts_mut(|f| f.layout_job(job));
        let shown: String = g.rows.iter().flat_map(|r| r.glyphs.iter().map(|g| g.chr)).collect();
        assert_eq!((g.elided, shown.as_str()), (true, "abc …"));
        assert!(plan(&g).is_none());
    }

    /// Text from other people: nothing panics, and the pictures only ever replace emoji glyphs
    /// (right-to-left text, combining marks, lone joiners and selectors, long sequences,
    /// wrapping, a selection, other sizes and scales).
    #[test]
    fn untrusted_text_never_panics_or_misplaces() {
        let nasty = [
            "\u{fe0f}", "\u{200d}", "\u{1f3fd}", "\u{20e3}", "\u{0}😂\u{0}", "😂\u{200d}", "\u{200d}😂", "😂\u{fe0f}\u{fe0f}", "❤\u{fe0e}\u{fe0f}",
            "😂\u{301}\u{301} e\u{301}😂", "שלום 😂 עולם ❤\u{fe0f} مرحبا 👍 بالعالم", "مرحبا😂بالعالم", "\u{202e}😂 r 👍\u{202c}", "🇧🇷🇵 🇧 1\u{fe0f}\u{20e3}#\u{20e3}",
            "👨\u{200d}👩\u{200d}👧\u{200d}👦 🏳\u{fe0f}\u{200d}🌈 👍🏻👍🏿", "tab\t😂\r\n😂\r", "…😂…—“😂”", "漢字😂かな😂", "um 😂😂😂😂😂😂😂😂 dois 👍👍👍👍 três",
        ];
        let long = "😂\u{200d}".repeat(2000) + &"😂".repeat(2000);
        let is_emoji = |c: char| is_pictographic(c as u32) || is_regional(c as u32) || matches!(c, '\u{fe0f}' | '\u{200d}' | '\u{20e3}' | '0'..='9' | '#');
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx);
            let mut input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(300.0, 9000.0))), ..Default::default() };
            input.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
            let mut out = ctx.run_ui(input, |ui| {
                ui.set_max_width(120.0);
                for s in nasty {
                    ui.label(s);
                    ui.label(egui::RichText::new(s).size(30.0).underline().strikethrough().italics());
                }
                ui.label(long.as_str());
                let mut g = ui.fonts_mut(|f| f.layout_no_wrap("sel 😂 e 👍".into(), egui::FontId::proportional(14.0), Color32::WHITE));
                let range = egui::text_selection::CCursorRange::two(epaint::text::cursor::CCursor::new(2), epaint::text::cursor::CCursor::new(7));
                egui::text_selection::visuals::paint_text_selection(&mut g, ui.visuals(), &range, None);
                ui.painter().add(Shape::Vec(vec![Shape::Text(TextShape::new(egui::pos2(5.0, 5.0), g, Color32::WHITE).with_override_text_color(Color32::YELLOW))]));
            });
            out.textures_delta.clear();
            let tex = ctx.with_plugin(|p: &mut ColorEmoji| p.atlas.as_ref().and_then(|a| a.as_ref()).map(|a| a.tex.id())).flatten().unwrap();
            let flat: Vec<&Shape> = out.shapes.iter().flat_map(|c| if let Shape::Vec(v) = &c.shape { v.iter().collect() } else { vec![&c.shape] }).collect();
            let mut pictures = 0;
            for (i, s) in flat.iter().enumerate() {
                let Shape::Text(t) = s else { continue };
                let origin = t.pos.round_to_pixels(ppp).to_vec2();
                let mut folded = Vec::new();
                for row in &t.galley.rows {
                    for g in row.glyphs.iter().filter(|g| !g.uv_rect.is_nothing()) {
                        let q = &row.visuals.mesh.vertices[g.first_vertex as usize..g.first_vertex as usize + 4];
                        if q.iter().all(|v| v.pos == q[0].pos) {
                            assert!(is_emoji(g.chr), "folded {:?} in {:?}", g.chr, t.galley.job.text);
                            folded.push(g.logical_rect().translate(row.pos.to_vec2() + origin).expand(0.6));
                        }
                    }
                }
                if let Some(Shape::Mesh(m)) = flat.get(i + 1) {
                    for q in m.vertices.chunks_exact(4).filter(|_| m.texture_id == tex) {
                        let at = Rect::from_points(&q.iter().map(|v| v.pos).collect::<Vec<_>>()).center();
                        assert!(folded.iter().any(|f| f.contains(at)), "a picture at {at:?} off its glyph in {:?}", t.galley.job.text);
                        pictures += 1;
                    }
                }
            }
            assert!(pictures > 2000, "{pictures} pictures at {ppp}");
            assert!(!ctx.tessellate(out.shapes, ppp).is_empty());
        }
    }

    /// Rebuilds `app/assets/emoji/` from Twemoji's 72×72 PNGs (`VOX_TWEMOJI_DIR`: a folder of
    /// them, named as Twemoji names them, with or without FE0F): each checked (PNG signature,
    /// 72×72), scaled to [`CELL`] px (Lanczos, on premultiplied alpha so the edges keep their
    /// colour) and packed into the atlas in name order; the atlas is reduced to one palette (index
    /// 0 fully transparent, then up to 255 colours with alpha: exact when there are no more,
    /// NeuQuant otherwise) and saved as an indexed PNG, with the index.
    #[test]
    #[ignore = "rebuilds the emoji atlas from VOX_TWEMOJI_DIR"]
    fn pack_atlas() {
        use image::{imageops, Rgba, RgbaImage};
        let dir = std::path::PathBuf::from(std::env::var("VOX_TWEMOJI_DIR").expect("VOX_TWEMOJI_DIR"));
        let mut files: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "png"))
            .map(|p| (key_of_file_name(&p.file_stem().unwrap().to_string_lossy()), p))
            .collect();
        files.sort();
        files.dedup_by(|a, b| a.0 == b.0);
        let rows = (files.len() as u32).div_ceil(COLS);
        let (w, h) = (COLS * STRIDE, rows * STRIDE);
        let mut atlas = RgbaImage::new(w, h);
        for (i, (k, path)) in files.iter().enumerate() {
            assert!(from_key(k).is_some(), "{path:?} is not named by code points");
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{path:?} is not a PNG");
            assert_eq!(&bytes[12..16], b"IHDR", "{path:?}");
            let (pw, ph) = (u32::from_be_bytes(bytes[16..20].try_into().unwrap()), u32::from_be_bytes(bytes[20..24].try_into().unwrap()));
            assert_eq!((pw, ph), (72, 72), "{path:?}");
            let mut src = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap().to_rgba32f();
            for p in src.pixels_mut() {
                let a = p[3];
                *p = Rgba([p[0] * a, p[1] * a, p[2] * a, a]);
            }
            let small = imageops::resize(&src, CELL, CELL, imageops::FilterType::Lanczos3);
            let (x0, y0) = ((i as u32 % COLS) * STRIDE + PAD, (i as u32 / COLS) * STRIDE + PAD);
            for (x, y, p) in small.enumerate_pixels() {
                let a = p[3].clamp(0.0, 1.0);
                let px = if a < 0.5 / 255.0 {
                    Rgba([0, 0, 0, 0])
                } else {
                    let c = |v: f32| ((v / a).clamp(0.0, 1.0) * 255.0).round() as u8;
                    Rgba([c(p[0]), c(p[1]), c(p[2]), (a * 255.0).round() as u8])
                };
                atlas.put_pixel(x0 + x, y0 + y, px);
            }
        }
        // One palette: entry 0 fully transparent, then the colours.
        let rgba: Vec<[u8; 4]> = atlas.pixels().map(|p| p.0).collect();
        let mut distinct: Vec<[u8; 4]> = rgba.iter().copied().filter(|c| c[3] > 0).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let (palette, indices): (Vec<[u8; 4]>, Vec<u8>) = if distinct.len() <= 255 {
            let mut pal = vec![[0u8; 4]];
            pal.extend(&distinct);
            (pal, rgba.iter().map(|c| if c[3] == 0 { 0 } else { (distinct.binary_search(c).unwrap() + 1) as u8 }).collect())
        } else {
            let visible: Vec<u8> = rgba.iter().filter(|c| c[3] > 0).flatten().copied().collect();
            let nq = color_quant::NeuQuant::new(1, 255, &visible);
            let mut pal = vec![[0u8; 4]];
            pal.extend(nq.color_map_rgba().chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]));
            (pal, rgba.iter().map(|c| if c[3] == 0 { 0 } else { (nq.index_of(c) + 1) as u8 }).collect())
        };
        let encode = |filter: png::Filter| {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Indexed);
            e.set_depth(png::BitDepth::Eight);
            e.set_palette(palette.iter().flat_map(|c| [c[0], c[1], c[2]]).collect::<Vec<u8>>());
            let mut trns: Vec<u8> = palette.iter().map(|c| c[3]).collect();
            while trns.last() == Some(&255) {
                trns.pop();
            }
            e.set_trns(trns);
            e.set_compression(png::Compression::High);
            e.set_filter(filter);
            let mut wr = e.write_header().unwrap();
            wr.write_image_data(&indices).unwrap();
            wr.finish().unwrap();
            out
        };
        let png = [png::Filter::NoFilter, png::Filter::Adaptive, png::Filter::MinEntropy].into_iter().map(encode).min_by_key(Vec::len).unwrap();
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/emoji");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("atlas.png"), &png).unwrap();
        let mut index = String::from(
            "# Twemoji 15.1.0 (https://github.com/jdecked/twemoji, tag v15.1.0, assets/72x72), (c) Twitter, Inc and\n\
             # other contributors, CC-BY 4.0 (LICENSE-Twemoji.txt). Packed by `emoji::tests::pack_atlas`.\n\
             # One emoji per line, in atlas order: code points in lowercase hex joined by '-', without FE0F.\n",
        );
        for (k, _) in &files {
            index.push_str(k);
            index.push('\n');
        }
        std::fs::write(out.join("index.txt"), &index).unwrap();
        println!("packed {} emoji ({} colours before the palette): atlas {} bytes ({w}x{h}), index {} bytes", files.len(), distinct.len(), png.len(), index.len());
    }
}
