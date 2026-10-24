//! The small components of the v1.0 design ([`crate::theme`]): buttons, sidebar rows, section
//! headers, pills and the "ao vivo" tag, avatars with the speaking ring, text fields, video-card,
//! popup and notice frames. Each is a plain function that paints with the theme's tokens; none
//! keeps state or allocates per frame beyond the text it lays out (egui caches the layouts).
//!
//! [`gallery`] shows every component (`--autotest kit`, for screenshots).

use eframe::egui::{self, Align2, Color32, CornerRadius, Frame, Margin, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType};

use crate::app::notify::{self, DrawnRow, PILL_H};
use crate::theme::{self, ph, radius, size, space, stroke};

// ---------------------------------------------------------------- buttons

/// What a button does, which decides its colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The one main action of a place (lamp fill, dark text).
    Primary,
    /// Everything else ([`theme::MOVEL`] fill).
    Secondary,
    /// No fill until hovered (toolbars, icon buttons).
    Ghost,
    /// Leaving / deleting ([`theme::AO_VIVO`] fill).
    Danger,
}

/// Button heights: compact (26), normal (32), the call dock (40).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    Normal,
    Large,
}

impl Size {
    fn height(self) -> f32 {
        match self {
            Size::Small => 26.0,
            Size::Normal => 32.0,
            Size::Large => 40.0,
        }
    }

    fn icon(self) -> f32 {
        match self {
            Size::Small => 15.0,
            Size::Normal => size::ICON,
            Size::Large => 19.0,
        }
    }
}

/// Fill and text colour of a button of `kind` (`hovered` / `pressed`).
pub fn button_colors(kind: Kind, hovered: bool, pressed: bool) -> (Color32, Color32) {
    let pick = |idle, hover, down| if pressed { down } else if hovered { hover } else { idle };
    match kind {
        Kind::Primary => (pick(theme::ABAJUR, theme::ABAJUR_HOVER, theme::ABAJUR_PRESSED), theme::ABAJUR_TINTA),
        Kind::Secondary => (pick(theme::MOVEL, theme::MOVEL_HOVER, theme::MOVEL_PRESSED), theme::GIZ),
        Kind::Ghost => (pick(Color32::TRANSPARENT, theme::MOVEL, theme::MOVEL_PRESSED), if hovered || pressed { theme::GIZ } else { theme::NEVOA }),
        Kind::Danger => (pick(theme::AO_VIVO, theme::AO_VIVO_HOVER, theme::AO_VIVO_PRESSED), Color32::WHITE),
    }
}

/// A button of the normal height: an optional Phosphor icon (`ph::...`) and a label (may be empty
/// for an icon-only button).
pub fn button(ui: &mut Ui, kind: Kind, icon: Option<&str>, text: &str) -> Response {
    button_sized(ui, kind, Size::Normal, icon, text)
}

/// [`button`] of any [`Size`].
pub fn button_sized(ui: &mut Ui, kind: Kind, sz: Size, icon: Option<&str>, text: &str) -> Response {
    paint_button(ui, sz, icon, text, |h, p| button_colors(kind, h, p), None, kind == Kind::Ghost)
}

/// Fill, text and border of a [`card_button`] (`primary`: the lamp; else a raised grey that stands
/// out on a card's [`theme::MOVEL`]).
pub fn card_button_colors(primary: bool, hovered: bool, pressed: bool) -> (Color32, Color32, Color32) {
    let pick = |idle, hover, down| if pressed { down } else if hovered { hover } else { idle };
    if primary {
        (pick(theme::ABAJUR, theme::ABAJUR_HOVER, theme::ABAJUR_PRESSED), theme::ABAJUR_TINTA, theme::ABAJUR_PRESSED)
    } else {
        (pick(theme::CAIXA, theme::CAIXA_HOVER, theme::MOVEL_PRESSED), theme::GIZ, theme::RISCO)
    }
}

/// A button placed on a video card or the stage ("Assistir transmissão", "Ouvir o áudio", "Parar
/// de ouvir"...): the card is [`theme::MOVEL`], the same as a [`Kind::Secondary`] button, which
/// would read as plain text there. `primary` is the lamp (the card's one action); otherwise a
/// raised grey. Both have a border, hover, pressed and disabled states and an optional icon.
pub fn card_button(ui: &mut Ui, primary: bool, sz: Size, icon: Option<&str>, text: &str) -> Response {
    let border = std::cell::Cell::new(theme::RISCO);
    paint_button(
        ui,
        sz,
        icon,
        text,
        |h, p| {
            let (fill, fg, line) = card_button_colors(primary, h, p);
            border.set(line);
            (fill, fg)
        },
        Some(&border),
        false,
    )
}

/// The one painter behind every text button: size, icon, label, colours by state, an optional
/// border (`border` is read after `colors` ran), the focus ring and the disabled look.
fn paint_button(ui: &mut Ui, sz: Size, icon: Option<&str>, text: &str, colors: impl Fn(bool, bool) -> (Color32, Color32), border: Option<&std::cell::Cell<Color32>>, ghost: bool) -> Response {
    let h = sz.height();
    let painter = ui.painter();
    let label = (!text.is_empty()).then(|| painter.layout_no_wrap(text.to_owned(), theme::semibold(size::BUTTON), Color32::PLACEHOLDER));
    let glyph = icon.map(|i| painter.layout_no_wrap(i.to_owned(), theme::body(sz.icon()), Color32::PLACEHOLDER));
    let gap = if label.is_some() && glyph.is_some() { space::M } else { 0.0 };
    let content = glyph.as_ref().map_or(0.0, |g| g.size().x) + gap + label.as_ref().map_or(0.0, |g| g.size().x);
    let pad = if label.is_some() { space::L } else { 0.0 };
    let w = (content + 2.0 * pad).max(h);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    if ui.is_rect_visible(rect) {
        let pressed = resp.is_pointer_button_down_on();
        let (mut fill, mut fg) = colors(resp.hovered(), pressed);
        let mut line = border.map(std::cell::Cell::get);
        if !ui.is_enabled() {
            // Disabled: every kind goes quiet the same way (readable, clearly not clickable).
            fill = if ghost { Color32::TRANSPARENT } else { theme::MOVEL_PRESSED };
            fg = theme::NEVOA.gamma_multiply(0.7);
            line = line.map(|_| theme::RISCO);
        }
        let painter = ui.painter();
        painter.rect_filled(rect, radius::BUTTON, fill);
        if let Some(l) = line {
            painter.rect_stroke(rect, radius::BUTTON, Stroke::new(stroke::HAIRLINE, l), StrokeKind::Inside);
        }
        if resp.has_focus() {
            painter.rect_stroke(rect, radius::BUTTON, Stroke::new(stroke::FOCUS, theme::GIZ), StrokeKind::Inside);
        }
        let mut x = rect.center().x - content / 2.0;
        if let Some(g) = glyph {
            let pos = Pos2::new(x, rect.center().y - g.size().y / 2.0);
            x += g.size().x + gap;
            painter.galley(pos, g, fg);
        }
        if let Some(g) = label {
            painter.galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, fg);
        }
    }
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), text));
    resp
}

/// How an icon button looks besides hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Quiet ([`theme::NEVOA`]).
    Normal,
    /// Switched on, shown open ([`theme::GIZ`] on [`theme::MOVEL`]).
    On,
    /// Something is off that is normally on (a muted microphone): [`theme::AO_VIVO`].
    Off,
}

/// A square ghost button with a Phosphor icon (toolbars, the user panel), 30 × 30.
pub fn icon_button(ui: &mut Ui, icon: &str, hover: &str, state: IconState) -> Response {
    let side = 30.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    if ui.is_rect_visible(rect) {
        let pressed = resp.is_pointer_button_down_on();
        let (fill, fg) = button_colors(Kind::Ghost, resp.hovered(), pressed);
        let fill = if state == IconState::On && fill == Color32::TRANSPARENT { theme::MOVEL } else { fill };
        let fg = match state {
            IconState::Off => theme::AO_VIVO,
            IconState::On => theme::GIZ,
            IconState::Normal => fg,
        };
        let fg = if ui.is_enabled() { fg } else { fg.gamma_multiply(0.5) };
        let painter = ui.painter();
        painter.rect_filled(rect, radius::ROW, fill);
        if resp.has_focus() {
            painter.rect_stroke(rect, radius::ROW, Stroke::new(stroke::FOCUS, theme::GIZ), StrokeKind::Inside);
        }
        painter.text(rect.center(), Align2::CENTER_CENTER, icon, theme::body(18.0), fg);
    }
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), hover));
    resp.on_hover_text(hover)
}

// ---------------------------------------------------------------- sidebar rows

/// Height of a sidebar row.
pub const ROW_H: f32 = 30.0;
/// Font of a sidebar row's name.
const ROW_FONT: f32 = 14.0;
/// Font of the counter in a pill.
const PILL_FONT: f32 = 11.0;

/// One row of the sidebar: what it shows and how.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    /// A Phosphor icon (`ph::HASH`, `ph::SPEAKER_HIGH`...).
    pub icon: &'a str,
    /// The name, cut with "…" when it does not fit.
    pub name: &'a str,
    /// How the row is known to the autotests and to screen readers (e.g. "# geral").
    pub label: &'a str,
    /// Open in the center (filled with [`theme::ROW_SELECTED`]).
    pub selected: bool,
    /// Has unread messages (strong name).
    pub unread: bool,
    /// The unread counter (0: none).
    pub count: u32,
    /// Unread mentions (an "@" before the counter).
    pub mentions: u32,
    pub hover: Option<&'a str>,
}

/// The counter pill's width for `unread`.
fn count_width(ui: &Ui, unread: u32) -> f32 {
    notify::pill_width(ui.painter().layout_no_wrap(notify::count_text(unread), theme::semibold(PILL_FONT), Color32::PLACEHOLDER).size().x)
}

/// A clickable sidebar row: icon, the name (truncated), and the counter with the "@" right-aligned
/// in their own space (`notify::row_layout`, so a badge never covers the name).
pub fn sidebar_row(ui: &mut Ui, row: Row<'_>) -> (Response, DrawnRow) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let count_w = (row.count > 0).then(|| count_width(ui, row.count));
    let icon_x = rect.left() + space::M;
    let layout = notify::row_layout(rect, icon_x + 18.0 + space::M, count_w, row.count > 0 && row.mentions > 0, 0.0);
    let painter = ui.painter_at(rect);
    if row.selected {
        painter.rect_filled(rect, radius::ROW, theme::ROW_SELECTED);
    } else if resp.hovered() {
        painter.rect_filled(rect, radius::ROW, theme::ROW_HOVER);
    }
    let strong = row.selected || row.unread || resp.hovered();
    let color = if strong { theme::GIZ } else { theme::NEVOA };
    painter.text(Pos2::new(icon_x, rect.center().y), Align2::LEFT_CENTER, row.icon, theme::body(size::ICON), color);
    let font = if row.unread { theme::semibold(ROW_FONT) } else { theme::body(ROW_FONT) };
    let g = notify::fit_name(&painter, row.name, font, color, layout.name.width());
    let elided = g.elided;
    let text_right = (layout.name.left() + g.size().x).min(layout.name.right());
    if layout.name.width() > 0.0 {
        let pos = Pos2::new(layout.name.left(), rect.center().y - g.size().y / 2.0);
        ui.painter().with_clip_rect(layout.name.intersect(ui.clip_rect())).galley(pos, g, color);
    }
    paint_badges(&painter, &layout, row.count);
    resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, row.selected, row.label));
    let resp = match (elided, row.hover) {
        (true, Some(h)) => resp.on_hover_text(format!("{}\n{h}", row.name)),
        (true, None) => resp.on_hover_text(row.name),
        (false, Some(h)) => resp.on_hover_text(h),
        (false, None) => resp,
    };
    (resp, DrawnRow { label: row.label.to_string(), layout, text_right, elided, selected: row.selected })
}

/// The counter (chalk pill) and the "@" (lamp pill) of a laid-out row.
pub fn paint_badges(painter: &egui::Painter, l: &notify::RowLayout, unread: u32) {
    if let Some(pill) = l.count {
        painter.rect_filled(pill, PILL_H / 2.0, theme::GIZ);
        painter.text(pill.center(), Align2::CENTER_CENTER, notify::count_text(unread), theme::semibold(PILL_FONT), theme::NOITE);
    }
    if let Some(at) = l.mention {
        painter.rect_filled(at, PILL_H / 2.0, theme::ABAJUR);
        painter.text(at.center(), Align2::CENTER_CENTER, "@", theme::semibold(PILL_FONT), theme::ABAJUR_TINTA);
    }
}

// ---------------------------------------------------------------- headers, pills, tags

/// A section header: sentence case, in the display font, quiet (never ALL CAPS).
pub fn section(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::display(size::SECTION)).color(theme::NEVOA)).selectable(false))
}

/// A small title (dialog parts, the home screen's list).
pub fn title(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::display(size::TITLE)).color(theme::GIZ)).selectable(false))
}

/// Size of a pill with `text` (height 17, round ends).
fn pill_galley(ui: &Ui, text: &str, fg: Color32, font: f32) -> (std::sync::Arc<egui::Galley>, Vec2) {
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::semibold(font), fg);
    let s = Vec2::new((g.size().x + 12.0).max(17.0), 17.0);
    (g, s)
}

/// A rounded pill (a counter, a role, a state).
pub fn pill(ui: &mut Ui, text: &str, fill: Color32, fg: Color32) -> Response {
    let (g, s) = pill_galley(ui, text, fg, PILL_FONT);
    let (rect, resp) = ui.allocate_exact_size(s, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, radius::PILL, fill);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    }
    resp
}

/// The "ao vivo" tag (white on [`theme::AO_VIVO`], small corners).
pub fn live_tag(ui: &mut Ui) -> Response {
    let (rect, resp) = ui.allocate_exact_size(live_tag_size(ui.painter()), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_live_tag(ui.painter(), rect);
    }
    resp
}

const LIVE_TEXT: &str = "ao vivo";
const LIVE_FONT: f32 = 10.5;

/// The size the "ao vivo" tag takes.
pub fn live_tag_size(painter: &egui::Painter) -> Vec2 {
    let g = painter.layout_no_wrap(LIVE_TEXT.to_owned(), theme::semibold(LIVE_FONT), Color32::WHITE);
    Vec2::new(g.size().x + 10.0, 16.0)
}

/// Paint the "ao vivo" tag in `rect` (see [`live_tag_size`]).
pub fn paint_live_tag(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, radius::TAG, theme::AO_VIVO);
    painter.text(rect.center(), Align2::CENTER_CENTER, LIVE_TEXT, theme::semibold(LIVE_FONT), Color32::WHITE);
}

// ---------------------------------------------------------------- avatars

/// The lamp ring of whoever speaks, around a round avatar of `radius` at `center`: a 2-point gap in
/// the background colour `bg`, then a 2-point [`theme::ABAJUR`] ring.
pub fn paint_speaking_ring(painter: &egui::Painter, center: Pos2, r: f32, bg: Color32) {
    paint_ring(painter, center, r, bg, theme::ABAJUR);
}

/// How far the speaking ring of an avatar of radius `r` reaches from its center (the gap, the ring
/// and a point for the anti-aliasing).
pub fn speaking_ring_reach(r: f32) -> f32 {
    r + stroke::RING_GAP + stroke::SPEAKING + 1.0
}

/// The square the speaking ring of an avatar of radius `r` at `center` needs (paint it with a clip
/// rect at least this big, or it is cut).
pub fn speaking_ring_rect(center: Pos2, r: f32) -> Rect {
    Rect::from_center_size(center, Vec2::splat(2.0 * speaking_ring_reach(r)))
}

/// A ring like the speaking one in another colour (the ringing window's caller, in chalk: the lamp
/// is only for whoever speaks).
pub fn paint_ring(painter: &egui::Painter, center: Pos2, r: f32, bg: Color32, color: Color32) {
    painter.circle_stroke(center, r + stroke::RING_GAP / 2.0, Stroke::new(stroke::RING_GAP, bg));
    painter.circle_stroke(center, r + stroke::RING_GAP + stroke::SPEAKING / 2.0, Stroke::new(stroke::SPEAKING, color));
}

/// A round avatar with an initial on a colour (no picture), `side` points wide, with the speaking
/// ring when `speaking` (the ring is drawn outside, over `bg`).
pub fn avatar_initial(ui: &mut Ui, side: f32, initial: &str, color: Color32, speaking: bool, bg: Color32) -> Response {
    let outer = side + 2.0 * (stroke::RING_GAP + stroke::SPEAKING);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(outer), Sense::hover());
    if ui.is_rect_visible(rect) {
        let c = rect.center();
        let p = ui.painter();
        p.circle_filled(c, side / 2.0, color);
        p.text(c, Align2::CENTER_CENTER, initial, theme::semibold(side * 0.42), theme::NOITE);
        if speaking {
            paint_speaking_ring(p, c, side / 2.0, bg);
        }
    }
    resp
}

// ---------------------------------------------------------------- scroll probes (autotests)

/// The key of the sidebar's channel list probe.
pub const SIDEBAR_SCROLL: &str = "sidebar";

/// What an autotest reads of a scroll area: its id, where it is on screen (to put the pointer
/// over it), how tall its content is, how much of it shows and where it is scrolled to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollProbe {
    pub id: egui::Id,
    pub rect: Rect,
    pub content_h: f32,
    pub view_h: f32,
    pub offset: f32,
}

impl ScrollProbe {
    pub fn of<R>(out: &egui::scroll_area::ScrollAreaOutput<R>) -> Self {
        ScrollProbe { id: out.id, rect: out.inner_rect, content_h: out.content_size.y, view_h: out.inner_rect.height(), offset: out.state.offset.y }
    }

    /// The content is taller than what shows: there is something to scroll.
    pub fn scrolls(&self) -> bool {
        self.content_h > self.view_h + 1.0
    }

    /// The furthest it scrolls.
    pub fn max_offset(&self) -> f32 {
        (self.content_h - self.view_h).max(0.0)
    }
}

/// Autotests only (the caller checks): remember what a scroll area showed this frame under `key`.
pub fn note_scroll<R>(ctx: &egui::Context, key: &str, out: &egui::scroll_area::ScrollAreaOutput<R>) {
    note_scroll_probe(ctx, key, ScrollProbe::of(out));
}

pub fn note_scroll_probe(ctx: &egui::Context, key: &str, probe: ScrollProbe) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(("scroll_probe", key)), probe));
}

/// What [`note_scroll`] saw last.
pub fn scroll_probe(ctx: &egui::Context, key: &str) -> Option<ScrollProbe> {
    ctx.data(|d| d.get_temp(egui::Id::new(("scroll_probe", key))))
}

/// Autotests: scroll the area noted under `key` by `delta` points (positive: towards the end),
/// as the mouse wheel would; done by [`apply_scroll_request`] the next time it is drawn.
pub fn scroll_request(ctx: &egui::Context, key: &str, delta: f32) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(("scroll_request", key)), delta));
}

/// Autotests only (the caller checks): inside the scroll area's content, do the scroll asked for.
pub fn apply_scroll_request(ui: &Ui, key: &str) {
    let id = egui::Id::new(("scroll_request", key));
    if let Some(delta) = ui.data_mut(|d| d.remove_temp::<f32>(id)) {
        ui.scroll_with_delta_animation(Vec2::new(0.0, -delta), egui::style::ScrollAnimation::none());
    }
}

// ---------------------------------------------------------------- fields, frames

/// A one-line text field in the theme's shape (32 high, roomy margins).
pub fn field<'t>(text: &'t mut dyn egui::TextBuffer) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).margin(Margin::symmetric(10, 7)).min_size(Vec2::new(0.0, 32.0))
}

/// The frame of a video card (radius 10, [`theme::MOVEL`]).
pub fn card_frame() -> Frame {
    Frame::new().fill(theme::MOVEL).corner_radius(radius::CARD)
}

/// Paint a video card's background and, while its person speaks, the 2-point lamp border inside it.
pub fn paint_card(painter: &egui::Painter, rect: Rect, speaking: bool) {
    painter.rect_filled(rect, radius::CARD, theme::MOVEL);
    if speaking {
        paint_card_speaking(painter, rect);
    }
}

/// Only the speaking border of a card (over a video).
pub fn paint_card_speaking(painter: &egui::Painter, rect: Rect) {
    painter.rect_stroke(rect, radius::CARD, Stroke::new(stroke::SPEAKING, theme::ABAJUR), StrokeKind::Inside);
}

/// The name strip at a card's bottom-left: translucent dark, an optional icon, the name and the
/// "ao vivo" tag when `live`. Returns its rectangle.
pub fn paint_card_label(painter: &egui::Painter, card: Rect, icon: Option<(&str, Color32)>, name: &str, live: bool) -> Rect {
    let font = theme::body(12.0);
    let name_g = painter.layout_no_wrap(name.to_owned(), font, theme::GIZ);
    let icon_g = icon.map(|(i, c)| (painter.layout_no_wrap(i.to_owned(), theme::body(14.0), c), c));
    let live_s = if live { live_tag_size(painter) } else { Vec2::ZERO };
    let mut w = 16.0 + name_g.size().x;
    if let Some((g, _)) = &icon_g {
        w += g.size().x + 6.0;
    }
    if live {
        w += live_s.x + 6.0;
    }
    let w = w.min(card.width() - 20.0).max(0.0);
    let strip = Rect::from_min_size(Pos2::new(card.left() + 10.0, card.bottom() - 10.0 - 22.0), Vec2::new(w, 22.0));
    let p = painter.with_clip_rect(strip.intersect(painter.clip_rect()));
    p.rect_filled(strip, radius::ROW, theme::ETIQUETA);
    let mut x = strip.left() + 8.0;
    let cy = strip.center().y;
    if let Some((g, c)) = icon_g {
        let gw = g.size().x;
        p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, c);
        x += gw + 6.0;
    }
    let nw = name_g.size().x;
    p.galley(Pos2::new(x, cy - name_g.size().y / 2.0), name_g, theme::GIZ);
    if live {
        x += nw + 6.0;
        paint_live_tag(&p, Rect::from_min_size(Pos2::new(x, cy - live_s.y / 2.0), live_s));
    }
    strip
}

/// A popup / menu frame: [`theme::MOVEL`], hairline outline, radius 10 and the only shadow in the app.
pub fn popup_frame(ui_style: &egui::Style) -> Frame {
    Frame::new()
        .fill(theme::MOVEL)
        .stroke(Stroke::new(stroke::HAIRLINE, theme::RISCO))
        .corner_radius(radius::POPUP)
        .inner_margin(Margin::same(6))
        .shadow(ui_style.visuals.popup_shadow)
}

/// One row of a menu: icon and text; `danger` rows are red and get a red tint under the pointer.
pub fn menu_row(ui: &mut Ui, icon: &str, text: &str, danger: bool) -> Response {
    menu_row_ex(ui, icon, None, text, danger, false)
}

/// [`menu_row`] with an icon colour of its own (a presence dot) and, for a submenu, a caret on the
/// right (`caret`; kept lit while the submenu is open).
pub fn menu_row_ex(ui: &mut Ui, icon: &str, icon_color: Option<Color32>, text: &str, danger: bool, caret: bool) -> Response {
    menu_row_full(ui, icon, icon_color, text, danger, caret, None)
}

/// [`menu_row`] with its keyboard shortcut quiet on the right ("Ctrl+V").
pub fn menu_row_hint(ui: &mut Ui, icon: &str, text: &str, hint: &str) -> Response {
    menu_row_full(ui, icon, None, text, false, false, Some(hint))
}

fn menu_row_full(ui: &mut Ui, icon: &str, icon_color: Option<Color32>, text: &str, danger: bool, caret: bool, hint: Option<&str>) -> Response {
    let w = ui.available_width().max(160.0);
    let id = ui.next_auto_id();
    let open = caret && egui::containers::menu::MenuState::from_ui(ui, |s, _| s.open_item == Some(egui::containers::menu::SubMenu::id_from_widget_id(id)));
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if resp.hovered() || open {
            p.rect_filled(rect, radius::ROW, if danger { theme::PERIGO_FUNDO } else { theme::MOVEL_HOVER });
        }
        let on = ui.is_enabled();
        let fg = if danger { theme::AO_VIVO } else { theme::GIZ };
        let fg = if on { fg } else { fg.gamma_multiply(0.5) };
        let icon_fg = match icon_color {
            Some(c) => c,
            None if danger => fg,
            None => theme::NEVOA,
        };
        let icon_fg = if on { icon_fg } else { icon_fg.gamma_multiply(0.5) };
        p.text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, icon, theme::body(size::ICON), icon_fg);
        p.text(Pos2::new(rect.left() + 34.0, rect.center().y), Align2::LEFT_CENTER, text, theme::body(13.5), fg);
        if caret {
            p.text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, ph::CARET_RIGHT, theme::body(14.0), theme::NEVOA);
        }
        if let Some(h) = hint {
            let c = if on { theme::NEVOA } else { theme::NEVOA.gamma_multiply(0.5) };
            p.text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, h, theme::body(12.0), c);
        }
    }
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), text));
    resp
}

/// A menu item that may be off: `why` (the reason, shown on hover) disables it. True when clicked.
pub fn menu_item(ui: &mut Ui, icon: &str, text: &str, danger: bool, why: Option<&str>) -> bool {
    let r = ui.add_enabled_ui(why.is_none(), |ui| menu_row(ui, icon, text, danger)).inner;
    match why {
        Some(w) => {
            r.on_disabled_hover_text(w);
            false
        }
        None => r.clicked(),
    }
}

/// A submenu entry (a caret on the right; the submenu opens beside it on hover or click), off with
/// a reason when `why` is set. `content` draws the submenu's rows.
pub fn menu_sub(ui: &mut Ui, icon: &str, text: &str, why: Option<&str>, content: impl FnOnce(&mut Ui)) {
    match why {
        None => {
            let r = menu_row_ex(ui, icon, None, text, false, true);
            egui::containers::menu::SubMenu::new().show(ui, &r, |ui| {
                ui.set_min_width(180.0);
                content(ui);
            });
        }
        Some(w) => {
            ui.add_enabled_ui(false, |ui| menu_row_ex(ui, icon, None, text, false, true)).inner.on_disabled_hover_text(w);
        }
    }
}

/// A choice in a submenu (a role, a status, an effect): a check in front of the current one.
pub fn menu_choice(ui: &mut Ui, current: bool, text: &str) -> Response {
    menu_row_ex(ui, if current { ph::CHECK } else { "" }, Some(theme::GIZ), text, false, false)
}

/// The style of the app's menus: egui's menu style, with submenus in the same [`theme::MOVEL`]
/// as [`popup_frame`] (a submenu's frame comes from its parent's `window_fill`).
pub fn menu_style(style: &mut egui::Style) {
    egui::containers::menu::menu_style(style);
    style.visuals.window_fill = theme::MOVEL;
    style.visuals.window_stroke = Stroke::new(stroke::HAIRLINE, theme::RISCO);
    style.spacing.item_spacing.y = 1.0;
    // egui's own widgets (a checkbox's box, a slider's rail) are filled with [`theme::MOVEL`], the
    // menu's own colour, so they vanished in a menu: a step down to the wall's colour instead.
    for w in [&mut style.visuals.widgets.inactive, &mut style.visuals.widgets.noninteractive] {
        w.bg_fill = theme::PAREDE;
        w.weak_bg_fill = theme::PAREDE;
    }
}

/// A small heading inside a menu (a section of the person menu: "Transmissão", "Moderação").
pub fn menu_heading(ui: &mut Ui, text: &str) {
    ui.add_space(space::S);
    ui.add(egui::Label::new(RichText::new(text).font(theme::semibold(12.0)).color(theme::NEVOA)).selectable(false));
    ui.add_space(space::XS);
}

/// A divider between the groups of a menu (a hairline with a little air).
pub fn menu_separator(ui: &mut Ui) {
    ui.add_space(space::XS);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(4.0), r.center().y, Stroke::new(stroke::HAIRLINE, theme::RISCO));
    ui.add_space(space::XS);
}

/// What a notice is about (its outline colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Warning,
    Error,
}

/// The frame of a toast / notice: [`theme::PAREDE`], radius 10, outlined by its kind.
pub fn notice_frame(kind: Notice) -> Frame {
    let (fill, line) = match kind {
        Notice::Info => (theme::PAREDE, theme::RISCO),
        Notice::Warning => (theme::PAREDE, theme::NEVOA),
        Notice::Error => (theme::PERIGO_FUNDO, theme::AO_VIVO),
    };
    Frame::new().fill(fill).stroke(Stroke::new(stroke::HAIRLINE, line)).corner_radius(radius::POPUP).inner_margin(Margin::symmetric(14, 10))
}

/// The sidebar's frame (a square [`theme::PAREDE`] panel, no outline).
pub fn side_panel_frame() -> Frame {
    Frame::new().fill(theme::PAREDE).inner_margin(Margin::symmetric(8, 8)).corner_radius(CornerRadius::same(radius::PANEL))
}

/// The icon and colour that go with a notice's kind.
pub fn notice_icon(kind: Notice) -> (&'static str, Color32) {
    match kind {
        Notice::Info => (ph::INFO, theme::NEVOA),
        Notice::Warning => (ph::WARNING, theme::AVISO),
        Notice::Error => (ph::WARNING_CIRCLE, theme::AO_VIVO),
    }
}

/// A notice's content: its icon, then the text (wrapping).
pub fn notice_text(ui: &mut Ui, kind: Notice, text: &str) {
    let (icon, color) = notice_icon(kind);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = space::M;
        ui.label(RichText::new(icon).font(theme::body(size::ICON)).color(color));
        ui.add(egui::Label::new(RichText::new(text).color(theme::GIZ)).wrap());
    });
}

// ---------------------------------------------------------------- dialogs and settings pages

/// Width of a confirmation dialog's content.
pub const DIALOG_W: f32 = 380.0;
/// What a [`dialog_frame`] dialog keeps outside its body's [`dialog_scroll`]: the title, a row
/// of buttons, the margins and a gap around the dialog.
pub const DIALOG_RESERVE: f32 = 170.0;

/// v1.1 I6: a dialog's content width, `want` or less so the dialog (its margins and a gap around
/// it) stays inside a small or zoomed-in window.
pub fn dialog_w(ctx: &egui::Context, want: f32) -> f32 {
    want.min(ctx.content_rect().width() - 72.0).max(160.0)
}

/// v1.1 I6: a window's size, `want` or less so it stays inside the app's window with `gap` around
/// it; `min` only while the window has room for it.
pub fn fit_size(ctx: &egui::Context, want: Vec2, min: Vec2, gap: f32) -> Vec2 {
    let room = ctx.content_rect().size() - Vec2::splat(2.0 * gap);
    want.min(room).max(min.min(room))
}

/// v1.1 I6: a dialog's body, in a vertical scroll once the app's window is too short for it (a
/// small or zoomed-in window): `reserve` is what the dialog keeps outside the scroll (its title,
/// its buttons, its margins and a gap around it), so those always show.
pub fn dialog_scroll<R>(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, reserve: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let max_h = (ui.ctx().content_rect().height() - reserve).max(60.0);
    egui::ScrollArea::vertical().id_salt(id).max_height(max_h).auto_shrink([false, true]).show(ui, add).inner
}

/// The quiet text after a header's title ("3 pessoas · 1 em foco", "@nome"): cut with "…" to the
/// room left, and left out when there is hardly any (never drawn under the header's buttons, which
/// are laid out first).
pub fn header_extra(ui: &mut Ui, text: Option<String>, size: f32) {
    if let Some(t) = text.filter(|_| ui.available_width() >= 48.0) {
        ui.add(egui::Label::new(RichText::new(t).font(theme::body(size)).color(theme::NEVOA)).truncate().selectable(false));
    }
}

/// The frame of a dialog: [`theme::PAREDE`], hairline outline, radius 10, roomy margins and the
/// popup shadow (a dialog floats like a popup).
pub fn dialog_frame(style: &egui::Style) -> Frame {
    Frame::new()
        .fill(theme::PAREDE)
        .stroke(Stroke::new(stroke::HAIRLINE, theme::RISCO))
        .corner_radius(radius::POPUP)
        .inner_margin(Margin::same(20))
        .shadow(style.visuals.popup_shadow)
}

/// A dialog's or a page's title (Bricolage 17, chalk).
pub fn dialog_title(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::display(17.0)).color(theme::GIZ)).selectable(false))
}

/// Body text of a dialog or a page (soft chalk, wraps).
pub fn body_text(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Label::new(RichText::new(text.into()).color(theme::TEXTO)).wrap())
}

/// A quiet explanation under a field or a group (small, [`theme::NEVOA`], wraps).
pub fn hint(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Label::new(RichText::new(text.into()).font(theme::body(12.5)).color(theme::NEVOA)).wrap())
}

/// A message after an action: green when it worked, red when not.
pub fn result_text(ui: &mut Ui, ok: bool, text: &str) -> Response {
    let (icon, color) = if ok { (ph::CHECK_CIRCLE, theme::ONLINE) } else { (ph::WARNING_CIRCLE, theme::AO_VIVO) };
    ui.add(egui::Label::new(RichText::new(format!("{icon}  {text}")).font(theme::body(13.0)).color(color)).wrap())
}

/// The label above a field (semibold 12.5, [`theme::NEVOA`]).
pub fn field_label(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::semibold(12.5)).color(theme::NEVOA)).selectable(false))
}

/// A confirmation dialog in the middle of the window: the title, `body`, then "Cancelar" and the
/// action on the right (`kind`: [`Kind::Danger`] for what cannot be undone). Esc cancels.
/// `Some(true)`: the action; `Some(false)`: cancelled; `None`: still open.
pub fn confirm(ctx: &egui::Context, id: impl std::hash::Hash + std::fmt::Debug, title: &str, yes: &str, kind: Kind, yes_enabled: bool, body: impl FnOnce(&mut Ui)) -> Option<bool> {
    let mut answer = ctx.input(|i| i.key_pressed(egui::Key::Escape)).then_some(false);
    egui::Window::new(title)
        .id(egui::Id::new(("kit_confirm", id)))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(dialog_frame(&ctx.global_style()))
        .show(ctx, |ui| {
            ui.set_width(dialog_w(ctx, DIALOG_W));
            ui.spacing_mut().item_spacing.y = 6.0;
            dialog_title(ui, title);
            ui.add_space(space::S);
            dialog_scroll(ui, "kit_confirm_body", DIALOG_RESERVE, body);
            ui.add_space(space::XL);
            // One row high (a bare right-to-left layout would take the window's whole height).
            ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), Size::Normal.height()), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled_ui(yes_enabled, |ui| button(ui, kind, None, yes)).inner.clicked() {
                    answer = Some(true);
                }
                if button(ui, Kind::Secondary, None, "Cancelar").clicked() {
                    answer = Some(false);
                }
            });
        });
    answer
}

/// A group of rows on a settings page: [`theme::PAREDE`] on the page's [`theme::NOITE`],
/// hairline outline, radius 10.
pub fn group_frame() -> Frame {
    Frame::new().fill(theme::PAREDE).stroke(Stroke::new(stroke::HAIRLINE, theme::RISCO)).corner_radius(radius::CARD).inner_margin(Margin::symmetric(14, 12))
}

/// A label / value line inside a [`group_frame`]: the label in [`theme::NEVOA`] in a column of
/// `label_w`, the value drawn by `value`.
pub fn info_row<R>(ui: &mut Ui, label_w: f32, label: &str, value: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(label_w, 22.0), Sense::hover());
        ui.painter().text(Pos2::new(r.left(), r.center().y), Align2::LEFT_CENTER, label, theme::body(13.0), theme::NEVOA);
        value(ui)
    })
    .inner
}

/// The page list of a settings-like window: a row per page (icon, name), the open one filled.
/// Returns the page clicked.
pub fn nav_list(ui: &mut Ui, pages: &[(&str, &str)], open: usize) -> Option<usize> {
    let mut picked = None;
    if nav_compact(ui) {
        // I6, a narrow window: the pages as icons (their names on hover).
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            for (i, (icon, name)) in pages.iter().enumerate() {
                if icon_button(ui, icon, name, if i == open { IconState::On } else { IconState::Normal }).clicked() {
                    picked = Some(i);
                }
            }
        });
        return picked;
    }
    for (i, (icon, name)) in pages.iter().enumerate() {
        let (r, _) = sidebar_row(ui, Row { icon, name, label: name, selected: i == open, unread: false, count: 0, mentions: 0, hover: None });
        if r.clicked() {
            picked = Some(i);
        }
    }
    picked
}

/// Width of the page list of a [`settings_window`].
pub const NAV_W: f32 = 236.0;
/// I6: its width in a narrow window (the pages as icons).
pub const NAV_ICONS_W: f32 = 52.0;
/// The narrowest page beside the page list with its names (else the list goes to icons).
pub const PAGE_MIN_W: f32 = 440.0;

/// The page list of a [`settings_window`] is the narrow one (icons only): its `ui` is that narrow.
pub fn nav_compact(ui: &Ui) -> bool {
    ui.available_width() < 120.0
}

/// A settings-like window (the server window...): no title bar, the page list on the left on
/// [`theme::PAREDE`] (`nav`) and the open page on the right on [`theme::NOITE`] (`page`, in a
/// vertical scroll, with a close button in its corner). `size` is clamped to the app's window.
/// Returns false once the close button (or Esc, when `esc` is true) closed it.
pub fn settings_window(ctx: &egui::Context, id: &str, size: Vec2, esc: bool, nav: impl FnOnce(&mut Ui), page: impl FnOnce(&mut Ui)) -> bool {
    // (I6: the minimum only while the window has room: never past its edges.)
    let size = fit_size(ctx, size, Vec2::new(480.0, 320.0), 16.0);
    let mut open = !(esc && ctx.input(|i| i.key_pressed(egui::Key::Escape)));
    let frame = Frame::new().fill(theme::PAREDE).stroke(Stroke::new(stroke::HAIRLINE, theme::RISCO)).corner_radius(radius::POPUP).shadow(ctx.global_style().visuals.popup_shadow);
    egui::Window::new(id)
        .id(egui::Id::new(("kit_settings_window", id)))
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .fixed_size(size)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(frame)
        .show(ctx, |ui| {
            let full = Rect::from_min_size(ui.max_rect().min, size);
            ui.allocate_rect(full, Sense::hover());
            let nav_w = if size.x - NAV_W >= PAGE_MIN_W { NAV_W } else { NAV_ICONS_W };
            let nav_rect = Rect::from_min_max(full.min, Pos2::new(full.left() + nav_w, full.bottom()));
            let page_rect = Rect::from_min_max(Pos2::new(nav_rect.right(), full.top()), full.max);
            let r = radius::POPUP;
            ui.painter().rect_filled(page_rect, CornerRadius { nw: 0, sw: 0, ne: r, se: r }, theme::NOITE);
            ui.painter().vline(page_rect.left(), page_rect.y_range(), Stroke::new(stroke::HAIRLINE, theme::RISCO));
            let nav_inner = nav_rect.shrink2(Vec2::new(if nav_w < NAV_W { 6.0 } else { 10.0 }, 14.0));
            let mut nav_ui = ui.new_child(egui::UiBuilder::new().max_rect(nav_inner).layout(egui::Layout::top_down(egui::Align::Min)));
            nav_ui.set_clip_rect(nav_rect.intersect(ui.clip_rect()));
            nav(&mut nav_ui);
            let close = Rect::from_min_size(Pos2::new(page_rect.right() - 44.0, page_rect.top() + 12.0), Vec2::splat(30.0));
            let inner = Rect::from_min_max(page_rect.min + Vec2::new(28.0, 22.0), page_rect.max - Vec2::new(12.0, 8.0));
            let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
            egui::ScrollArea::vertical().id_salt((id, "page")).auto_shrink([false, false]).show(&mut page_ui, |ui| {
                ui.set_width(inner.width() - 24.0);
                page(ui);
                ui.add_space(space::XL);
            });
            if ui.put(close, |ui: &mut Ui| icon_button(ui, ph::X, "Fechar", IconState::Normal)).clicked() {
                open = false;
            }
        });
    open
}

/// The title of a settings page, with an optional quiet line under it.
pub fn page_title(ui: &mut Ui, title: &str, sub: Option<&str>) {
    ui.add(egui::Label::new(RichText::new(title).font(theme::display(size::HEADING)).color(theme::GIZ)).selectable(false));
    if let Some(s) = sub {
        hint(ui, s);
    }
    ui.add_space(space::L);
}

// ---------------------------------------------------------------- gallery

/// Every component on one page (`--autotest kit`): for screenshots and a look at the theme.
pub struct Gallery {
    pub text: String,
    pub selected: usize,
    /// Scroll to the end (the second screenshot).
    pub to_end: bool,
}

impl Default for Gallery {
    fn default() -> Self {
        Self { text: "Mensagem em #geral".into(), selected: 0, to_end: false }
    }
}

/// Show the gallery in `ui`.
pub fn gallery(ui: &mut Ui, g: &mut Gallery) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(space::M);
        ui.heading("Componentes da interface 1.0");
        ui.label(RichText::new("A sala à noite: tudo calmo, o abajur só em quem fala, na ação principal e nas menções.").color(theme::NEVOA));
        ui.add_space(space::XL);
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            section(ui, "Botões");
            ui.horizontal(|ui| {
                button(ui, Kind::Primary, Some(ph::SCREENCAST), "Transmitir");
                button(ui, Kind::Secondary, Some(ph::VIDEO_CAMERA), "Câmera");
                button(ui, Kind::Ghost, Some(ph::GEAR_SIX), "Configurações");
                button(ui, Kind::Danger, Some(ph::PHONE_X), "Sair");
            });
            ui.horizontal(|ui| {
                button(ui, Kind::Secondary, None, "Cancelar");
                button(ui, Kind::Primary, None, "Salvar");
                ui.add_enabled_ui(false, |ui| button(ui, Kind::Primary, None, "Desativado"));
                button_sized(ui, Kind::Secondary, Size::Small, Some(ph::PLUS), "Pequeno");
            });
            ui.horizontal(|ui| {
                for (icon, kind) in [(ph::MICROPHONE, Kind::Secondary), (ph::VIDEO_CAMERA, Kind::Secondary), (ph::MUSIC_NOTES, Kind::Secondary), (ph::PHONE_X, Kind::Danger)] {
                    button_sized(ui, kind, Size::Large, Some(icon), "");
                }
                button_sized(ui, Kind::Primary, Size::Large, Some(ph::SCREENCAST), "Transmitir");
            });
            // v1.1: the buttons that sit on a video card ("Assistir transmissão", "Ouvir o áudio").
            Frame::new().fill(theme::MOVEL).corner_radius(radius::CARD).inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    card_button(ui, true, Size::Normal, Some(ph::PLAY), "Assistir transmissão");
                    card_button(ui, true, Size::Small, Some(ph::SPEAKER_HIGH), "Ouvir o áudio");
                    card_button(ui, false, Size::Small, None, "Parar de ouvir");
                    ui.add_enabled_ui(false, |ui| card_button(ui, true, Size::Small, Some(ph::PLAY), "Assistir"));
                });
            });
            ui.horizontal(|ui| {
                icon_button(ui, ph::MICROPHONE, "Mutar", IconState::Normal);
                icon_button(ui, ph::MICROPHONE_SLASH, "Desmutar", IconState::Off);
                icon_button(ui, ph::HEADPHONES, "Ensurdecer", IconState::Normal);
                icon_button(ui, ph::CHAT_CENTERED_TEXT, "Mostrar chat", IconState::On);
                icon_button(ui, ph::CORNERS_OUT, "Tela cheia", IconState::Normal);
                icon_button(ui, ph::GEAR_SIX, "Configurações", IconState::Normal);
            });
            ui.add_space(space::L);
            section(ui, "Pílulas e etiquetas");
            ui.horizontal(|ui| {
                pill(ui, "3", theme::GIZ, theme::NOITE);
                pill(ui, "@", theme::ABAJUR, theme::ABAJUR_TINTA);
                pill(ui, "99+", theme::GIZ, theme::NOITE);
                pill(ui, "Moderador", theme::MOVEL, theme::NEVOA);
                live_tag(ui);
                ui.label(RichText::new("@dono").color(theme::ABAJUR).background_color(theme::ABAJUR_FUNDO));
            });
            ui.add_space(space::L);
            section(ui, "Avatares");
            ui.horizontal(|ui| {
                avatar_initial(ui, 22.0, "M", Color32::from_rgb(0xE7, 0xA6, 0xC1), true, theme::PAREDE);
                avatar_initial(ui, 22.0, "J", Color32::from_rgb(0x9F, 0xD3, 0xC7), false, theme::PAREDE);
                avatar_initial(ui, 32.0, "D", Color32::from_rgb(0xC9, 0xB8, 0xF2), true, theme::PAREDE);
                avatar_initial(ui, 56.0, "B", Color32::from_rgb(0xF2, 0xD2, 0x9B), true, theme::PAREDE);
            });
            ui.add_space(space::L);
            section(ui, "Campos");
            ui.add(field(&mut g.text).desired_width(280.0));
            ui.add(field(&mut String::new()).hint_text("Seu nome").desired_width(280.0));
            let mut on = true;
            ui.checkbox(&mut on, "Lembrar a senha neste computador");
            let mut v = 70.0;
            ui.add(egui::Slider::new(&mut v, 0.0..=100.0).text("Volume"));
            ui.horizontal(|ui| {
                let mut r = 1;
                ui.radio_value(&mut r, 0, "Automático");
                ui.radio_value(&mut r, 1, "Manual");
            });
            ui.add_space(space::L);
            section(ui, "Avisos");
            notice_frame(Notice::Info).show(ui, |ui| {
                ui.label("A transmissão de Tati foi encerrada.");
            });
            ui.add_space(space::S);
            notice_frame(Notice::Warning).show(ui, |ui| {
                ui.label(RichText::new(format!("{} Conexão com o servidor perdida. Tentando reconectar...", ph::WARNING)).color(theme::AVISO));
            });
            ui.add_space(space::S);
            notice_frame(Notice::Error).show(ui, |ui| {
                ui.label(RichText::new("Não foi possível entrar na sala de voz.").color(theme::GIZ));
            });
            ui.add_space(space::L);
            section(ui, "Letras");
            ui.label(RichText::new("Título — Bricolage Grotesque 19").heading());
            ui.label(RichText::new("Título pequeno — Bricolage 15,5").font(theme::display(size::TITLE)));
            ui.label("Corpo — Atkinson Hyperlegible Next 14: ação, atenção, coração, pão.");
            ui.label(RichText::new("Pequeno — Atkinson 12, texto secundário").small().color(theme::NEVOA));
            ui.label(RichText::new("Botão — Atkinson SemiBold 13").font(theme::semibold(size::BUTTON)));
            ui.label(RichText::new("Mono — 0123456789 XXXX-XXXX").monospace());
            ui.label(format!("Ícones: {} {} {} {} {} {} · emoji: 👍 🔥 🎉", ph::MICROPHONE, ph::HEADPHONES, ph::SCREENCAST, ph::HASH, ph::LOCK_SIMPLE, ph::GEAR_SIX));

            let ui = &mut cols[1];
            section(ui, "Lateral");
            side_panel_frame().show(ui, |ui| {
                ui.set_width(248.0);
                ui.label(RichText::new("Turma da Lua").heading());
                ui.label(RichText::new("7 online · 3 na voz").small().color(theme::NEVOA));
                ui.add_space(space::S);
                section(ui, "Texto");
                let rows: [(&str, &str, &str, bool, u32, u32); 4] = [
                    (ph::HASH, "geral", "# geral", false, 0, 0),
                    (ph::HASH, "jogatina-de-sexta-e-outros-planos", "# jogatina", true, 3, 1),
                    (ph::HASH, "memes", "# memes", false, 0, 0),
                    (ph::LOCK_SIMPLE, "administração", "# administração", false, 0, 0),
                ];
                for (i, (icon, name, label, unread, count, mentions)) in rows.into_iter().enumerate() {
                    let (r, _) = sidebar_row(ui, Row { icon, name, label, selected: g.selected == i, unread, count, mentions, hover: None });
                    if r.clicked() {
                        g.selected = i;
                    }
                }
                section(ui, "Voz");
                sidebar_row(ui, Row { icon: ph::SPEAKER_HIGH, name: "Sala principal", label: "Sala principal", selected: false, unread: false, count: 0, mentions: 0, hover: None });
                sidebar_row(ui, Row { icon: ph::SPEAKER_HIGH, name: "Sala do foco", label: "Sala do foco", selected: false, unread: false, count: 0, mentions: 0, hover: None });
            });
            ui.add_space(space::L);
            section(ui, "Cards de vídeo");
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), 150.0), Sense::hover());
            let half = Rect::from_min_max(rect.min, Pos2::new(rect.center().x - 5.0, rect.max.y));
            let other = Rect::from_min_max(Pos2::new(rect.center().x + 5.0, rect.min.y), rect.max);
            let p = ui.painter();
            paint_card(p, half, true);
            paint_card_label(p, half, Some((ph::MONITOR, theme::GIZ)), "Tati · tela", true);
            paint_card(p, other, false);
            p.circle_filled(other.center() - Vec2::new(0.0, 8.0), 28.0, Color32::from_rgb(0xC9, 0xB8, 0xF2));
            p.text(other.center() - Vec2::new(0.0, 8.0), Align2::CENTER_CENTER, "D", theme::semibold(22.0), theme::NOITE);
            paint_card_label(p, other, Some((ph::MICROPHONE_SLASH, theme::AO_VIVO)), "dono", false);
            ui.add_space(space::XS);
            card_frame().inner_margin(Margin::same(10)).show(ui, |ui| {
                ui.set_width(200.0);
                title(ui, "Card sem vídeo");
                ui.label(RichText::new("moldura de card (raio 10)").small().color(theme::NEVOA));
            });
            ui.add_space(space::L);
            section(ui, "Menu");
            popup_frame(ui.style()).show(ui, |ui| {
                ui.set_width(230.0);
                menu_row(ui, ph::EYE_SLASH, "Ocultar este card", false);
                menu_row(ui, ph::USERS_THREE, "Ocultar todos sem vídeo", false);
                ui.separator();
                menu_row(ui, ph::USER, "Ver perfil", false);
                menu_row(ui, ph::SPEAKER_SIMPLE_HIGH, "Volume", false);
                menu_row(ui, ph::PROHIBIT, "Bloquear", true);
            });
        });
        ui.add_space(space::XXL);
        if g.to_end {
            ui.scroll_to_cursor(Some(egui::Align::Max));
        }
    });
}
