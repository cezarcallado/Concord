//! The v1.0 visual system, "a sala à noite" (docs/PLANO-v1.0.md): colour tokens, spacing, corner
//! radii and strokes, the fonts and the icon font, and [`apply`], which installs all of it in egui
//! once at startup. Nothing here runs per frame except the tiny font-id helpers (no allocation:
//! the named font families are built once and cloned, which only bumps a reference count).
//!
//! Fonts (all SIL Open Font License 1.1, texts in `app/assets/fonts/OFL-*.txt`, listed in
//! THIRD-PARTY-NOTICES.md). google/fonts only publishes variable fonts of these families, so the
//! static weights embedded here were instanced from them with fontTools' `varLib.instancer`
//! (fontTools 4.62.1; no other change):
//! - `AtkinsonHyperlegibleNext-Regular.ttf` (wght=400) and `AtkinsonHyperlegibleNext-SemiBold.ttf`
//!   (wght=600), from
//!   <https://raw.githubusercontent.com/google/fonts/47d54ca1ebb14fd864695f1d39dc734f39017c33/ofl/atkinsonhyperlegiblenext/AtkinsonHyperlegibleNext%5Bwght%5D.ttf>
//!   (source SHA-256 5a455d1cfa099b601ab70751bb9673e8fe1854dc4500c80e1a220d0d75e31745;
//!   instances 026d442ddce0692b42a702111eea11a6e9dd72fc4d222c5a2c31892f0e5147a1 and
//!   3376de4ae875ac030f3cbe9b285fa0f470c7f9cfe4ca35c7fef8742f878ad7bf).
//! - `BricolageGrotesque-SemiBold.ttf` (wght=600, wdth=100, opsz=18), from
//!   <https://raw.githubusercontent.com/google/fonts/47d54ca1ebb14fd864695f1d39dc734f39017c33/ofl/bricolagegrotesque/BricolageGrotesque%5Bopsz%2Cwdth%2Cwght%5D.ttf>
//!   (source SHA-256 413e7357809ddd12fd80a96a8a396de0e401638d4acd3cb3e37532f0472ac682;
//!   instance d1aa5676e1f688b742130a5a43b684f178aa527f3d299d850d43721c2a83f23f).
//!
//! Icons: Phosphor (MIT), regular weight, from the `egui-phosphor` crate, subset at compile time
//! to the icons named in [`ph`] (a few KB instead of the full ~480 KB font).
//!
//! egui's own fonts stay as fallbacks after ours: Ubuntu-Light (symbols such as ✓ ▶ ●), the two
//! emoji fonts (reactions, messages, the soundboard) and Hack (monospace). The emoji that have a
//! Twemoji picture (`emoji`, CC-BY 4.0) are drawn in colour over their monochrome glyphs.

use std::sync::LazyLock;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle};

// ---------------------------------------------------------------- colours

/// Background of the window (the room at night).
pub const NOITE: Color32 = Color32::from_rgb(0x13, 0x1C, 0x26);
/// Panels (the sidebar) and dialog windows.
pub const PAREDE: Color32 = Color32::from_rgb(0x1B, 0x27, 0x33);
/// Raised things: buttons, fields, the selected row, video cards, menus.
pub const MOVEL: Color32 = Color32::from_rgb(0x25, 0x34, 0x44);
/// Dividers and outlines.
pub const RISCO: Color32 = Color32::from_rgb(0x2E, 0x3F, 0x51);
/// Strong text.
pub const GIZ: Color32 = Color32::from_rgb(0xE6, 0xEC, 0xF1);
/// Secondary text, idle icons, section headers.
pub const NEVOA: Color32 = Color32::from_rgb(0x8A, 0x9D, 0xAE);
/// The lamp: whoever is speaking, the main action and mentions. Nothing else.
pub const ABAJUR: Color32 = Color32::from_rgb(0xF0, 0xB4, 0x4C);
/// The lamp's light on a surface (a mention's background).
pub const ABAJUR_FUNDO: Color32 = Color32::from_rgb(0x3A, 0x32, 0x22);
/// Live, and destructive actions.
pub const AO_VIVO: Color32 = Color32::from_rgb(0xE5, 0x53, 0x4B);
/// Online / connected.
pub const ONLINE: Color32 = Color32::from_rgb(0x57, 0xC2, 0x8A);

// Derived states, chosen to sit between the tokens above.

/// Body text: a little softer than [`GIZ`] for long reading (the mockup's message text).
pub const TEXTO: Color32 = Color32::from_rgb(0xD3, 0xDC, 0xE4);
/// [`MOVEL`] under the pointer.
pub const MOVEL_HOVER: Color32 = Color32::from_rgb(0x2D, 0x3F, 0x52);
/// [`MOVEL`] while pressed.
pub const MOVEL_PRESSED: Color32 = Color32::from_rgb(0x1F, 0x2C, 0x3A);
/// A list row under the pointer (quieter than the selected one).
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x21, 0x2E, 0x3C);
/// The selected / open row.
pub const ROW_SELECTED: Color32 = MOVEL;
/// [`ABAJUR`] under the pointer / pressed.
pub const ABAJUR_HOVER: Color32 = Color32::from_rgb(0xF4, 0xC2, 0x68);
pub const ABAJUR_PRESSED: Color32 = Color32::from_rgb(0xD9, 0x9C, 0x38);
/// Text on an [`ABAJUR`] fill.
pub const ABAJUR_TINTA: Color32 = Color32::from_rgb(0x2E, 0x22, 0x06);
/// [`AO_VIVO`] under the pointer / pressed.
pub const AO_VIVO_HOVER: Color32 = Color32::from_rgb(0xEC, 0x6A, 0x62);
pub const AO_VIVO_PRESSED: Color32 = Color32::from_rgb(0xC8, 0x43, 0x3C);
/// A quiet danger surface (an error notice, a destructive menu row under the pointer).
pub const PERIGO_FUNDO: Color32 = Color32::from_rgb(0x3D, 0x24, 0x28);
/// The box of a checkbox or a radio button and a slider's rail (a step above [`MOVEL`]), and
/// under the pointer.
pub const CAIXA: Color32 = Color32::from_rgb(0x34, 0x47, 0x5B);
pub const CAIXA_HOVER: Color32 = Color32::from_rgb(0x3D, 0x52, 0x68);
/// Below [`NOITE`]: progress tracks, the area behind scroll bars.
pub const FUNDO: Color32 = Color32::from_rgb(0x0E, 0x15, 0x1D);
/// The translucent strip behind a name on a video card.
pub const ETIQUETA: Color32 = Color32::from_rgba_premultiplied(11, 17, 23, 204);
/// Offline / switched-off dot.
pub const DESLIGADO: Color32 = Color32::from_rgb(0x4A, 0x5B, 0x6B);
/// Links in the chat.
pub const LINK: Color32 = Color32::from_rgb(0x7F, 0xB4, 0xE6);

/// The selection highlight and the filled part of a slider: mist at about half strength (v1.0
/// phase 4: no longer the lamp, which stays for speakers, the main action and mentions).
pub const SELECAO: Color32 = Color32::from_rgba_premultiplied(62, 71, 78, 115);
/// Warnings and "working on it" states (a notice's icon and outline, a hint that needs
/// attention): chalk, never the lamp. Danger keeps [`AO_VIVO`].
pub const AVISO: Color32 = GIZ;

// ---------------------------------------------------------------- spacing, radii, strokes

/// Spacing scale (points).
pub mod space {
    pub const XS: f32 = 2.0;
    pub const S: f32 = 4.0;
    pub const M: f32 = 8.0;
    pub const L: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 24.0;
}

/// Corner radii: video cards and popups 10, buttons 8, rows 6, small tags 4, pills round,
/// panels square.
pub mod radius {
    pub const CARD: u8 = 10;
    pub const POPUP: u8 = 10;
    pub const BUTTON: u8 = 8;
    pub const ROW: u8 = 6;
    pub const TAG: u8 = 4;
    /// Fully round at any height a pill can have.
    pub const PILL: u8 = 255;
    pub const PANEL: u8 = 0;
}

/// Stroke widths.
pub mod stroke {
    /// Dividers, outlines of popups and fields.
    pub const HAIRLINE: f32 = 1.0;
    /// The lamp around whoever speaks: the card's border and the avatar's ring.
    pub const SPEAKING: f32 = 2.0;
    /// The gap (in the background colour) between an avatar and its speaking ring.
    pub const RING_GAP: f32 = 2.0;
    /// Keyboard focus.
    pub const FOCUS: f32 = 1.5;
}

// ---------------------------------------------------------------- fonts

const ATKINSON_REGULAR: &[u8] = include_bytes!("../assets/fonts/AtkinsonHyperlegibleNext-Regular.ttf");
const ATKINSON_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/AtkinsonHyperlegibleNext-SemiBold.ttf");
const BRICOLAGE_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/BricolageGrotesque-SemiBold.ttf");

/// Names of the font families (and font data keys) we add.
const F_BODY: &str = "atkinson";
const F_SEMIBOLD: &str = "atkinson-semibold";
const F_DISPLAY: &str = "bricolage";
/// Family for semibold UI text (buttons, unread rows, names).
pub const FAMILY_SEMIBOLD: &str = "ui-semibold";
/// Family for titles (server name, rooms, section headers).
pub const FAMILY_DISPLAY: &str = "display";
/// Text style of a small title (Bricolage ~15.5).
pub const STYLE_TITLE: &str = "title";
/// Text style of a section header (Bricolage 13.5).
pub const STYLE_SECTION: &str = "section";

static SEMIBOLD: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name(FAMILY_SEMIBOLD.into()));
static DISPLAY: LazyLock<FontFamily> = LazyLock::new(|| FontFamily::Name(FAMILY_DISPLAY.into()));

/// Body / UI text (Atkinson Hyperlegible Next Regular).
pub fn body(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Semibold UI text (Atkinson Hyperlegible Next SemiBold).
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, SEMIBOLD.clone())
}

/// Titles (Bricolage Grotesque SemiBold).
pub fn display(size: f32) -> FontId {
    FontId::new(size, DISPLAY.clone())
}

/// Text sizes (points).
pub mod size {
    pub const HEADING: f32 = 19.0;
    pub const TITLE: f32 = 15.5;
    pub const SECTION: f32 = 13.5;
    pub const BODY: f32 = 14.0;
    pub const BUTTON: f32 = 13.0;
    pub const SMALL: f32 = 12.0;
    pub const MONO: f32 = 13.0;
    /// A Phosphor icon next to body text / in a button.
    pub const ICON: f32 = 17.0;
}

egui_phosphor::subset! {
    /// The Phosphor icons the app uses (regular weight). Only these are embedded.
    pub mod phosphor {
        use regular::{
            // (phase 3a: APERTURE, ARROW_COUNTER_CLOCKWISE, BELL, FOLDER_OPEN, MAGNIFYING_GLASS, SCISSORS,
            // SLIDERS_HORIZONTAL, SUN, UPLOAD_SIMPLE, WAVEFORM for the settings and the secondary windows.)
            APERTURE, ARROW_COUNTER_CLOCKWISE, BELL, FOLDER_OPEN, MAGNIFYING_GLASS, SCISSORS, SLIDERS_HORIZONTAL, SUN, UPLOAD_SIMPLE, WAVEFORM,
            // (phase 3b: CARET_UP, CHECK_CIRCLE, CLIPBOARD_TEXT, PHONE_INCOMING, WARNING_CIRCLE for the server window,
            // the host panel, the ringing window and the notices.)
            CARET_UP, CHECK_CIRCLE, CLIPBOARD_TEXT, PHONE_INCOMING, WARNING_CIRCLE,
            // (v1.1: IDENTIFICATION_BADGE, LINK, SELECTION_ALL for the message and text box menus.)
            IDENTIFICATION_BADGE, LINK, SELECTION_ALL,
            // (v1.1: KEYBOARD for "Atalhos".)
            KEYBOARD,
            // (v1.1: GAME_CONTROLLER for the Go Live hover card's "Jogando".)
            GAME_CONTROLLER,
            // (v1.1 T0: LOCK_SIMPLE_OPEN for the server header's "Conexão sem proteção".)
            LOCK_SIMPLE_OPEN,
            // (v1.1 D1: HEART for "Apoiar o projeto".)
            HEART,
            ARROW_BEND_UP_LEFT, ARROW_RIGHT, ARROW_SQUARE_OUT, ARROWS_CLOCKWISE, ARROWS_LEFT_RIGHT, CARET_DOWN,
            CARET_LEFT, CARET_RIGHT, CHAT_CENTERED_TEXT, CHAT_CIRCLE, CHECK, CIRCLE, COPY, CORNERS_IN, CORNERS_OUT, CROWN, DOTS_THREE,
            ENVELOPE_SIMPLE, EYE, EYE_SLASH, GAVEL, GEAR_SIX, HARD_DRIVES, HASH, HEADPHONES, IMAGE, INFO, LOCK_SIMPLE,
            MAGNIFYING_GLASS_PLUS, MASK_HAPPY, MICROPHONE, MICROPHONE_SLASH, MINUS, MONITOR, MUSIC_NOTES, PAPER_PLANE_RIGHT,
            PENCIL_SIMPLE, PHONE, PHONE_X, PLAY, PLUS, PLUS_CIRCLE, PROHIBIT, PUSH_PIN, RECORD, SCREENCAST, SHIELD,
            SMILEY, SPEAKER_HIGH, SPEAKER_LOW, SPEAKER_SIMPLE_HIGH, SPEAKER_SLASH, SQUARE_SPLIT_HORIZONTAL,
            SQUARES_FOUR, STOP, TRASH, USER, USERS, USERS_THREE, VIDEO_CAMERA, VIDEO_CAMERA_SLASH, WARNING, WRENCH, X,
        };
    }
}

/// The Phosphor icon constants (`ph::MICROPHONE`, ...): characters to put in any text.
pub use phosphor::regular as ph;

/// egui's fonts plus ours: Atkinson for UI text (first in `Proportional`, so every plain label
/// uses it), Atkinson SemiBold and Bricolage in their own families, Phosphor as the fallback right
/// after the text font of every family (its codepoints are in the private use area; it also maps
/// a-z to blank ligature parts, so it must never come before a text font).
pub fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(F_BODY.into(), FontData::from_static(ATKINSON_REGULAR).into());
    fonts.font_data.insert(F_SEMIBOLD.into(), FontData::from_static(ATKINSON_SEMIBOLD).into());
    fonts.font_data.insert(F_DISPLAY.into(), FontData::from_static(BRICOLAGE_SEMIBOLD).into());
    // Adds Phosphor second in `Proportional` (after Ubuntu-Light, which we put behind ours below)
    // and its own family.
    ph::add_to_fonts(&mut fonts);
    let icons = ph::FONT_NAME.to_string();
    // egui's defaults after the icon font: Ubuntu-Light, then the emoji fonts.
    let fallbacks: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .map(|v| v.iter().filter(|k| **k != icons).cloned().collect())
        .unwrap_or_default();
    let family = |first: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = first.iter().map(|s| s.to_string()).collect();
        v.push(icons.clone());
        v.extend(fallbacks.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family(&[F_BODY]));
    fonts.families.insert(FontFamily::Name(FAMILY_SEMIBOLD.into()), family(&[F_SEMIBOLD]));
    // (Bricolage has no ✓ or arrows; Atkinson SemiBold fills in before the generic fallbacks.)
    fonts.families.insert(FontFamily::Name(FAMILY_DISPLAY.into()), family(&[F_DISPLAY, F_SEMIBOLD]));
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.insert(mono.len().min(1), icons);
    }
    fonts
}

// ---------------------------------------------------------------- egui style

fn widget(fill: Color32, stroke: Stroke, fg: Color32, r: u8) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: stroke,
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: CornerRadius::same(r),
        expansion: 0.0,
    }
}

/// The dark visuals of the night room.
pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let hair = Stroke::new(stroke::HAIRLINE, RISCO);
    v.widgets.noninteractive = widget(PAREDE, hair, TEXTO, radius::ROW);
    // (egui's own buttons, checkboxes and combo boxes: 6, so a 16-point checkbox stays a square.)
    v.widgets.inactive = widget(MOVEL, Stroke::NONE, GIZ, radius::ROW);
    v.widgets.hovered = widget(MOVEL_HOVER, Stroke::new(stroke::HAIRLINE, Color32::from_rgb(0x3B, 0x50, 0x66)), GIZ, radius::ROW);
    v.widgets.active = widget(MOVEL_PRESSED, hair, GIZ, radius::ROW);
    v.widgets.open = widget(PAREDE, hair, GIZ, radius::ROW);
    v.widgets.open.weak_bg_fill = MOVEL;
    // A checkbox's or radio's box and a slider's rail use `bg_fill` (buttons and combo boxes use
    // `weak_bg_fill`): a step lighter than the raised colour, so an empty box still reads on the
    // wall-coloured settings pages (MOVEL on PAREDE was about 1.2:1).
    v.widgets.inactive.bg_fill = CAIXA;
    v.widgets.hovered.bg_fill = CAIXA_HOVER;
    v.weak_text_color = Some(NEVOA);
    // Mist at half strength behind selected text, selected buttons and a slider's filled part; their
    // text (and a focused field's outline) in chalk, so the lamp stays for speakers, the main action
    // and mentions.
    v.selection = egui::style::Selection { bg_fill: SELECAO, stroke: Stroke::new(stroke::HAIRLINE, GIZ) };
    v.hyperlink_color = LINK;
    v.faint_bg_color = Color32::from_rgb(0x18, 0x23, 0x2E);
    v.extreme_bg_color = FUNDO;
    v.text_edit_bg_color = Some(MOVEL);
    v.code_bg_color = MOVEL;
    v.warn_fg_color = AVISO;
    v.error_fg_color = AO_VIVO;
    v.window_corner_radius = CornerRadius::same(radius::POPUP);
    v.menu_corner_radius = CornerRadius::same(radius::POPUP);
    // Shadows only on menus and popups.
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow { offset: [0, 8], blur: 22, spread: 0, color: Color32::from_black_alpha(120) };
    v.window_fill = PAREDE;
    v.window_stroke = hair;
    v.panel_fill = NOITE;
    v.text_cursor.stroke = Stroke::new(2.0, GIZ);
    // No repaint timer while a text box has focus (an idle app draws nothing).
    v.text_cursor.blink = false;
    v.striped = false;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.indent_has_left_vline = false;
    v
}

/// The whole egui style: visuals, text styles and spacing.
pub fn style() -> egui::Style {
    let mut s = egui::Style { visuals: visuals(), ..Default::default() };
    s.text_styles = [
        (TextStyle::Small, body(size::SMALL)),
        (TextStyle::Body, body(size::BODY)),
        (TextStyle::Button, semibold(size::BUTTON)),
        (TextStyle::Heading, display(size::HEADING)),
        (TextStyle::Monospace, FontId::monospace(size::MONO)),
        (TextStyle::Name(STYLE_TITLE.into()), display(size::TITLE)),
        (TextStyle::Name(STYLE_SECTION.into()), display(size::SECTION)),
    ]
    .into();
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(space::M, 5.0);
    sp.button_padding = egui::vec2(space::M, 3.0);
    sp.interact_size = egui::vec2(40.0, 22.0);
    sp.window_margin = Margin::same(space::L as i8);
    sp.menu_margin = Margin::same(6);
    sp.icon_width = 16.0;
    sp.icon_width_inner = 9.0;
    sp.icon_spacing = 6.0;
    sp.indent = 22.0;
    sp.slider_rail_height = 4.0;
    sp.combo_height = 260.0;
    // Thin, quiet scroll bars: invisible until the pointer is over the area, never taking space.
    sp.scroll = egui::style::ScrollStyle { bar_width: 8.0, floating_width: 3.0, handle_min_length: 24.0, foreground_color: false, ..egui::style::ScrollStyle::floating() };
    s
}

/// Install the theme and the fonts (once, at startup). Dark only in 1.0, whatever Windows uses.
/// Also the colour emoji ([`emoji`]: Twemoji pictures over the emoji font's grey glyphs).
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, style());
    emoji::install(ctx);
}

/// Colour emoji (v1.1): a child module here so it comes with the fonts it draws over.
#[path = "emoji.rs"]
pub mod emoji;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_starts_with_a_text_font_and_has_the_icons_next() {
        let f = fonts();
        let icons = ph::FONT_NAME.to_string();
        for (family, first) in [
            (FontFamily::Proportional, F_BODY),
            (FontFamily::Name(FAMILY_SEMIBOLD.into()), F_SEMIBOLD),
            (FontFamily::Name(FAMILY_DISPLAY.into()), F_DISPLAY),
        ] {
            let keys = &f.families[&family];
            assert_eq!(keys[0], first, "{family:?}");
            assert!(keys.contains(&icons), "{family:?}");
            // egui's emoji fonts stay as fallbacks (reactions, messages, the soundboard).
            assert!(keys.iter().any(|k| k == "NotoEmoji-Regular") && keys.iter().any(|k| k == "emoji-icon-font"), "{family:?}");
            assert_eq!(keys.iter().filter(|k| **k == icons).count(), 1, "{family:?}");
        }
        assert_eq!(f.families[&FontFamily::Monospace][0], "Hack");
    }

    #[test]
    fn at_most_four_font_files_of_our_own() {
        // Three text fonts and the (subset) icon font; egui's defaults are not ours.
        let ours = [ATKINSON_REGULAR.len(), ATKINSON_SEMIBOLD.len(), BRICOLAGE_SEMIBOLD.len(), ph::FONT.len()];
        assert_eq!(ours.len(), 4);
        println!("font bytes: atkinson {} + {}, bricolage {}, phosphor subset {}", ours[0], ours[1], ours[2], ours[3]);
        assert!(ph::FONT.len() < 64 * 1024, "the icon subset grew to {} bytes", ph::FONT.len());
    }

    #[test]
    fn the_text_styles_are_the_planned_ones() {
        let s = style();
        assert_eq!(s.text_styles[&TextStyle::Body], body(14.0));
        assert_eq!(s.text_styles[&TextStyle::Button], semibold(13.0));
        assert_eq!(s.text_styles[&TextStyle::Heading], display(19.0));
        assert!(s.visuals.window_shadow == Shadow::NONE && s.visuals.popup_shadow != Shadow::NONE);
        assert!(!s.visuals.text_cursor.blink);
    }
}
