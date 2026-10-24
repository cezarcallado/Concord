//! The form pieces of the v1.0 secondary windows (phase 3a): settings pages, "Configurar câmera",
//! the Go Live picker, the soundboard dialogs. Built on [`crate::theme`] and [`crate::ui_kit`]:
//!
//! - [`Dialog`]: a window drawn as an egui area (no title bar of egui's): [`theme::PAREDE`],
//!   hairline, radius 10, its own header with a Bricolage title and a close button, Esc to close.
//! - [`group`] / [`divider`]: sentence-case section headers (Bricolage) and the hairline between
//!   groups.
//! - [`row`]: a label in its own column and the control after it, so every label of a page lines up.
//! - [`note`]: a quiet hint under a control (wraps).
//! - [`segmented`]: a row of pill choices (the selected one in chalk on [`theme::MOVEL`]).
//!
//! Plain functions, no state: nothing here allocates per frame beyond the text egui lays out.

use eframe::egui::{self, Align, Align2, Color32, Frame, Id, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, WidgetType};

use crate::theme::{self, ph, radius, size, space, stroke};
use crate::ui_kit::{self, IconState};

/// Width of the label column of a [`row`].
pub const LABEL_W: f32 = 176.0;
/// Height of a form row (the kit's button and field height).
pub const ROW_H: f32 = 30.0;
/// Width of the combo boxes of the forms.
pub const COMBO_W: f32 = 300.0;
/// Width of the sliders of the forms.
pub const SLIDER_W: f32 = 220.0;
/// v1.1 I6: the narrowest the control column gets beside the label column; a narrower page
/// (a small or zoomed-in window) puts each label above its control instead ([`stacked`]).
pub const CONTROL_MIN_W: f32 = 240.0;

/// The page is too narrow for the label column: labels go above their controls, and nothing is
/// indented under the control column ([`row`], [`indented`], [`note`]).
pub fn stacked(ui: &Ui) -> bool {
    ui.available_width() < LABEL_W + ui.spacing().item_spacing.x + CONTROL_MIN_W
}

/// A combo box's width in the control column: [`COMBO_W`], or what is left when less is.
pub fn combo_w(ui: &Ui) -> f32 {
    COMBO_W.min(ui.available_width()).max(120.0)
}

/// Fit the sliders of a page to its width (call at the top of the page): [`SLIDER_W`], or less
/// so that a slider and its number stay in the control column.
pub fn fit_sliders(ui: &mut Ui) {
    let control = if stacked(ui) { ui.available_width() } else { ui.available_width() - LABEL_W - ui.spacing().item_spacing.x };
    ui.spacing_mut().slider_width = SLIDER_W.min(control - 90.0).max(80.0);
}

/// A section header inside a page: sentence case, Bricolage, quiet (like the sidebar's).
pub fn group(ui: &mut Ui, title: &str) {
    ui.add_space(space::S);
    ui_kit::section(ui, title);
    ui.add_space(space::XS);
}

/// [`group`] with a Phosphor icon before the title (the groups of "Configurar câmera").
pub fn group_icon(ui: &mut Ui, icon: &str, title: &str) {
    ui.add_space(space::S);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::M;
        ui.add(egui::Label::new(RichText::new(icon).font(theme::body(15.0)).color(theme::NEVOA)).selectable(false));
        ui_kit::section(ui, title);
    });
    ui.add_space(space::XS);
}

/// The hairline between two groups of a page.
pub fn divider(ui: &mut Ui) {
    ui.add_space(space::L);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(stroke::HAIRLINE, theme::RISCO));
    ui.add_space(space::L);
}

/// A labelled form row: `label` in the label column (vertically centred), then the control. On a
/// narrow page ([`stacked`]) the label goes above the control. The control's texts (a checkbox's)
/// wrap at the page's edge instead of running past it.
pub fn row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    if stacked(ui) {
        ui.add(egui::Label::new(RichText::new(label).font(theme::body(size::BODY)).color(theme::TEXTO)).wrap().selectable(false));
        return ui
            .horizontal(|ui| {
                ui.spacing_mut().interact_size.y = ROW_H;
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                add(ui)
            })
            .inner;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().interact_size.y = ROW_H;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(LABEL_W, ROW_H), Sense::hover());
        let g = ui.painter().layout(label.to_owned(), theme::body(size::BODY), theme::TEXTO, LABEL_W - space::M);
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - g.size().y / 2.0), g, theme::TEXTO);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        add(ui)
    })
    .inner
}

/// The control column's indent ([`LABEL_W`] and the gap; none on a [`stacked`] page).
fn indent_w(ui: &Ui) -> f32 {
    if stacked(ui) {
        0.0
    } else {
        LABEL_W + ui.spacing().item_spacing.x
    }
}

/// A control in the control column with no label (checkboxes, buttons under a row).
pub fn indented<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let left = indent_w(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().interact_size.y = ROW_H;
        ui.add_space(left);
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        add(ui)
    })
    .inner
}

/// A quiet hint (small, mist, wraps). `indent`: under the control column of a [`row`].
pub fn note(ui: &mut Ui, text: impl Into<String>, indent: bool) {
    note_colored(ui, text, indent, theme::NEVOA);
}

/// [`note`] in another colour (a warning in the lamp, an error in red).
pub fn note_colored(ui: &mut Ui, text: impl Into<String>, indent: bool, color: Color32) {
    let left = if indent { indent_w(ui) } else { 0.0 };
    let w = (ui.available_width() - left).max(80.0);
    ui.horizontal(|ui| {
        ui.add_space(left);
        ui.vertical(|ui| {
            ui.set_max_width(w);
            ui.add(egui::Label::new(RichText::new(text.into()).size(size::SMALL + 0.5).color(color)).wrap());
        });
    });
}

/// Pill choices in a row: the chosen one filled ([`theme::MOVEL`], chalk text, chalk hairline),
/// the others quiet. Returns true when the choice changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::S;
        for (v, label) in options {
            if chip(ui, *value == *v, label).clicked() && *value != *v {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

/// One pill of [`segmented`] (also used alone: a toggle chip). 28 high.
pub fn chip(ui: &mut Ui, selected: bool, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_owned(), theme::semibold(size::BUTTON), Color32::PLACEHOLDER);
    let s = Vec2::new(g.size().x + 2.0 * space::L, 28.0);
    let (rect, resp) = ui.allocate_exact_size(s, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered() && ui.is_enabled();
        let (fill, fg, line) = if selected {
            (theme::MOVEL, theme::GIZ, Stroke::new(stroke::HAIRLINE, theme::GIZ))
        } else if hovered {
            (theme::ROW_HOVER, theme::GIZ, Stroke::new(stroke::HAIRLINE, theme::RISCO))
        } else {
            (Color32::TRANSPARENT, theme::NEVOA, Stroke::new(stroke::HAIRLINE, theme::RISCO))
        };
        let fg = if ui.is_enabled() { fg } else { fg.gamma_multiply(0.5) };
        let p = ui.painter();
        p.rect(rect, radius::PILL, fill, line, StrokeKind::Inside);
        p.galley(rect.center() - g.size() / 2.0, g, fg);
    }
    resp.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, ui.is_enabled(), selected, text));
    resp
}

/// A secondary window of v1.0 (settings, camera, Go Live, soundboard dialogs).
pub struct Dialog<'a> {
    pub id: &'a str,
    pub title: &'a str,
    /// A Phosphor icon before the title.
    pub icon: Option<&'a str>,
    /// The size of the whole window (header included). `None`: as big as the content.
    pub size: Option<Vec2>,
    /// Width when `size` is `None`.
    pub width: f32,
    /// Esc closes it (the soundboard's add dialog keeps Esc for the text fields).
    pub esc_closes: bool,
}

impl<'a> Dialog<'a> {
    pub fn new(id: &'a str, title: &'a str) -> Self {
        Self { id, title, icon: None, size: None, width: 420.0, esc_closes: true }
    }

    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn size(mut self, size: Vec2) -> Self {
        self.size = Some(size);
        self
    }

    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// The frame of a dialog: [`theme::PAREDE`], hairline, radius 10 (no shadow: windows have none).
    pub fn frame() -> Frame {
        Frame::new().fill(theme::PAREDE).stroke(Stroke::new(stroke::HAIRLINE, theme::RISCO)).corner_radius(radius::POPUP)
    }

    /// Show it centred on the screen. `add` gets the body's `Ui` (below the header, with the
    /// dialog's padding). Returns `false` when it was closed (the ✕ or Esc) this frame.
    pub fn show(self, ctx: &egui::Context, add: impl FnOnce(&mut Ui)) -> bool {
        let mut open = true;
        let screen = ctx.content_rect();
        egui::Area::new(Id::new(self.id))
            .order(egui::Order::Middle)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .constrain(true)
            .show(ctx, |ui| {
                Self::frame().show(ui, |ui| {
                    // (A content-sized dialog scrolls when the app's window is too short for it:
                    // v1.0 phase 4, "Adicionar som" in a 600-point window lost its title bar.)
                    let mut body_max = None;
                    match self.size {
                        Some(s) => {
                            let s = s.min(screen.size() - Vec2::splat(16.0)).max(Vec2::new(320.0, 200.0));
                            ui.set_min_size(s);
                            ui.set_max_size(s);
                        }
                        None => {
                            let w = self.width.min(screen.width() - 32.0);
                            ui.set_min_width(w);
                            ui.set_max_width(w);
                            ui.set_max_height(screen.height() - 32.0);
                            // (The header's 50 points, the body's margins and the spacing.)
                            body_max = Some((screen.height() - 32.0 - 50.0 - 34.0).max(120.0));
                        }
                    }
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if header(ui, self.title, self.icon) {
                        open = false;
                    }
                    let body = Frame::new().inner_margin(Margin { left: 18, right: 18, top: 12, bottom: 16 });
                    match body_max {
                        Some(h) => {
                            egui::ScrollArea::vertical().id_salt((self.id, "body")).max_height(h).auto_shrink([false, true]).show(ui, |ui| {
                                body.show(ui, add);
                            });
                        }
                        None => {
                            body.show(ui, add);
                        }
                    }
                });
            });
        if self.esc_closes && ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !egui::Popup::is_any_open(ctx) {
            open = false;
        }
        open
    }
}

/// The header of a dialog: the title in Bricolage, a ✕ on the right, the hairline under it.
/// Returns true when the ✕ was clicked.
pub fn header(ui: &mut Ui, title: &str, icon: Option<&str>) -> bool {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 50.0), Sense::hover());
    let inner = rect.shrink2(Vec2::new(18.0, 0.0));
    let mut x = inner.left();
    if let Some(i) = icon {
        let g = ui.painter().layout_no_wrap(i.to_owned(), theme::body(19.0), theme::NEVOA);
        let gw = g.size().x;
        ui.painter().galley(Pos2::new(x, rect.center().y - g.size().y / 2.0), g, theme::NEVOA);
        x += gw + space::M + 2.0;
    }
    ui.painter().text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, title, theme::display(17.0), theme::GIZ);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(stroke::HAIRLINE, theme::RISCO));
    let close = Rect::from_center_size(Pos2::new(inner.right() - 15.0 + 6.0, rect.center().y), Vec2::splat(30.0));
    let mut c = ui.new_child(UiBuilder::new().max_rect(close).layout(Layout::left_to_right(Align::Center)));
    ui_kit::icon_button(&mut c, ph::X, "Fechar (Esc)", IconState::Normal).clicked()
}

/// The footer line of a dialog: a hairline, then `add` laid out right to left (put the main
/// action first).
pub fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(space::M);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, Stroke::new(stroke::HAIRLINE, theme::RISCO));
    ui.add_space(space::M + 2.0);
    ui.with_layout(Layout::right_to_left(Align::Center), add);
}
