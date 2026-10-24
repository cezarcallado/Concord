//! "Atalhos" (v1.1, card K1): the settings page, the keyboard hook's lifecycle and what each
//! binding does (a child module of `app`). The decision itself is `crate::keybinds`.
//!
//! Each action does exactly what its button does, under the same conditions (mute and deafen only
//! in a call, mute not with push-to-talk or while deafened or server-muted, Go Live through its
//! picker, a soundboard sound through `play_sound` with its rate limit).

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use vox_proto::SoundId;

use super::App;
use crate::backend::Cmd;
use crate::forms::{self, note, note_colored, row};
use crate::keybinds::{self, Action, KeyHook, Keybind, Msg, SoundRef};
use crate::theme::{self, ph};
use crate::ui_kit::{self, IconState, Kind, Size};

/// What "Adicionar atalho" adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewKind {
    #[default]
    Mute,
    Deafen,
    Ptt,
    Leave,
    GoLive,
    Sound,
}

impl NewKind {
    const ALL: [NewKind; 6] = [NewKind::Mute, NewKind::Deafen, NewKind::Ptt, NewKind::Leave, NewKind::GoLive, NewKind::Sound];

    fn label(self) -> &'static str {
        match self {
            NewKind::Mute => Action::ToggleMute.label(),
            NewKind::Deafen => Action::ToggleDeafen.label(),
            NewKind::Ptt => Action::PushToTalk.label(),
            NewKind::Leave => Action::LeaveVoice.label(),
            NewKind::GoLive => Action::ToggleGoLive.label(),
            NewKind::Sound => "Tocar um som do soundboard",
        }
    }
}

/// The app's side of the shortcuts.
pub struct KeysUi {
    hook: Option<KeyHook>,
    tx: std_mpsc::Sender<Msg>,
    rx: std_mpsc::Receiver<Msg>,
    /// The table the matcher listens for (a `Msg::Fire` of another one is stale).
    gen: u32,
    /// The binding whose key is being recorded.
    pub capture: Option<usize>,
    pub new_kind: NewKind,
    pub new_sound: Option<SoundId>,
    /// A concord text field had the focus in the last drawn frame.
    text_focus: bool,
    /// The window had the focus in the last drawn frame.
    window_focus: bool,
    /// macOS (and Windows without the hook): this frame's key events, reused.
    egui_keys: Vec<(u32, bool, u8)>,
    /// Bindings that ran (the autotest's probe; only filled in a test run).
    pub fired: Vec<&'static str>,
    /// When a recording last ended with a key (Esc cancelling it, too): that press must not also
    /// close the settings window.
    capture_ended: Option<Instant>,
}

impl KeysUi {
    /// The system keyboard hook is installed.
    pub fn os_hook(&self) -> bool {
        self.hook.is_some()
    }

    /// A recording ended with a key a moment ago: an Esc now was that key (the settings window
    /// stays open).
    pub fn capture_just_ended(&self) -> bool {
        recently(self.capture_ended, Instant::now())
    }
}

/// The key that ended a recording reaches the window in the same frame or very soon after.
fn recently(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(500))
}

impl Default for KeysUi {
    fn default() -> Self {
        let (tx, rx) = std_mpsc::channel();
        KeysUi {
            hook: None,
            tx,
            rx,
            gen: 0,
            capture: None,
            new_kind: NewKind::default(),
            new_sound: None,
            text_focus: false,
            window_focus: false,
            egui_keys: Vec::new(),
            fired: Vec::new(),
            capture_ended: None,
        }
    }
}

/// The modifiers, as the keyboard names them.
#[cfg(target_os = "macos")]
const MODS_TEXT: &str = "Control, Option, Shift ou Cmd";
#[cfg(not(target_os = "macos"))]
const MODS_TEXT: &str = "Ctrl, Shift, Alt ou Win";

/// The OS hook is used (Windows) - except in the background tests, which feed synthetic events
/// and must never listen to the person's real keyboard (`VOX_TEST_REAL_KEY_HOOK=1` allows it for a
/// manual check).
fn os_hook_allowed() -> bool {
    crate::platform::global_keybinds() && (!vox_client_core::testmode::background() || std::env::var_os("VOX_TEST_REAL_KEY_HOOK").is_some())
}

impl App {
    /// Once, at start: where the key events go, and the old push-to-talk key adopted.
    pub(super) fn keybinds_init(&mut self) {
        let s = &mut self.settings;
        if keybinds::adopt_legacy_ptt(&mut s.keybinds, s.ptt_vk, &s.ptt_name) {
            tracing::info!("keyboard shortcuts: the push-to-talk key of the older settings became a binding");
        }
        let backend = self.backend.tx.clone();
        let tx = self.keys.tx.clone();
        // The window is woken from a thread of its own: egui's context lock can be held by a frame
        // being drawn, and the keyboard hook (every key of the system) must never wait for it.
        let (wake_tx, wake_rx) = std_mpsc::channel::<()>();
        let repaint = self.repaint.clone();
        let waker = std::thread::Builder::new().name("vox-key-wake".into()).spawn(move || {
            while wake_rx.recv().is_ok() {
                while wake_rx.try_recv().is_ok() {}
                repaint.wake();
            }
        });
        if let Err(e) = waker {
            tracing::warn!(error = %e, "keyboard shortcuts: no wake thread (they run on the next frame)");
        }
        keybinds::set_sink(move |m| match m {
            // Straight to the voice session: no UI frame in between.
            Msg::Ptt(down) => {
                let _ = backend.send(Cmd::PttKey(down));
            }
            other => {
                let _ = tx.send(other);
                let _ = wake_tx.send(());
            }
        });
        self.sync_keybinds();
    }

    /// After any change of the bindings or of push-to-talk: the hook runs only while something
    /// needs it (a binding, or a key being recorded), the matcher gets the new table, and
    /// `ptt_vk` / `ptt_name` follow for an older app.
    pub fn sync_keybinds(&mut self) {
        let (vk, name) = keybinds::legacy_ptt(&self.settings.keybinds);
        self.settings.ptt_vk = vk;
        self.settings.ptt_name = name;
        let ptt_on = self.ptt_active();
        let (gen, ptt_was_held) = keybinds::set_table(&self.settings.keybinds, ptt_on);
        self.keys.gen = gen;
        if ptt_was_held {
            self.backend.send(Cmd::PttKey(false));
        }
        let any = self.settings.keybinds.iter().any(|k| k.key.is_set() && (!k.action.is_ptt() || ptt_on));
        let want = os_hook_allowed() && (any || self.settings_ui.capturing_key);
        if want && self.keys.hook.is_none() {
            self.keys.hook = KeyHook::start();
            if self.keys.hook.is_none() {
                tracing::warn!("cannot install the keyboard hook: shortcuts only work with the window in front");
            }
        } else if !want && self.keys.hook.is_some() {
            self.keys.hook = None;
        }
    }

    /// Bindings or push-to-talk changed outside the page (the autotest): sync, tell the voice
    /// session, save.
    pub fn keybinds_changed(&mut self) {
        self.ptt_settings_changed();
    }

    /// Run what the keys asked for right now (the autotest, between synthetic presses).
    pub fn keybinds_pump_now(&mut self) {
        self.keybinds_pump();
    }

    /// The hook is gone (exit).
    pub(super) fn keybinds_exit(&mut self) {
        self.keys.hook = None;
    }

    /// From `logic()` (also while the window is hidden): run what the keys asked for.
    pub(super) fn keybinds_pump(&mut self) {
        keybinds::set_typing(self.keys.text_focus && self.window_focused());
        while let Ok(m) = self.keys.rx.try_recv() {
            match m {
                Msg::Fire { gen, idx } if gen == self.keys.gen => self.run_keybind(idx as usize),
                Msg::Fire { .. } | Msg::Ptt(_) => {}
                Msg::Captured { combo, name } => {
                    if let Some(kb) = self.keys.capture.and_then(|i| self.settings.keybinds.get_mut(i)) {
                        kb.key = combo;
                        kb.key_name = name;
                    }
                    self.keys.capture_ended = Some(Instant::now());
                    self.end_capture();
                }
                Msg::CaptureCancelled => {
                    self.keys.capture_ended = Some(Instant::now());
                    self.end_capture();
                }
            }
        }
    }

    /// From `ui()`: whether a text field has the focus and, without the OS hook (macOS), the
    /// window's own key events.
    pub(super) fn keybinds_frame(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        self.keys.text_focus = focused && ctx.text_edit_focused();
        keybinds::set_typing(self.keys.text_focus);
        let lost = std::mem::replace(&mut self.keys.window_focus, focused) && !focused;
        if self.keys.hook.is_some() {
            return;
        }
        if lost {
            // A key held in the window is released elsewhere: never seen.
            keybinds::release_all();
        }
        if !focused {
            return;
        }
        let mut keys = std::mem::take(&mut self.keys.egui_keys);
        keys.clear();
        ctx.input(|i| {
            for e in &i.events {
                if let egui::Event::Key { key, pressed, repeat, modifiers, .. } = e {
                    if let Some(vk) = keybinds::vk_of_egui(*key) {
                        // A repeat is a press the matcher ignores anyway; skip it here.
                        if !(*pressed && *repeat) {
                            keys.push((vk, *pressed, keybinds::mods_of_egui(*modifiers)));
                        }
                    }
                }
            }
        });
        for &(vk, down, mods) in &keys {
            keybinds::feed(vk, down, mods);
        }
        self.keys.egui_keys = keys;
        if !self.keys.egui_keys.is_empty() {
            self.keybinds_pump();
        }
    }

    fn end_capture(&mut self) {
        self.keys.capture = None;
        self.settings_ui.capturing_key = false;
        keybinds::cancel_capture();
        self.ptt_settings_changed();
    }

    fn start_capture(&mut self, i: usize) {
        self.keys.capture = Some(i);
        self.settings_ui.capturing_key = true;
        self.sync_keybinds();
        keybinds::begin_capture();
    }

    /// Closing the settings while a key is being recorded.
    pub(super) fn keybinds_settings_closed(&mut self) {
        if self.keys.capture.is_some() || self.settings_ui.capturing_key {
            self.keys.capture = None;
            self.settings_ui.capturing_key = false;
            keybinds::cancel_capture();
        }
    }

    /// The server a new soundboard binding belongs to, and how to recognise it.
    fn sound_server(&self) -> (Option<String>, String) {
        (self.session_server_id.clone(), self.session_url.to_lowercase())
    }

    /// Binding `i` was pressed: what its button would do.
    pub fn run_keybind(&mut self, i: usize) {
        let Some(action) = self.settings.keybinds.get(i).map(|k| k.action.clone()) else { return };
        let in_voice = self.voice.channel.is_some();
        tracing::info!(action = action.log_name(), in_voice, "keyboard shortcut");
        if !self.persist {
            // The autotest's probe (a test run only: nothing grows in a real session).
            self.keys.fired.push(action.log_name());
        }
        match action {
            Action::PushToTalk => {}
            Action::ToggleMute => {
                let ptt = self.ptt_active();
                if in_voice && !ptt && !self.voice.deafened && !self.voice.no_mic && !self.i_am_server_muted() {
                    self.toggle_mute();
                }
            }
            Action::ToggleDeafen => {
                if in_voice {
                    self.toggle_deafen();
                }
            }
            Action::LeaveVoice => {
                if self.in_dm_call() {
                    self.hang_up_dm_call();
                } else if in_voice || self.voice.joining.is_some() {
                    self.leave_voice();
                }
            }
            Action::ToggleGoLive => {
                if !in_voice {
                    return;
                }
                match self.voice.share {
                    super::Share::Live { .. } => self.stop_share(),
                    super::Share::Starting => {}
                    super::Share::Off if crate::platform::screen_soon() => {}
                    super::Share::Off => {
                        // The picker is in the window: bring it to the front.
                        self.open_golive();
                        if !self.window_focused() {
                            if let Some(ctx) = self.repaint.ctx() {
                                self.show_window(&ctx);
                            }
                        }
                    }
                }
            }
            Action::PlaySound(s) => {
                let (id, url) = self.sound_server();
                if !in_voice || self.in_dm_call() || !self.sb.supported || !s.on_server(id.as_deref(), &url) {
                    return;
                }
                if self.sb.find(s.sound).is_some() {
                    self.play_sound(s.sound);
                } else {
                    self.notice = Some(format!("{} Atalho: o som \"{}\" não existe mais neste servidor.", super::ICON_WARN, s.name));
                }
            }
        }
    }

    /// Push-to-talk's keys, for the Voz page ("(nenhuma)" without one).
    pub(super) fn ptt_keys_text(&self) -> String {
        let keys: Vec<String> = self.settings.keybinds.iter().filter(|k| k.action.is_ptt() && k.key.is_set()).map(|k| k.label()).collect();
        if keys.is_empty() {
            "(nenhuma)".to_string()
        } else {
            keys.join("  ·  ")
        }
    }

    // ------------------------------------------------------------ the settings page

    /// "Atalhos". Returns true when the bindings changed (saved, hook and table synced).
    pub(super) fn page_keybinds(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        forms::group(ui, "Atalhos de teclado");
        if crate::platform::global_keybinds() {
            note(
                ui,
                "Funcionam também com o concord em segundo plano (no jogo, por exemplo), e as teclas continuam chegando aos outros programas. \
                 Enquanto você digita no concord, os atalhos de letras, números e outras teclas de texto sem Ctrl, Alt ou Win ficam pausados.",
                false,
            );
        } else {
            note(
                ui,
                format!(
                    "No Mac, os atalhos funcionam com a janela do concord em foco. Fora dela: {} (precisa da permissão de Monitoramento de Entrada).",
                    crate::platform::SOON_MAC_LC
                ),
                false,
            );
        }
        ui.add_space(theme::space::S);
        if self.settings.keybinds.is_empty() {
            note(ui, "Nenhum atalho ainda. Escolha uma ação abaixo e pressione a tecla.", false);
        }
        let mut remove = None;
        let mut record = None;
        let mut cancel = false;
        let (srv_id, srv_url) = self.sound_server();
        let capturing = self.keys.capture;
        for i in 0..self.settings.keybinds.len() {
            let kb = &self.settings.keybinds[i];
            let title = match &kb.action {
                Action::PlaySound(s) => {
                    let emoji = if s.emoji.is_empty() { "🔊" } else { s.emoji.as_str() };
                    format!("Som: {emoji} {}", s.name)
                }
                a => a.label().to_string(),
            };
            row(ui, &title, |ui| {
                let recording = capturing == Some(i);
                let (text, fill) = if recording {
                    ("pressione as teclas...".to_string(), theme::MOVEL_HOVER)
                } else {
                    (kb.label(), theme::MOVEL)
                };
                let fg = if kb.key.is_set() || recording { theme::GIZ } else { theme::NEVOA };
                ui_kit::pill(ui, &text, fill, fg);
                if recording {
                    if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Cancelar").clicked() {
                        cancel = true;
                    }
                } else {
                    let label = if kb.key.is_set() { "Alterar" } else { "Gravar tecla" };
                    if ui.add_enabled_ui(capturing.is_none(), |ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, label)).inner.clicked() {
                        record = Some(i);
                    }
                    // (Not while another key is recorded: the list must keep its order until then.)
                    if ui.add_enabled_ui(capturing.is_none(), |ui| ui_kit::icon_button(ui, ph::TRASH, "Remover atalho", IconState::Normal)).inner.clicked() {
                        remove = Some(i);
                    }
                }
            });
            let others: Vec<&str> = keybinds::conflicts(&self.settings.keybinds, i).map(|j| self.settings.keybinds[j].action.label()).collect();
            if !others.is_empty() {
                note_colored(ui, format!("{} A mesma tecla também está em: {}. As ações disparam juntas.", ph::WARNING, others.join(", ")), true, theme::ABAJUR);
            }
            match &kb.action {
                Action::PushToTalk if !crate::platform::global_ptt() => note(ui, format!("Push-to-talk: {}.", crate::platform::SOON_MAC_LC), true),
                Action::PushToTalk if !self.settings.ptt_enabled => note(ui, "O push-to-talk está desligado: ligue em Voz, \"Usar push-to-talk\".", true),
                Action::PlaySound(s) => {
                    if self.me.is_some() && s.on_server(srv_id.as_deref(), &srv_url) {
                        if self.sb.supported && self.sb.find(s.sound).is_none() {
                            note_colored(ui, format!("{} Som indisponível: ele foi apagado deste servidor.", ph::WARNING), true, theme::ABAJUR);
                        }
                    } else if !s.server_name.is_empty() {
                        note(ui, format!("Do servidor {}: só funciona lá.", s.server_name), true);
                    }
                }
                _ => {}
            }
        }
        if cancel {
            self.keys.capture = None;
            self.settings_ui.capturing_key = false;
            keybinds::cancel_capture();
            changed = true;
        }
        if let Some(i) = record {
            self.start_capture(i);
        }
        if let Some(i) = remove {
            self.settings.keybinds.remove(i);
            changed = true;
        }

        forms::divider(ui);
        forms::group(ui, "Adicionar atalho");
        row(ui, "Ação", |ui| {
            egui::ComboBox::from_id_salt("keybind_new").width(forms::combo_w(ui)).selected_text(self.keys.new_kind.label()).show_ui(ui, |ui| {
                for k in NewKind::ALL {
                    ui.selectable_value(&mut self.keys.new_kind, k, k.label());
                }
            });
        });
        let mut can_add = capturing.is_none();
        if self.keys.new_kind == NewKind::Sound {
            if self.me.is_none() || !self.sb.supported || self.sb.sounds.is_empty() {
                can_add = false;
                let text = if self.me.is_none() {
                    "Entre num servidor para escolher um dos sons dele."
                } else if !self.sb.supported {
                    "Este servidor não tem soundboard."
                } else {
                    "O soundboard deste servidor ainda não tem sons."
                };
                note(ui, text, true);
            } else {
                if self.keys.new_sound.is_none_or(|id| self.sb.find(id).is_none()) {
                    self.keys.new_sound = self.sb.sounds.first().map(|s| s.id);
                }
                let shown = self.keys.new_sound.and_then(|id| self.sb.find(id)).map(|s| sound_text(&s.emoji, &s.name)).unwrap_or_default();
                row(ui, "Som", |ui| {
                    egui::ComboBox::from_id_salt("keybind_sound").width(forms::combo_w(ui)).selected_text(shown).show_ui(ui, |ui| {
                        for s in &self.sb.sounds {
                            ui.selectable_value(&mut self.keys.new_sound, Some(s.id), sound_text(&s.emoji, &s.name));
                        }
                    });
                });
            }
        }
        let add = forms::indented(ui, |ui| ui.add_enabled_ui(can_add, |ui| ui_kit::button(ui, Kind::Primary, Some(ph::PLUS), "Adicionar atalho")).inner.clicked());
        note(ui, format!("Depois de adicionar, pressione a tecla ou a combinação ({MODS_TEXT} mais uma tecla). Esc cancela."), true);
        if add {
            let action = match self.keys.new_kind {
                NewKind::Mute => Some(Action::ToggleMute),
                NewKind::Deafen => Some(Action::ToggleDeafen),
                NewKind::Ptt => Some(Action::PushToTalk),
                NewKind::Leave => Some(Action::LeaveVoice),
                NewKind::GoLive => Some(Action::ToggleGoLive),
                NewKind::Sound => self.keys.new_sound.and_then(|id| self.sb.find(id)).map(|s| {
                    Action::PlaySound(SoundRef {
                        server_id: srv_id.clone(),
                        server_url: srv_url.clone(),
                        server_name: self.server_name.clone(),
                        sound: s.id,
                        name: s.name.clone(),
                        emoji: s.emoji.clone(),
                    })
                }),
            };
            if let Some(a) = action {
                self.settings.keybinds.push(Keybind::new(a));
                let i = self.settings.keybinds.len() - 1;
                self.start_capture(i);
                changed = true;
            }
        }
        changed
    }
}

fn sound_text(emoji: &str, name: &str) -> String {
    let e = if emoji.is_empty() { "🔊" } else { emoji };
    format!("{e}  {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_new_kind_has_a_label() {
        for k in NewKind::ALL {
            assert!(!k.label().is_empty());
        }
    }

    #[test]
    fn the_esc_that_cancels_a_recording_does_not_close_the_settings() {
        let t = Instant::now();
        assert!(!recently(None, t));
        assert!(recently(Some(t), t));
        assert!(recently(Some(t), t + Duration::from_millis(300)));
        assert!(!recently(Some(t), t + Duration::from_millis(600)), "a later Esc closes them again");
    }
}
