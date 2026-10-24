//! The v1.0 app shell (docs/PLANO-v1.0.md, "Navegação como no Discord"): what the center shows,
//! which sidebar row is "open", and the three center modes.
//!
//! - The center shows one thing, picked in the sidebar: a text channel's chat, a DM, or the call
//!   stage (the cards) of the voice room we are in.
//! - The sidebar highlights only what is open in the center ([`open_in_center`]). With the stage
//!   in focus, the text channel loses its "open" highlight.
//! - In a call, three modes (icon buttons in the center header, remembered in settings):
//!   [`CenterMode::Chat`], [`CenterMode::Cards`] and [`CenterMode::CardsWithChat`] (the cards with
//!   a normal text channel's chat docked on the right). The docked channel never gets the "open"
//!   highlight, and what arrives in it while it is visible counts as read.
//!
//! ## Contract with the call stage (phase 2b)
//! - The shell calls [`App::call_stage_ui`] once per frame for the cards area, with a `ui` whose
//!   `max_rect()` is the area left after the sidebar, the center header and the dock. It fills it.
//! - [`App::stage_header_tools`] draws the stage's own buttons (hidden cards, fullscreen...) in the
//!   center header, right before the mode buttons (right to left layout).
//! - `call_stage_ui` draws the voice dock (the call's controls) itself. Outside the stage (only
//!   the chat in the center) the sidebar's voice panel and the user panel have the call's controls:
//!   mute, deafen, leave, the join in progress with "Cancelar", and a failed join with "Tentar de
//!   novo" / "Fechar".
//! - When `stage_fullscreen` is true the shell draws nothing but the stage (no sidebar, no dock, no
//!   headers): `call_stage_ui` gets the whole window. The stage toggles the OS fullscreen and
//!   handles Esc.

use serde::{Deserialize, Serialize};
use vox_proto::ChannelId;

use crate::app::{App, Center};

// ---------------------------------------------------------------- responsive rules (v1.1 I6)
//
// Widths in points: egui's zoom (Ctrl+= / Ctrl+-) is already in them, so a zoomed-in window has
// fewer points and the same rules apply. The sidebar leaves the center [`CENTER_ROOM_W`] while
// it can (down to its minimum). The center then gives way in this order: the member list
// collapses (its button shows it over the chat), the docked chat narrows to what leaves the
// stage its minimum, then goes (the cards stay, "Só o chat" still shows the chat; it comes back
// when the window has room).

/// The narrowest a chat (center or docked) is made to get while something beside it can give way.
pub const CHAT_MIN_W: f32 = 360.0;
/// The member list beside a channel's chat.
pub const MEMBERS_W: f32 = 232.0;
/// The gate's rule: a window narrower than this (points) never has the member list docked.
pub const MEMBERS_AUTO_W: f32 = 760.0;
/// The narrowest the call stage (the cards) is made to get beside the docked chat.
pub const STAGE_MIN_W: f32 = 260.0;
/// The docked chat's own range.
pub const DOCK_MIN_W: f32 = 280.0;
pub const DOCK_MAX_W: f32 = 640.0;
/// The sidebar's range.
pub const SIDEBAR_MIN_W: f32 = 200.0;
pub const SIDEBAR_MAX_W: f32 = 380.0;
/// What the sidebar leaves the center while it can: the docked chat beside the stage's minimum
/// in the smallest window (760 points).
pub const CENTER_ROOM_W: f32 = 560.0;

/// The member list is docked beside a chat in a center this wide (else it collapses).
pub fn members_fit(center_w: f32) -> bool {
    center_w - MEMBERS_W >= CHAT_MIN_W
}

/// The docked chat's width range in a center this wide, `None` when it does not fit beside the
/// stage's minimum.
pub fn dock_range(center_w: f32) -> Option<std::ops::RangeInclusive<f32>> {
    let max = (center_w - STAGE_MIN_W).min(DOCK_MAX_W);
    (max >= DOCK_MIN_W).then(|| DOCK_MIN_W..=max)
}

/// The sidebar's width range in a window this wide: it leaves the center [`CENTER_ROOM_W`]
/// while it can, never under its minimum.
pub fn sidebar_range(window_w: f32) -> std::ops::RangeInclusive<f32> {
    SIDEBAR_MIN_W..=(window_w - CENTER_ROOM_W).clamp(SIDEBAR_MIN_W, SIDEBAR_MAX_W)
}

/// The sidebar's width until the person drags it.
pub const SIDEBAR_W: f32 = 248.0;

/// A resizable side panel the window squeezed (its range's end under `want`, the width the person
/// chose) gets `want` back, within its range, once the window has room again: egui keeps a panel's
/// last width, so a width the window forced on it would otherwise stay. Not during a drag.
pub fn restore_panel_width(ctx: &eframe::egui::Context, id: eframe::egui::Id, want: f32, range: &std::ops::RangeInclusive<f32>) {
    if ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let fit = want.clamp(*range.start(), *range.end());
    if let Some(mut st) = eframe::egui::PanelState::load(ctx, id) {
        if st.size().x + 0.5 < fit {
            st.outer_rect.max.x = st.outer_rect.min.x + fit;
            ctx.data_mut(|d| d.insert_persisted(id, st));
        }
    }
}

/// The channel whose member list was asked for while it is collapsed (its button): shown over
/// that channel's chat until asked off, another channel opens or the window has room again.
/// Session only.
pub fn members_peek(ctx: &eframe::egui::Context) -> Option<ChannelId> {
    ctx.data(|d| d.get_temp::<Option<ChannelId>>(eframe::egui::Id::new("members_peek"))).flatten()
}

pub fn set_members_peek(ctx: &eframe::egui::Context, channel: Option<ChannelId>) {
    ctx.data_mut(|d| d.insert_temp(eframe::egui::Id::new("members_peek"), channel));
}

/// Autotests: the rects of the layout's main parts are noted ([`probe`]) for the checks.
static PROBING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_probing(on: bool) {
    PROBING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Note where `key` was drawn this frame (only while an autotest probes: nothing otherwise).
pub fn probe(ctx: &eframe::egui::Context, key: &'static str, rect: eframe::egui::Rect) {
    if PROBING.load(std::sync::atomic::Ordering::Relaxed) {
        let frame = ctx.cumulative_frame_nr();
        ctx.data_mut(|d| d.insert_temp(eframe::egui::Id::new(("layout_probe", key)), (frame, rect)));
    }
}

/// The frame and rect [`probe`] noted last for `key`.
pub fn probed(ctx: &eframe::egui::Context, key: &str) -> Option<(u64, eframe::egui::Rect)> {
    ctx.data(|d| d.get_temp(eframe::egui::Id::new(("layout_probe", key))))
}

/// How the center looks while we are in a call (remembered in settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CenterMode {
    /// Only the chat of the text channel (or DM) picked in the sidebar.
    Chat,
    /// Only the call stage (the cards). The default: the whole center for the cards on the
    /// default window size (the chat is one click away and remembered).
    #[default]
    Cards,
    /// The cards, with a normal text channel's chat docked on the right.
    CardsWithChat,
}

/// The shell's remembered choices (`Settings::layout`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    /// The last mode picked with the header buttons.
    pub center_mode: CenterMode,
    /// Width of the docked chat (points).
    pub dock_width: f32,
    /// The member list ("Online — N") on the right of the chat.
    pub show_members: bool,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self { center_mode: CenterMode::default(), dock_width: 340.0, show_members: true }
    }
}

/// What the center shows right now (and so what the sidebar highlights).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// Nothing picked yet.
    Nothing,
    /// A text channel's chat.
    Text(ChannelId),
    /// A DM conversation (its id).
    Dm(i64),
    /// The call stage of this voice channel (`dms::DM_CALL_CHANNEL` for a private call).
    Stage(ChannelId),
}

/// The inputs of [`open_in_center`], straight from the app's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    pub center: Center,
    /// The voice channel we are in or joining.
    pub call: Option<ChannelId>,
    pub text: Option<ChannelId>,
    pub dm: Option<i64>,
}

/// What is open in the center: the stage while it is in focus and we are in (or joining) a call,
/// else the DM or the text channel picked last.
pub fn open_in_center(f: Focus) -> Open {
    match (f.center, f.call) {
        (Center::Call, Some(ch)) => Open::Stage(ch),
        _ => match (f.dm, f.text) {
            (Some(dm), _) => Open::Dm(dm),
            (None, Some(t)) => Open::Text(t),
            (None, None) => Open::Nothing,
        },
    }
}

/// The mode in effect: Chat while the chat is in focus or there is no call; otherwise the
/// remembered stage mode (a remembered Chat shows the cards: the stage was asked for).
pub fn effective_mode(center: Center, in_call: bool, remembered: CenterMode) -> CenterMode {
    match (center, in_call, remembered) {
        (Center::Chat, _, _) | (_, false, _) => CenterMode::Chat,
        (Center::Call, true, CenterMode::CardsWithChat) => CenterMode::CardsWithChat,
        (Center::Call, true, _) => CenterMode::Cards,
    }
}

/// What a mode button puts in focus: the chat, or the stage.
pub fn center_for(mode: CenterMode) -> Center {
    match mode {
        CenterMode::Chat => Center::Chat,
        CenterMode::Cards | CenterMode::CardsWithChat => Center::Call,
    }
}

/// The text channel whose chat is on screen: the one open in the center, or the docked one beside
/// the cards (nothing while the stage fills the window). What arrives in it while the window is
/// in front counts as read.
pub fn text_on_screen(main_screen: bool, stage_fullscreen: bool, open: Open, mode: CenterMode, docked: Option<ChannelId>) -> Option<ChannelId> {
    if !main_screen {
        return None;
    }
    match (open, mode) {
        (Open::Text(c), _) => Some(c),
        (Open::Stage(_), CenterMode::CardsWithChat) if !stage_fullscreen => docked,
        _ => None,
    }
}

impl App {
    /// The voice channel we are in, or joining.
    pub fn call_channel(&self) -> Option<ChannelId> {
        self.voice.channel.or(self.voice.joining)
    }

    /// What the center shows (see [`open_in_center`]).
    pub fn open_in_center(&self) -> Open {
        open_in_center(Focus { center: self.center, call: self.call_channel(), text: self.selected, dm: self.dms.selected })
    }

    /// The mode in effect (see [`effective_mode`]). I6: the cards alone while the window has no
    /// room for the chat beside them (`CallUi::dock_squeezed`; the mode stays remembered).
    pub fn effective_mode(&self) -> CenterMode {
        match effective_mode(self.center, self.call_channel().is_some(), self.center_mode) {
            CenterMode::CardsWithChat if self.call.dock_squeezed => CenterMode::Cards,
            m => m,
        }
    }

    /// The text channel docked beside the cards: the one picked in the dock, else the last one
    /// viewed (always an existing text channel).
    pub fn docked_channel(&self) -> Option<ChannelId> {
        let text = |id: ChannelId| self.channels.iter().any(|c| c.id == id && c.kind == vox_proto::ChannelKind::Text);
        self.docked_chat_channel
            .filter(|c| text(*c))
            .or(self.selected.filter(|c| text(*c)))
            .or_else(|| self.channels.iter().find(|c| c.kind == vox_proto::ChannelKind::Text).map(|c| c.id))
    }

    /// The text channel whose chat is on screen (see [`text_on_screen`]).
    pub fn chat_on_screen(&self) -> Option<ChannelId> {
        let main = self.screen == crate::app::Screen::Main;
        text_on_screen(main, self.stage_fullscreen, self.open_in_center(), self.effective_mode(), self.docked_channel())
    }

    /// The chat whose input is on screen (a channel, or the DM open in the center).
    pub fn chat_target_on_screen(&self) -> Option<crate::chat_images::Target> {
        match self.open_in_center() {
            Open::Dm(dm) if self.screen == crate::app::Screen::Main => Some(crate::chat_images::Target::Dm(dm)),
            _ => self.chat_on_screen().map(crate::chat_images::Target::Channel),
        }
    }

    /// The center header's mode buttons: show that mode (and remember it). The modes with cards
    /// need a call.
    pub fn set_center_mode(&mut self, mode: CenterMode) {
        if mode != CenterMode::Chat && self.call_channel().is_none() {
            return;
        }
        self.center = center_for(mode);
        if mode == CenterMode::Chat && self.selected.is_none() && self.dms.selected.is_none() {
            if let Some(ch) = self.docked_channel() {
                self.select_text(ch);
            }
        }
        self.center_mode = mode;
        if self.settings.layout.center_mode != mode {
            self.settings.layout.center_mode = mode;
            self.save_settings();
        }
    }

    /// Pick the channel docked beside the cards.
    pub fn dock_channel(&mut self, ch: ChannelId) {
        self.docked_chat_channel = Some(ch);
        let log = self.logs.entry(ch).or_default();
        if !log.requested {
            log.requested = true;
            self.backend.send(crate::backend::Cmd::LoadHistory { channel: ch, before: None });
        }
    }

    /// The cards area. Phase 2b owns this: it fills `ui.max_rect()` (see the module docs).
    pub fn call_stage_ui(&mut self, ui: &mut eframe::egui::Ui) {
        self.call_view(ui);
    }

    /// The stage's own buttons in the center header (right to left, before the mode buttons):
    /// the stage fullscreen and "mostrar ocultos" (`call.rs`).
    pub fn stage_header_tools(&mut self, ui: &mut eframe::egui::Ui) {
        self.stage_tools(ui, false);
    }

    /// The quiet line after the room's name in the center header: "3 pessoas · 2 em foco · 1
    /// card oculto" (`call.rs`).
    pub fn stage_header_meta(&self) -> String {
        if self.members_here().is_empty() {
            return String::new();
        }
        self.stage_meta()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus(center: Center, call: Option<ChannelId>, text: Option<ChannelId>, dm: Option<i64>) -> Focus {
        Focus { center, call, text, dm }
    }

    #[test]
    fn the_stage_in_focus_takes_the_open_highlight_from_the_text_channel() {
        // text → voice → text
        assert_eq!(open_in_center(focus(Center::Chat, None, Some(1), None)), Open::Text(1));
        assert_eq!(open_in_center(focus(Center::Call, Some(5), Some(1), None)), Open::Stage(5));
        assert_eq!(open_in_center(focus(Center::Chat, Some(5), Some(1), None)), Open::Text(1));
        // A DM follows the same rule.
        assert_eq!(open_in_center(focus(Center::Chat, Some(5), Some(1), Some(9))), Open::Dm(9));
        assert_eq!(open_in_center(focus(Center::Call, Some(5), Some(1), Some(9))), Open::Stage(5));
        // No call: the stage cannot be open.
        assert_eq!(open_in_center(focus(Center::Call, None, Some(1), None)), Open::Text(1));
        assert_eq!(open_in_center(focus(Center::Chat, None, None, None)), Open::Nothing);
    }

    #[test]
    fn each_mode_highlights_only_what_the_center_shows() {
        // In a call (room 5), #1 picked last and docked.
        let (room, text) = (5, 1);
        for (mode, open) in [(CenterMode::Chat, Open::Text(text)), (CenterMode::Cards, Open::Stage(room)), (CenterMode::CardsWithChat, Open::Stage(room))] {
            let shown = open_in_center(focus(center_for(mode), Some(room), Some(text), None));
            assert_eq!(shown, open, "{mode:?}");
            assert_eq!(effective_mode(center_for(mode), true, mode), mode);
        }
        // Beside the cards the docked channel is on screen but never the open (highlighted) one.
        let open = open_in_center(focus(center_for(CenterMode::CardsWithChat), Some(room), Some(text), None));
        assert_ne!(open, Open::Text(text));
        assert_eq!(text_on_screen(true, false, open, CenterMode::CardsWithChat, Some(text)), Some(text));
    }

    #[test]
    fn the_docked_chat_is_on_screen_only_beside_the_cards() {
        use CenterMode::*;
        // Chat in the center: that channel, whatever is docked.
        assert_eq!(text_on_screen(true, false, Open::Text(1), Chat, Some(2)), Some(1));
        // The stage with the chat docked: the docked one (so its messages count as read).
        assert_eq!(text_on_screen(true, false, Open::Stage(5), CardsWithChat, Some(2)), Some(2));
        // Cards only, the stage fullscreen, a DM, the home screen: no channel is being read.
        assert_eq!(text_on_screen(true, false, Open::Stage(5), Cards, Some(2)), None);
        assert_eq!(text_on_screen(true, true, Open::Stage(5), CardsWithChat, Some(2)), None);
        assert_eq!(text_on_screen(true, false, Open::Dm(9), Chat, Some(2)), None);
        assert_eq!(text_on_screen(false, false, Open::Text(1), Chat, Some(2)), None);
    }

    #[test]
    fn modes_with_cards_need_a_call_and_the_stage_in_focus() {
        use CenterMode::*;
        for m in [Chat, Cards, CardsWithChat] {
            assert_eq!(effective_mode(Center::Chat, true, m), Chat);
            assert_eq!(effective_mode(Center::Call, false, m), Chat);
        }
        assert_eq!(effective_mode(Center::Call, true, Cards), Cards);
        assert_eq!(effective_mode(Center::Call, true, CardsWithChat), CardsWithChat);
        assert_eq!(effective_mode(Center::Call, true, Chat), Cards);
    }
    #[test]
    fn the_member_list_collapses_when_the_chat_would_get_too_narrow() {
        // The smallest window zoomed in (760 px at 125%: 608 points), with the sidebar at its
        // minimum: collapsed. A normal window (1060 points, the default sidebar): docked.
        assert!(!members_fit(608.0 - SIDEBAR_MIN_W));
        assert!(!members_fit(MEMBERS_AUTO_W - SIDEBAR_MIN_W));
        assert!(members_fit(1060.0 - 248.0));
        assert!(members_fit(CHAT_MIN_W + MEMBERS_W));
        assert!(!members_fit(CHAT_MIN_W + MEMBERS_W - 1.0));
    }

    #[test]
    fn the_docked_chat_leaves_the_stage_its_minimum_or_goes() {
        // The smallest window (760 points): the sidebar leaves the center 560, the chat fits.
        let center = 760.0 - *sidebar_range(760.0).end();
        let r = dock_range(center).expect("the dock fits in the smallest window");
        assert!(*r.start() >= DOCK_MIN_W && center - *r.end() >= STAGE_MIN_W);
        // Zoomed in (608 points): no room for both, the cards stay.
        assert_eq!(dock_range(608.0 - SIDEBAR_MIN_W), None);
        // A wide window: up to its maximum.
        assert_eq!(dock_range(2000.0), Some(DOCK_MIN_W..=DOCK_MAX_W));
    }

    #[test]
    fn a_panel_the_window_squeezed_gets_its_width_back() {
        use eframe::egui::{pos2, vec2, Context, Id, PanelState, Rect};
        let ctx = Context::default();
        let id = Id::new("left");
        let width = |ctx: &Context| PanelState::load(ctx, id).map(|s| s.size().x);
        ctx.data_mut(|d| d.insert_persisted(id, PanelState { outer_rect: Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 400.0)) }));
        // Still no room: it stays squeezed.
        restore_panel_width(&ctx, id, 248.0, &sidebar_range(760.0));
        assert_eq!(width(&ctx), Some(200.0));
        // Room again: the width the person chose, within the range.
        restore_panel_width(&ctx, id, 248.0, &sidebar_range(1280.0));
        assert_eq!(width(&ctx), Some(248.0));
        restore_panel_width(&ctx, id, 500.0, &sidebar_range(860.0));
        assert_eq!(width(&ctx), Some(300.0));
        // A panel already as wide (or wider) is left alone.
        restore_panel_width(&ctx, id, 248.0, &sidebar_range(1920.0));
        assert_eq!(width(&ctx), Some(300.0));
    }

    #[test]
    fn the_sidebar_leaves_the_center_its_room_while_it_can() {
        assert_eq!(sidebar_range(760.0), SIDEBAR_MIN_W..=SIDEBAR_MIN_W);
        assert_eq!(sidebar_range(507.0), SIDEBAR_MIN_W..=SIDEBAR_MIN_W);
        assert_eq!(sidebar_range(860.0), SIDEBAR_MIN_W..=300.0);
        assert_eq!(sidebar_range(1920.0), SIDEBAR_MIN_W..=SIDEBAR_MAX_W);
    }
}
