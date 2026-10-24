//! Chat notifications and the tray: who deserves a sound, a counter, a taskbar flash, a badge and
//! a Windows notification, and the window that hides in the tray instead of quitting.
//!
//! The rules are pure functions ([`plan`], [`badge_count`], [`preview`], [`BalloonGate`]) so they
//! are unit tested; the Win32 side is `tray.rs`. A child module of `app` (declared there), so it
//! uses the app's private helpers.
//!
//! What counts as "reading": the conversation is open in the chat AND the window has focus. A
//! message for something you are reading is silent. Everything else counts as unread (sidebar
//! counter, taskbar badge) and plays its sound. The Windows notification (taskbar flash,
//! balloon) is only for when the window is not in front, and follows the setting "Notificações do
//! Windows". A message that mentions me (v0.8: by id, `@todos` from staff, or a reply with
//! "@ ligado") has its own sound, counts for "Só mensagens diretas e menções" and puts an `@` on
//! the channel's counter.

use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use vox_client_core::AlertKind;
use vox_proto::{ChannelId, DmId, DmMessage, Message};

use super::{App, Center, Screen};
use crate::tray::{Tray, TrayEvent};

// ---------------------------------------------------------------- settings

/// "Notificações do Windows".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WinNotify {
    /// Every message in every channel.
    All,
    Off,
    /// Direct messages and mentions only. (A value this version does not know reads as this.)
    #[default]
    #[serde(other)]
    DmsAndMentions,
}

impl WinNotify {
    pub const ALL: [WinNotify; 3] = [WinNotify::All, WinNotify::DmsAndMentions, WinNotify::Off];

    pub fn label(self) -> &'static str {
        match self {
            WinNotify::All => "Todas as mensagens",
            WinNotify::DmsAndMentions => "Só mensagens diretas e menções",
            WinNotify::Off => "Desligadas",
        }
    }
}

/// `settings.json` -> `notify`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifySettings {
    pub windows: WinNotify,
    /// "Ao fechar, minimizar para a bandeja" (on by default; older files get the default).
    pub tray_on_close: bool,
    /// The one-time "o concord continua na bandeja" balloon was shown already.
    pub tray_hint_shown: bool,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self { windows: WinNotify::default(), tray_on_close: true, tray_hint_shown: false }
    }
}

// ---------------------------------------------------------------- the rules (pure)

/// One message that just arrived, as far as the rules care.
#[derive(Debug, Clone, Copy)]
pub struct Incoming {
    pub is_dm: bool,
    /// The message mentions me (v0.8).
    pub is_mention: bool,
    /// I wrote it (another device of mine, or just now).
    pub mine: bool,
    /// The author is somebody I blocked.
    pub blocked: bool,
    /// Its channel / DM is the one open in the chat.
    pub open: bool,
    /// The window is in front.
    pub focused: bool,
}

/// What to do about an [`Incoming`] message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Count it as unread (sidebar counter, taskbar badge).
    pub unread: bool,
    /// The chat sound to ask for (the alert settings and "Não perturbe" still apply).
    pub sound: Option<AlertKind>,
    /// Flash the taskbar button and raise a Windows notification.
    pub toast: bool,
}

pub fn plan(mode: WinNotify, dnd: bool, i: &Incoming) -> Plan {
    const NOTHING: Plan = Plan { unread: false, sound: None, toast: false };
    if i.mine || i.blocked || (i.open && i.focused) {
        return NOTHING;
    }
    let sound = if i.is_mention {
        AlertKind::Mention
    } else if i.is_dm {
        AlertKind::DirectMessage
    } else {
        AlertKind::ChannelMessage
    };
    let wanted = match mode {
        WinNotify::All => true,
        WinNotify::DmsAndMentions => i.is_dm || i.is_mention,
        WinNotify::Off => false,
    };
    Plan { unread: true, sound: Some(sound), toast: wanted && !dnd && !i.focused }
}

/// The number on the taskbar button: what the setting says is worth a notification, unread.
/// (`mention_unread` is part of `channel_unread`.)
pub fn badge_count(mode: WinNotify, dm_unread: u32, channel_unread: u32, mention_unread: u32) -> u32 {
    match mode {
        WinNotify::Off => 0,
        WinNotify::DmsAndMentions => dm_unread.saturating_add(mention_unread),
        WinNotify::All => dm_unread.saturating_add(channel_unread),
    }
}

const PREVIEW_CHARS: usize = 120;

/// The one-line body of a notification: the text on one line, cut short; images get a sentence.
pub fn preview(caption: &str, images: usize) -> String {
    let text = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return match images {
            0 => String::new(),
            1 => "Enviou uma imagem".into(),
            n => format!("Enviou {n} imagens"),
        };
    }
    if text.chars().count() <= PREVIEW_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(PREVIEW_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// The title of a notification: the author, and where they wrote when it is a channel.
pub fn toast_title(author: &str, channel: Option<&str>) -> String {
    match channel {
        Some(c) => format!("{author} em #{c}"),
        None => author.to_string(),
    }
}

/// The title of a mention's notification.
pub fn mention_title(author: &str, channel: &str) -> String {
    format!("{author} mencionou você em #{channel}")
}

/// Windows queues balloons and shows each for a few seconds: a burst of messages gets one.
pub const BALLOON_GAP: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct BalloonGate {
    last: Option<Instant>,
}

impl BalloonGate {
    pub fn admit(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < BALLOON_GAP) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

// ---------------------------------------------------------------- sidebar

/// Height of the red counter pill; the amber "@" is a circle of the same size.
pub const PILL_H: f32 = 16.0;
/// Space between the row's right edge and the badges, between badges, and before the name ends.
const ROW_PAD: f32 = 6.0;
const BADGE_GAP: f32 = 3.0;

/// Where the parts of a sidebar row go (v0.8.2): the name on the left, ending before the badges
/// (it is truncated with "…" when it does not fit), and the badges right-aligned in space reserved
/// for them: the counter at the right end, the "@" left of it, then an icon slot (the 📞 of a DM
/// in a call). Pure, so it is unit tested (v0.8.1 drew the counter over the name's end).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowLayout {
    pub row: egui::Rect,
    /// Where the name may be drawn (zero width when a very narrow row has room for badges only).
    pub name: egui::Rect,
    pub count: Option<egui::Rect>,
    pub mention: Option<egui::Rect>,
    pub icon: Option<egui::Rect>,
}

impl RowLayout {
    pub fn badges(&self) -> impl Iterator<Item = egui::Rect> {
        [self.count, self.mention, self.icon].into_iter().flatten()
    }

    /// No badge overlaps the name's space (a positive-area overlap; touching edges is fine).
    pub fn name_is_clear(&self) -> bool {
        self.badges().all(|b| !self.name.intersect(b).is_positive())
    }
}

/// The counter's text: "1".."99", then "99+".
pub fn count_text(unread: u32) -> String {
    if unread > 99 { "99+".to_string() } else { unread.to_string() }
}

/// The counter pill's width for a text this wide.
pub fn pill_width(text_w: f32) -> f32 {
    (text_w + 10.0).max(18.0)
}

/// Lay out a row: `name_left` is where the name starts (after an avatar, say); `count_w` the
/// counter pill's width (none: no counter), `mention` the "@", `icon_w` an extra icon slot (0: none).
pub fn row_layout(row: egui::Rect, name_left: f32, count_w: Option<f32>, mention: bool, icon_w: f32) -> RowLayout {
    let name_left = name_left.clamp(row.left(), row.right());
    let cy = row.center().y;
    let mut right = row.right() - ROW_PAD;
    let mut place = |w: f32| {
        let r = egui::Rect::from_min_max(egui::pos2(right - w, cy - PILL_H / 2.0), egui::pos2(right, cy + PILL_H / 2.0));
        right = r.left() - BADGE_GAP;
        r
    };
    let count = count_w.map(&mut place);
    let mention = mention.then(|| place(PILL_H));
    let icon = (icon_w > 0.0).then(|| place(icon_w));
    let any = count.is_some() || mention.is_some() || icon.is_some();
    // The name ends a little before the first badge (or before the row's padding).
    let name_right = if any { right + BADGE_GAP - ROW_PAD } else { right };
    let name = egui::Rect::from_min_max(egui::pos2(name_left, row.top()), egui::pos2(name_right.max(name_left), row.bottom()));
    RowLayout { row, name, count, mention, icon }
}

/// Paint the badges of a laid-out row (the v1.0 look, `ui_kit::paint_badges`).
pub fn paint_badges(painter: &egui::Painter, l: &RowLayout, unread: u32) {
    crate::ui_kit::paint_badges(painter, l, unread);
}

/// A row's name laid out to fit `max_w` on one line ("…" at the end when it does not).
pub fn fit_name(painter: &egui::Painter, text: &str, font: egui::FontId, color: egui::Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
    job.wrap.max_width = max_w.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    painter.layout_job(job)
}

/// What a drawn sidebar row looked like (the autotest checks it).
#[derive(Debug, Clone, PartialEq)]
pub struct DrawnRow {
    pub label: String,
    pub layout: RowLayout,
    /// Where the drawn name really ends, and whether it was cut ("…").
    pub text_right: f32,
    pub elided: bool,
    /// Drawn with the "open" highlight (v1.0: only what the center shows).
    pub selected: bool,
}

/// Unread messages remembered per channel (to uncount deleted ones); older ones just stay counted.
const UNREAD_IDS_MAX: usize = 500;

// ---------------------------------------------------------------- state

/// What a notification opens when it is clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Channel(ChannelId),
    Dm(DmId),
}

/// What happened, for the autotest and diagnostics.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Chat sounds that passed the settings and the debounce (names of the `AlertKind`s).
    pub sounds: Vec<&'static str>,
    pub flashes: u32,
    pub balloons: u32,
    pub last_balloon: Option<(String, String)>,
    /// The number the taskbar badge shows now.
    pub badge: u32,
    pub hides: u32,
    pub restores: u32,
}

pub struct NotifyUi {
    tray: Option<Tray>,
    rx: Receiver<TrayEvent>,
    tx: std::sync::mpsc::Sender<TrayEvent>,
    tray_retry_at: Option<Instant>,
    /// The window is hidden in the tray (no ui passes run, only `logic()`).
    pub hidden: bool,
    /// "Sair" or an autotest finishing: closing the window really closes it.
    quitting: bool,
    /// Real Windows effects (flash, balloon). Off in the autotests, which only record them.
    real_effects: bool,
    /// Real tray icon and taskbar badge. Off in the autotests unless `VOX_TRAY_TEST=1`.
    real_tray: bool,
    /// Autotest: pretend the window has / has not the focus.
    pub focus_override: Option<bool>,
    last_target: Option<Target>,
    gate: BalloonGate,
    badge_sent: u32,
    badge_resend_at: Option<Instant>,
    pub stats: Stats,
}

impl NotifyUi {
    pub fn new(persist: bool) -> Self {
        let env = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0");
        let (tx, rx) = channel();
        Self {
            tray: None,
            rx,
            tx,
            tray_retry_at: None,
            hidden: false,
            quitting: false,
            real_effects: persist || env("VOX_NOTIFY_REAL"),
            real_tray: persist || env("VOX_NOTIFY_REAL"),
            focus_override: None,
            last_target: None,
            gate: BalloonGate::default(),
            badge_sent: 0,
            badge_resend_at: None,
            stats: Stats::default(),
        }
    }

    /// Closing the window now really closes the app (an autotest is done, or "Sair").
    pub fn allow_quit(&mut self) {
        self.quitting = true;
    }

    /// "Sair" was cancelled (the host's "your server goes down" question, `host.rs`).
    pub fn cancel_quit(&mut self) {
        self.quitting = false;
    }

    /// Autotest: this client may have the real tray icon and badge (`VOX_TRAY_TEST=1`). Returns
    /// whether it does.
    pub fn allow_real_tray(&mut self) -> bool {
        self.real_tray |= std::env::var("VOX_TRAY_TEST").is_ok_and(|v| !v.is_empty() && v != "0");
        self.real_tray
    }

    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    /// Autotest: click the tray icon / use its menu / click the balloon.
    pub fn simulate(&self, ev: TrayEvent) {
        if let Some(t) = &self.tray {
            t.simulate(ev);
        }
    }

    /// The window hides on close: the tray exists to bring it back, and the setting is on.
    pub fn hides_on_close(&self, on: bool) -> bool {
        on && self.tray.is_some() && !self.quitting
    }
}

// ---------------------------------------------------------------- the app side

impl App {
    /// The window is in front (and shown).
    pub fn window_focused(&self) -> bool {
        if let Some(f) = self.notify.focus_override {
            return f;
        }
        !self.notify.hidden && self.repaint.ctx().is_some_and(|c| c.input(|i| i.viewport().focused.unwrap_or(false)))
    }

    /// The channel's chat is on screen: open in the center, or docked beside the cards (focus
    /// aside; `layout::text_on_screen`).
    fn channel_open(&self, ch: ChannelId) -> bool {
        self.chat_on_screen() == Some(ch)
    }

    /// A message arrived in a text channel (the hub sends them all, open or not).
    pub(super) fn channel_message_arrived(&mut self, m: &Message) {
        let Some(me) = self.my_id() else { return };
        let is_mention = self.chatx.features.mentions && m.mentions_me(me);
        let inc = Incoming {
            is_dm: false,
            is_mention,
            mine: m.author.id == me,
            blocked: self.dms.is_blocked(m.author.id),
            open: self.channel_open(m.channel_id),
            focused: self.window_focused(),
        };
        let p = plan(self.settings.notify.windows, self.do_not_disturb(), &inc);
        if p.unread {
            *self.unread.entry(m.channel_id).or_default() += 1;
            if is_mention {
                *self.chatx.mention_unread.entry(m.channel_id).or_default() += 1;
            }
            // Which messages the counter stands for: a deleted one takes its count with it.
            let ids = self.chatx.unread_ids.entry(m.channel_id).or_default();
            ids.push((m.id, is_mention));
            if ids.len() > UNREAD_IDS_MAX {
                ids.remove(0);
            }
        }
        if let Some(kind) = p.sound {
            self.chat_sound(kind, m.author.id);
        }
        if p.toast {
            let channel = self.channels.iter().find(|c| c.id == m.channel_id).map(|c| c.name.clone());
            let title = match (&channel, is_mention) {
                (Some(c), true) => mention_title(m.author.shown_name(), c),
                _ => toast_title(m.author.shown_name(), channel.as_deref()),
            };
            self.raise_toast(Target::Channel(m.channel_id), title, preview(m.caption(), m.attachments.len()));
        }
    }

    /// A DM line from somebody else arrived and is not being read (`dms.rs` decided that and
    /// plays the sound): the taskbar flash and the Windows notification.
    pub(super) fn dm_message_arrived(&mut self, m: &DmMessage, me: i64) {
        let inc = Incoming {
            is_dm: true,
            is_mention: false,
            mine: m.author.id == me,
            blocked: self.dms.is_blocked(m.author.id),
            open: self.dms.selected == Some(m.dm_id) && self.screen == Screen::Main && self.center == Center::Chat,
            focused: self.window_focused(),
        };
        let p = plan(self.settings.notify.windows, self.do_not_disturb(), &inc);
        if p.toast {
            self.dms.flashes += 1;
            // v1.1 (E2): an encrypted line shows its text once opened here, or only that it came.
            let body = if m.kind.is_call() {
                super::dms::call_line_text(m, me)
            } else if crate::backend::e2ee::dm::is_unreadable(m) {
                crate::backend::e2ee::dm::NOTIFY_TEXT.to_string()
            } else {
                preview(m.caption(), m.attachments.len())
            };
            self.raise_toast(Target::Dm(m.dm_id), toast_title(m.author.shown_name(), None), body);
        }
    }

    fn chat_sound(&mut self, kind: AlertKind, who: i64) {
        if self.remote_alert(kind, who) {
            self.notify.stats.sounds.push(kind.name());
        }
    }

    /// Flash the taskbar button (until the window is focused) and show a Windows notification.
    fn raise_toast(&mut self, target: Target, title: String, body: String) {
        self.notify.last_target = Some(target);
        self.platform_flash();
        if self.notify.gate.admit(Instant::now()) {
            self.notify.stats.balloons += 1;
            tracing::info!(title = %title, "windows notification");
            if self.notify.real_effects {
                if let Some(t) = &self.notify.tray {
                    t.balloon(&title, &body);
                }
            }
            self.notify.stats.last_balloon = Some((title, body));
        }
    }

    /// `FlashWindowEx` on the main window: blinks until the user brings it to the front.
    pub(super) fn platform_flash(&mut self) {
        self.notify.stats.flashes += 1;
        if !self.notify.real_effects {
            return;
        }
        #[cfg(windows)]
        if let Some(hwnd) = self.video.parent {
            use windows_sys::Win32::UI::WindowsAndMessaging::{FlashWindowEx, FLASHWINFO, FLASHW_TIMERNOFG, FLASHW_TRAY};
            let info = FLASHWINFO { cbSize: std::mem::size_of::<FLASHWINFO>() as u32, hwnd: hwnd as _, dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG, uCount: 0, dwTimeout: 0 };
            // SAFETY: a plain call with a filled struct; the window is ours.
            unsafe { FlashWindowEx(&info) };
        }
    }

    // ------------------------------------------------------------ the tray

    /// Hide the window: the app (and a call) keeps running, `logic()` keeps ticking.
    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        tracing::info!("window hidden in the tray");
        self.notify.hidden = true;
        self.notify.stats.hides += 1;
        self.repaint.set_hidden(true);
        match self.video.parent.filter(|_| vox_client_core::testmode::background()) {
            // Autotests: natively, so the restore below can show it again without activating it.
            Some(hwnd) => vox_client_core::testmode::hide(hwnd),
            None => ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false)),
        }
        if !self.settings.notify.tray_hint_shown && self.notify.real_effects {
            self.settings.notify.tray_hint_shown = true;
            self.save_settings();
            if let Some(t) = &self.notify.tray {
                t.balloon("O concord continua aberto", "Ele ficou na bandeja do sistema. Clique no ícone para abrir ou use o botão direito para sair.");
            }
        }
    }

    /// The `donate` autotest: into the tray, as closing the window does.
    #[cfg(feature = "donate-test")]
    pub fn hide_for_test(&mut self) {
        if let Some(c) = self.repaint.ctx() {
            self.hide_to_tray(&c);
        }
    }

    /// Bring the window back to the front (tray click, "Abrir concord", a clicked notification).
    pub fn show_window(&mut self, ctx: &egui::Context) {
        tracing::info!(was_hidden = self.notify.hidden, "window restored from the tray");
        self.notify.hidden = false;
        self.notify.stats.restores += 1;
        self.repaint.set_hidden(false);
        match self.video.parent.filter(|_| vox_client_core::testmode::background()) {
            // Autotests (also with the real tray, `VOX_TRAY_TEST`): shown again without taking the
            // foreground (`SW_SHOWNOACTIVATE`), never focused.
            Some(hwnd) => vox_client_core::testmode::show_behind(hwnd),
            None => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        // The taskbar button is new after a hide: put the badge on it again.
        self.notify.badge_resend_at = Some(Instant::now() + Duration::from_millis(600));
        ctx.request_repaint_after(Duration::from_millis(650));
    }

    /// Per-frame work, from `logic()` (which keeps running while the window is hidden).
    pub(super) fn notify_tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.sync_tray(now);
        // v1.1: concord was opened again (`single_instance`): bring this one to the front.
        if crate::single_instance::take_show() {
            self.show_window(ctx);
        }

        while let Ok(ev) = self.notify.rx.try_recv() {
            tracing::info!(?ev, "tray event");
            match ev {
                TrayEvent::Open => self.show_window(ctx),
                TrayEvent::BalloonClicked => {
                    self.show_window(ctx);
                    match self.notify.last_target {
                        Some(Target::Channel(ch)) if self.channels.iter().any(|c| c.id == ch) => self.select_text(ch),
                        Some(Target::Dm(dm)) if self.dms.conv(dm).is_some() => self.select_dm(dm),
                        _ => {}
                    }
                }
                TrayEvent::Quit => {
                    self.notify.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // The close button: hide instead (when the tray is there to bring the window back).
        if ctx.input(|i| i.viewport().close_requested()) && self.notify.hides_on_close(self.settings.notify.tray_on_close) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray(ctx);
        }

        // Coming back to the front reads the open channel.
        if !self.unread.is_empty() && self.window_focused() {
            if let Some(ch) = self.chat_on_screen() {
                self.channel_read(ch);
            }
        }

        let want = badge_count(self.settings.notify.windows, self.dms.total_unread(), self.unread.values().sum(), self.chatx.total_mentions());
        let due = self.notify.badge_resend_at.is_some_and(|t| now >= t);
        if want != self.notify.badge_sent || (due && want > 0) {
            if want != self.notify.badge_sent {
                tracing::info!(count = want, "taskbar badge");
            }
            self.notify.badge_sent = want;
            self.notify.stats.badge = want;
            if let Some(t) = self.notify.tray.as_ref().filter(|_| self.notify.real_tray) {
                t.set_badge(want);
            }
        }
        if due {
            self.notify.badge_resend_at = None;
        }
    }

    /// Create the tray icon when something needs it, remove it when nothing does.
    fn sync_tray(&mut self, now: Instant) {
        let n = &self.settings.notify;
        let wanted = self.notify.real_tray && (n.tray_on_close || n.windows != WinNotify::Off);
        if !wanted && self.notify.tray.is_some() && !self.notify.hidden {
            self.notify.tray = None;
        }
        if !wanted || self.notify.tray.is_some() || self.notify.tray_retry_at.is_some_and(|t| now < t) {
            return;
        }
        let Some(hwnd) = self.video.parent else { return };
        let r = self.repaint.clone();
        match Tray::start(hwnd, "concord", self.notify.tx.clone(), move || r.wake()) {
            Ok(t) => {
                tracing::info!("tray icon ready");
                self.notify.tray = Some(t);
                self.notify.badge_sent = 0;
            }
            Err(e) => {
                // Explorer may not be up yet (autostart at login): try again in a while. Meanwhile
                // closing the window quits, since nothing could bring it back.
                tracing::warn!(error = %e, "no tray icon");
                self.notify.tray_retry_at = Some(now + Duration::from_secs(20));
            }
        }
    }

    pub(super) fn notify_on_exit(&mut self) {
        self.notify.tray = None; // removes the icon and the badge
        // Opening concord again from here on starts a new one (`single_instance::release`).
        crate::single_instance::release();
    }
    // (The settings of this module, "Notificações do Windows" and the tray, are drawn in
    // Configurações → Notificações: `settings_ui.rs`.)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.8.1 drew the counter over the name. Long names, narrow sidebars, "@" + counter, "99+",
    /// a DM's avatar and call icon: the badges never cover the name's space, the name keeps the
    /// space left of them, and nothing leaves the row.
    #[test]
    fn sidebar_badges_never_cover_the_name() {
        let rows = [180.0f32, 200.0, 235.0, 320.0, 60.0, 30.0];
        for w in rows {
            let row = egui::Rect::from_min_size(egui::pos2(10.0, 50.0), egui::vec2(w, 20.0));
            for (count, mention, icon, indent) in [
                (None, false, 0.0, 4.0),
                (Some(pill_width(7.0)), false, 0.0, 4.0),
                (Some(pill_width(20.0)), true, 0.0, 4.0),
                (Some(pill_width(20.0)), true, 16.0, 4.0 + 28.0),
                (None, false, 16.0, 30.0),
                (None, true, 0.0, 4.0),
            ] {
                let l = row_layout(row, row.left() + indent, count, mention, icon);
                assert!(l.name_is_clear(), "w {w}: {l:?}");
                assert!(l.name.left() >= row.left() && l.name.right() <= row.right() && l.name.width() >= 0.0, "w {w}: {l:?}");
                for b in l.badges() {
                    assert!(b.right() <= row.right() && (b.center().y - row.center().y).abs() < 0.01, "w {w}: {b:?}");
                }
                let badges: Vec<egui::Rect> = l.badges().collect();
                for (i, a) in badges.iter().enumerate() {
                    for b in &badges[i + 1..] {
                        assert!(!a.intersect(*b).is_positive(), "badges overlap each other: {a:?} {b:?}");
                    }
                }
                if count.is_none() && !mention && icon == 0.0 {
                    assert_eq!(l.name.right(), row.right() - ROW_PAD, "no badges: the whole row is the name's");
                }
                if w >= 180.0 {
                    assert!(l.name.width() > 50.0, "a normal sidebar keeps room for the name: {l:?}");
                }
            }
        }
        // The "@" sits left of the counter, the icon left of both.
        let l = row_layout(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(235.0, 20.0)), 6.0, Some(24.0), true, 16.0);
        let (c, m, i) = (l.count.unwrap(), l.mention.unwrap(), l.icon.unwrap());
        assert!(i.right() < m.left() && m.right() < c.left() && l.name.right() < i.left());
        assert_eq!(count_text(7), "7");
        assert_eq!(count_text(150), "99+");
        assert_eq!(pill_width(2.0), 18.0);
    }

    /// The real font: a long name is cut with "…" and its drawn end stays left of the badges.
    #[test]
    fn a_long_channel_name_is_cut_before_the_badges() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut drawn = Vec::new();
        let mut run = |events: Vec<egui::Event>| {
            let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(235.0, 200.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                drawn.clear();
                for (label, unread, mentions) in [
                    ("# um-canal-com-um-nome-muito-comprido-mesmo-de-verdade", 150u32, 3u32),
                    ("# geral", 2, 0),
                    ("# outro-canal-de-nome-longo-sem-mencoes-aqui", 5, 0),
                    ("# lido", 0, 0),
                ] {
                    let name = label.trim_start_matches("# ");
                    let row = crate::ui_kit::Row { icon: crate::theme::ph::HASH, name, label, selected: false, unread: unread > 0, count: unread, mentions, hover: None };
                    drawn.push(crate::ui_kit::sidebar_row(ui, row).1);
                }
            });
            out.textures_delta.clear();
        };
        run(vec![]);
        run(vec![]);
        assert_eq!(drawn.len(), 4);
        for d in &drawn {
            assert!(d.layout.name_is_clear(), "{d:?}");
            assert!(d.text_right <= d.layout.name.right() + 0.5, "the drawn name runs under the badges: {d:?}");
        }
        assert!(drawn[0].elided && drawn[0].layout.count.is_some() && drawn[0].layout.mention.is_some(), "{:?}", drawn[0]);
        assert!(!drawn[1].elided && drawn[1].layout.mention.is_none());
        assert!(drawn[2].elided);
        assert!(drawn[3].layout.count.is_none() && !drawn[3].elided);
    }

    fn inc(is_dm: bool) -> Incoming {
        Incoming { is_dm, is_mention: false, mine: false, blocked: false, open: false, focused: false }
    }

    #[test]
    fn never_for_my_own_messages_or_blocked_people() {
        for mode in WinNotify::ALL {
            for dm in [false, true] {
                let mine = Incoming { mine: true, ..inc(dm) };
                let blocked = Incoming { blocked: true, ..inc(dm) };
                for i in [mine, blocked] {
                    assert_eq!(plan(mode, false, &i), Plan { unread: false, sound: None, toast: false }, "{mode:?} {i:?}");
                }
            }
        }
    }

    #[test]
    fn the_open_and_focused_conversation_is_silent() {
        let reading = Incoming { open: true, focused: true, ..inc(false) };
        assert_eq!(plan(WinNotify::All, false, &reading), Plan { unread: false, sound: None, toast: false });
        // Open but the window is behind another: it is news (sound, unread, notification).
        let behind = Incoming { open: true, focused: false, ..inc(false) };
        let p = plan(WinNotify::All, false, &behind);
        assert!(p.unread && p.toast && p.sound == Some(AlertKind::ChannelMessage));
        // In front but looking at another conversation: a sound and a counter, no Windows notice.
        let elsewhere = Incoming { open: false, focused: true, ..inc(false) };
        let p = plan(WinNotify::All, false, &elsewhere);
        assert!(p.unread && !p.toast && p.sound.is_some());
    }

    #[test]
    fn the_setting_decides_what_reaches_windows() {
        let chan = inc(false);
        let dm = inc(true);
        let mention = Incoming { is_mention: true, ..inc(false) };
        let toast = |mode, i: &Incoming| plan(mode, false, i).toast;
        assert!(toast(WinNotify::All, &chan) && toast(WinNotify::All, &dm) && toast(WinNotify::All, &mention));
        assert!(!toast(WinNotify::DmsAndMentions, &chan));
        assert!(toast(WinNotify::DmsAndMentions, &dm) && toast(WinNotify::DmsAndMentions, &mention));
        assert!(!toast(WinNotify::Off, &chan) && !toast(WinNotify::Off, &dm) && !toast(WinNotify::Off, &mention));
        // Off silences Windows only: the in-app counter and the sound are separate settings.
        let p = plan(WinNotify::Off, false, &dm);
        assert!(p.unread && p.sound.is_some());
        // "Não perturbe" silences Windows too.
        assert!(!plan(WinNotify::All, true, &dm).toast);
    }

    #[test]
    fn each_kind_of_message_has_its_own_sound() {
        let kind = |i: Incoming| plan(WinNotify::Off, false, &i).sound;
        assert_eq!(kind(inc(false)), Some(AlertKind::ChannelMessage));
        assert_eq!(kind(inc(true)), Some(AlertKind::DirectMessage));
        assert_eq!(kind(Incoming { is_mention: true, ..inc(false) }), Some(AlertKind::Mention));
        assert_eq!(kind(Incoming { is_mention: true, ..inc(true) }), Some(AlertKind::Mention), "a mention beats the kind");
    }

    #[test]
    fn the_badge_counts_what_the_setting_cares_about() {
        assert_eq!(badge_count(WinNotify::Off, 3, 4, 1), 0);
        assert_eq!(badge_count(WinNotify::DmsAndMentions, 3, 4, 0), 3);
        assert_eq!(badge_count(WinNotify::DmsAndMentions, 3, 4, 2), 5, "mentions count with DMs");
        assert_eq!(badge_count(WinNotify::All, 3, 4, 2), 7, "a mention is already one of the channel lines");
        assert_eq!(badge_count(WinNotify::All, u32::MAX, 5, 0), u32::MAX);
        assert_eq!(mention_title("Ana", "geral"), "Ana mencionou você em #geral");
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(preview("  oi \n  tudo   bem? ", 0), "oi tudo bem?");
        assert_eq!(preview("", 1), "Enviou uma imagem");
        assert_eq!(preview("", 3), "Enviou 3 imagens");
        assert_eq!(preview("olha isso", 2), "olha isso");
        let long = "palavra ".repeat(60);
        let p = preview(&long, 0);
        assert_eq!(p.chars().count(), PREVIEW_CHARS);
        assert!(p.ends_with("palavra…"), "{p}");
        // Multi-byte text is cut on characters, never inside one.
        let acentos = "ação ".repeat(60);
        assert!(preview(&acentos, 0).ends_with('…'));
        assert_eq!(toast_title("Ana", Some("geral")), "Ana em #geral");
        assert_eq!(toast_title("Ana", None), "Ana");
    }

    #[test]
    fn a_burst_gets_one_balloon_per_gap() {
        let t0 = Instant::now();
        let mut g = BalloonGate::default();
        assert!(g.admit(t0));
        assert!(!g.admit(t0 + Duration::from_millis(500)));
        assert!(!g.admit(t0 + BALLOON_GAP - Duration::from_millis(1)));
        assert!(g.admit(t0 + BALLOON_GAP));
    }

    #[test]
    fn settings_defaults_and_old_files() {
        let d = NotifySettings::default();
        assert!(d.tray_on_close && d.windows == WinNotify::DmsAndMentions && !d.tray_hint_shown);
        let s: NotifySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, d);
        let s: NotifySettings = serde_json::from_str(r#"{"windows":"all","tray_on_close":false}"#).unwrap();
        assert_eq!((s.windows, s.tray_on_close), (WinNotify::All, false));
        assert_eq!(serde_json::to_string(&WinNotify::Off).unwrap(), "\"off\"");
        // A mode a newer app wrote does not throw the whole settings file away.
        let s: NotifySettings = serde_json::from_str(r#"{"windows":"mentions_only"}"#).unwrap();
        assert_eq!(s.windows, WinNotify::DmsAndMentions);
        let back: NotifySettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn closing_hides_only_with_a_tray_the_setting_and_no_quit_in_progress() {
        let mut n = NotifyUi::new(false);
        assert!(!n.hides_on_close(true), "no tray icon: nothing could bring the window back");
        n.quitting = true;
        assert!(!n.hides_on_close(true));
    }
}
