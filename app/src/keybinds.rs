//! "Atalhos de teclado" (v1.1, card K1): keys or key combinations for mute, deafen, push-to-talk,
//! leave the voice room, start / stop Go Live and play a soundboard sound.
//!
//! - The bindings live in settings.json (`Settings::keybinds`). Push-to-talk's key used to be
//!   `ptt_vk` / `ptt_name`; those fields stay, mirrored from the first push-to-talk binding, so an
//!   older app still finds its key ([`adopt_legacy_ptt`], [`legacy_ptt`]). An entry this version
//!   cannot read (a newer app's action) is skipped, never a reason to drop the whole file.
//! - Keys are Windows virtual-key codes on every system (egui's keys are mapped to them on macOS),
//!   plus Ctrl / Shift / Alt / Win (Cmd on the Mac). A combination fires when its key goes down
//!   with at least its modifiers held; among several bindings of that key, the one with the most
//!   modifiers wins (`V` and `Ctrl+V` can do different things). A modifier alone is a key too
//!   (Ctrl as push-to-talk).
//! - [`Matcher`] is the whole decision, free of the system so it is unit tested with synthetic
//!   events: toggles fire once per press (auto-repeat is ignored), push-to-talk is held, and while
//!   a concord text field has the focus a binding of a key that types or edits text (a letter, a
//!   digit, Space, an arrow...) without Ctrl / Alt / Win does not fire (typing in the chat never
//!   mutes anybody; Right Ctrl, F13 or Caps Lock as push-to-talk still talk). Releases are always seen, so a key held while focus moves
//!   never sticks.
//! - Windows: one low-level keyboard hook on its own thread (the old push-to-talk hook), installed
//!   only while some binding needs it or a key is being recorded. It only reads: every key goes on
//!   to the other programs unchanged (like Discord). Keystrokes are never stored or logged; the
//!   callback is a lock, a few compares and at most one channel send (the UI is woken from another
//!   thread). No polling, except while push-to-talk is held: the hook thread then asks the system
//!   every 250 ms whether its key still is down, so a release that never reaches the hook (Win+L,
//!   a UAC prompt) cannot leave the microphone open.
//! - AltGr (Ctrl+Alt to the system; pt-BR ABNT2 types "/" with AltGr+Q) only triggers a binding
//!   with both Ctrl and Alt, and never while typing in concord.
//! - macOS: the same matcher fed with egui's key events, so the bindings work while the window has
//!   the focus. A global hook there needs the Input Monitoring / Accessibility permission (not
//!   done; see `platform::global_keybinds`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Deserializer, Serialize};
use vox_proto::SoundId;

pub const MOD_CTRL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_ALT: u8 = 4;
/// The Windows key; Cmd on the Mac.
pub const MOD_WIN: u8 = 8;
const MOD_ALL: u8 = MOD_CTRL | MOD_SHIFT | MOD_ALT | MOD_WIN;
/// Only in what the hook reports as held, never in a binding: the right Alt is down with the left
/// Ctrl, i.e. AltGr (pt-BR ABNT2: AltGr+Q is "/", and the system sees it as Ctrl+Alt).
pub const MOD_ALTGR: u8 = 16;

pub const VK_ESCAPE: u32 = 0x1B;

/// The modifier class of a virtual key (0 for a normal key).
pub const fn mod_bit(vk: u32) -> u8 {
    match vk {
        0x10 | 0xA0 | 0xA1 => MOD_SHIFT,
        0x11 | 0xA2 | 0xA3 => MOD_CTRL,
        0x12 | 0xA4 | 0xA5 => MOD_ALT,
        0x5B | 0x5C => MOD_WIN,
        _ => 0,
    }
}

pub const fn is_modifier(vk: u32) -> bool {
    mod_bit(vk) != 0
}

// ---------------------------------------------------------------- the saved bindings

/// A key plus the modifiers that must be held with it. `vk == 0`: no key chosen yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Combo {
    pub vk: u32,
    pub mods: u8,
}

impl Combo {
    pub const fn new(vk: u32, mods: u8) -> Combo {
        Combo { vk, mods }.normalized()
    }

    pub const fn is_set(self) -> bool {
        self.vk != 0
    }

    /// Only the four modifier bits, and never the main key's own (Ctrl as the key is just Ctrl).
    pub const fn normalized(self) -> Combo {
        Combo { vk: self.vk, mods: self.mods & MOD_ALL & !mod_bit(self.vk) }
    }

    /// Has Ctrl, Alt or Win (Shift alone still types).
    pub const fn has_command_mod(self) -> bool {
        self.mods & (MOD_CTRL | MOD_ALT | MOD_WIN) != 0
    }

    /// Pressing it in a text field types or edits: a key that types or edits text, without
    /// Ctrl, Alt or Win.
    pub const fn is_typing(self) -> bool {
        !self.has_command_mod() && types_text(self.vk)
    }

    /// "Ctrl + Shift + M" (`key_name`: the key's own name as recorded; a generic one if empty).
    pub fn label(self, key_name: &str) -> String {
        if !self.is_set() {
            return "(nenhuma tecla)".to_string();
        }
        let mut s = String::new();
        for (bit, name) in MOD_NAMES {
            if self.mods & bit != 0 {
                s.push_str(name);
                s.push_str(" + ");
            }
        }
        if key_name.is_empty() {
            s.push_str(&vk_name(self.vk));
        } else {
            s.push_str(key_name);
        }
        s
    }
}

#[cfg(target_os = "macos")]
const MOD_NAMES: [(u8, &str); 4] = [(MOD_CTRL, "Control"), (MOD_ALT, "Option"), (MOD_SHIFT, "Shift"), (MOD_WIN, "Cmd")];
#[cfg(not(target_os = "macos"))]
const MOD_NAMES: [(u8, &str); 4] = [(MOD_CTRL, "Ctrl"), (MOD_SHIFT, "Shift"), (MOD_ALT, "Alt"), (MOD_WIN, "Win")];

/// A key that types or edits text: letters, digits, Space, the numeric keypad, punctuation, Enter,
/// Backspace, Tab, Delete, the arrows, Home / End / Page Up / Page Down. Not the F-keys, a
/// modifier alone, Caps Lock, Insert, Pause or the media keys.
pub const fn types_text(vk: u32) -> bool {
    matches!(vk, 0x08 | 0x09 | 0x0D | 0x20..=0x28 | 0x2E | 0x30..=0x39 | 0x41..=0x5A | 0x60..=0x6F | 0xBA..=0xC0 | 0xDB..=0xDF | 0xE2)
}

/// A name for a virtual key when the system gave none (letters, digits, F-keys, a few others).
pub fn vk_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x14 => "Caps Lock".into(),
        VK_ESCAPE => "Esc".into(),
        0x20 => "Espaço".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Seta esquerda".into(),
        0x26 => "Seta para cima".into(),
        0x27 => "Seta direita".into(),
        0x28 => "Seta para baixo".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x10 | 0xA0 | 0xA1 => "Shift".into(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".into(),
        0x12 | 0xA4 | 0xA5 => "Alt".into(),
        0x5B | 0x5C => "Win".into(),
        _ => format!("Tecla {vk}"),
    }
}

/// The sound a "Tocar um som" binding plays: which server it belongs to (sound ids are per
/// server) and its name and emoji when it was chosen (shown when it is gone).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundRef {
    /// The server's identity (`HealthResponse::server_id`); `None` for a hub older than v1.0.1.
    pub server_id: Option<String>,
    /// Its address (normalized, lowercase): the match when either side has no identity.
    pub server_url: String,
    pub server_name: String,
    pub sound: SoundId,
    pub name: String,
    pub emoji: String,
}

impl SoundRef {
    /// This sound belongs to the server at `url` with identity `id`.
    pub fn on_server(&self, id: Option<&str>, url: &str) -> bool {
        match (self.server_id.as_deref(), id) {
            (Some(a), Some(b)) => a == b,
            _ => !self.server_url.is_empty() && self.server_url == url,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    LeaveVoice,
    ToggleGoLive,
    PlaySound(SoundRef),
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::ToggleMute => "Mutar/desmutar o microfone",
            Action::ToggleDeafen => "Ensurdecer",
            Action::PushToTalk => "Falar (push-to-talk)",
            Action::LeaveVoice => "Sair da voz",
            Action::ToggleGoLive => "Iniciar/parar transmissão",
            Action::PlaySound(_) => "Tocar um som",
        }
    }

    /// A short name for the log (never the key).
    pub fn log_name(&self) -> &'static str {
        match self {
            Action::ToggleMute => "mute",
            Action::ToggleDeafen => "deafen",
            Action::PushToTalk => "push-to-talk",
            Action::LeaveVoice => "leave",
            Action::ToggleGoLive => "go-live",
            Action::PlaySound(_) => "sound",
        }
    }

    pub fn is_ptt(&self) -> bool {
        matches!(self, Action::PushToTalk)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    pub action: Action,
    #[serde(default)]
    pub key: Combo,
    /// The key's name as the system called it when it was recorded ("M", "Ctrl direito").
    #[serde(default)]
    pub key_name: String,
}

impl Keybind {
    pub fn new(action: Action) -> Keybind {
        Keybind { action, key: Combo::default(), key_name: String::new() }
    }

    pub fn label(&self) -> String {
        self.key.label(&self.key_name)
    }
}

/// `Settings::keybinds` from settings.json: whatever is there, keeping each entry this version
/// understands (a newer app's action, or anything malformed, is skipped instead of failing the
/// whole settings file).
pub fn de_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Keybind>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    let serde_json::Value::Array(items) = v else { return Ok(Vec::new()) };
    Ok(items
        .into_iter()
        .filter_map(|it| serde_json::from_value::<Keybind>(it).ok())
        .map(|mut k| {
            k.key = k.key.normalized();
            k
        })
        .collect())
}

/// Older settings (or an older app since): push-to-talk's key is `ptt_vk`. Make sure it is a
/// push-to-talk binding. When that key is not among them, the older app changed it: it replaces
/// the push-to-talk bindings without modifiers (the ones it could have written). True if changed.
pub fn adopt_legacy_ptt(list: &mut Vec<Keybind>, ptt_vk: u32, ptt_name: &str) -> bool {
    if ptt_vk == 0 {
        return false;
    }
    let legacy = Combo::new(ptt_vk, 0);
    if list.iter().any(|k| k.action.is_ptt() && k.key == legacy) {
        return false;
    }
    let at = list.iter().position(|k| k.action.is_ptt() && k.key.mods == 0);
    list.retain(|k| !(k.action.is_ptt() && k.key.mods == 0));
    let kb = Keybind { action: Action::PushToTalk, key: legacy, key_name: ptt_name.to_string() };
    list.insert(at.unwrap_or(list.len()).min(list.len()), kb);
    true
}

/// What to keep in `ptt_vk` / `ptt_name` for an older app (and for the push-to-talk hover texts):
/// the first push-to-talk binding with a single key. An older app only knows a single key, so
/// with combinations only it gets no key (it then asks for one instead of talking on half of it).
pub fn legacy_ptt(list: &[Keybind]) -> (u32, String) {
    let ptt = || list.iter().filter(|k| k.action.is_ptt() && k.key.is_set());
    match ptt().find(|k| k.key.mods == 0) {
        Some(k) => (k.key.vk, k.label()),
        None => (0, ptt().next().map(Keybind::label).unwrap_or_default()),
    }
}

/// Some push-to-talk binding has a key.
pub fn has_ptt(list: &[Keybind]) -> bool {
    list.iter().any(|k| k.action.is_ptt() && k.key.is_set())
}

/// The other bindings on exactly the same keys as binding `i` (they all fire together).
pub fn conflicts(list: &[Keybind], i: usize) -> impl Iterator<Item = usize> + '_ {
    let key = list.get(i).map(|k| k.key).unwrap_or_default();
    list.iter().enumerate().filter(move |(j, k)| *j != i && key.is_set() && k.key == key).map(|(j, _)| j)
}

// ---------------------------------------------------------------- the decision

/// What a key event did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Push-to-talk pressed / released (any of its bindings; only the first press and the last
    /// release are reported).
    Ptt(bool),
    /// Binding `idx` of table `gen` (see [`set_table`]) was pressed.
    Fire { gen: u32, idx: u16 },
    /// Recording: this combination was pressed (`name`: the system's name of the key, may be empty).
    Captured { combo: Combo, name: String },
    /// Recording: Esc.
    CaptureCancelled,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    key: Combo,
    ptt: bool,
    /// Down and not released yet: a repeat does nothing.
    held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    Off,
    Waiting,
    /// A modifier went down first: it becomes the key if it comes up with nothing else pressed.
    Modifier(u32),
}

/// The bindings being listened for and the keys held now.
#[derive(Debug)]
pub struct Matcher {
    entries: Vec<Entry>,
    /// The `idx` of each entry in the app's list.
    idx: Vec<u16>,
    gen: u32,
    ptt_held: u16,
    capture: Capture,
}

impl Default for Matcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Matcher {
    pub const fn new() -> Matcher {
        Matcher { entries: Vec::new(), idx: Vec::new(), gen: 0, ptt_held: 0, capture: Capture::Off }
    }

    /// Listen for `binds` (the app's list; push-to-talk ones only with `ptt_on`). Everything held
    /// is forgotten. Returns the new table's generation and whether push-to-talk was held (the
    /// caller releases it).
    pub fn set(&mut self, binds: &[Keybind], ptt_on: bool) -> (u32, bool) {
        let was = self.ptt_held > 0;
        self.entries.clear();
        self.idx.clear();
        for (i, k) in binds.iter().enumerate() {
            if !k.key.is_set() || (k.action.is_ptt() && !ptt_on) || i > u16::MAX as usize {
                continue;
            }
            self.entries.push(Entry { key: k.key.normalized(), ptt: k.action.is_ptt(), held: false });
            self.idx.push(i as u16);
        }
        self.ptt_held = 0;
        self.gen = self.gen.wrapping_add(1);
        (self.gen, was)
    }

    pub fn begin_capture(&mut self) {
        self.capture = Capture::Waiting;
    }

    pub fn cancel_capture(&mut self) {
        self.capture = Capture::Off;
    }

    #[cfg(test)]
    pub fn capturing(&self) -> bool {
        self.capture != Capture::Off
    }

    /// One key event. `mods`: the modifiers held, plus [`MOD_ALTGR`] (asked only when the key
    /// matters); `typing`: a concord text field has the focus.
    pub fn on_key(&mut self, vk: u32, down: bool, mods: impl FnOnce() -> u8, typing: bool, emit: &mut dyn FnMut(Msg)) {
        if vk == 0 {
            return;
        }
        let mut mods = Some(mods);
        let mut held = move || mods.take().map_or(0, |f| f());
        let own = |raw: u8| raw & MOD_ALL & !mod_bit(vk);
        if down {
            if self.capture != Capture::Off {
                if is_modifier(vk) {
                    self.capture = Capture::Modifier(vk);
                    return;
                }
                let m = own(held());
                self.capture = Capture::Off;
                if vk == VK_ESCAPE && m == 0 {
                    emit(Msg::CaptureCancelled);
                } else {
                    emit(Msg::Captured { combo: Combo::new(vk, m), name: String::new() });
                }
                return;
            }
            if !self.entries.iter().any(|e| e.key.vk == vk) {
                return;
            }
            let raw = held();
            let m = own(raw);
            // AltGr types a character: only a binding with both Ctrl and Alt takes it (as the
            // system's own hotkeys do), and never while typing in concord.
            let altgr = raw & MOD_ALTGR != 0 && !is_modifier(vk);
            let need = if altgr { MOD_CTRL | MOD_ALT } else { 0 };
            // The most specific binding of this key whose modifiers are all held.
            let mut best: Option<u8> = None;
            for e in self.entries.iter().filter(|e| e.key.vk == vk && e.key.mods & !m == 0 && e.key.mods & need == need) {
                if best.is_none_or(|b| e.key.mods.count_ones() > b.count_ones()) {
                    best = Some(e.key.mods);
                }
            }
            let Some(best) = best else { return };
            if typing && (altgr || Combo::new(vk, best).is_typing()) {
                return;
            }
            for (n, e) in self.entries.iter_mut().enumerate() {
                if e.key.vk != vk || e.key.mods != best || e.held {
                    continue;
                }
                e.held = true;
                if e.ptt {
                    self.ptt_held += 1;
                    if self.ptt_held == 1 {
                        emit(Msg::Ptt(true));
                    }
                } else {
                    emit(Msg::Fire { gen: self.gen, idx: self.idx[n] });
                }
            }
        } else {
            if self.capture == Capture::Modifier(vk) {
                let m = own(held());
                self.capture = Capture::Off;
                emit(Msg::Captured { combo: Combo::new(vk, m), name: String::new() });
            }
            let bit = mod_bit(vk);
            for e in self.entries.iter_mut() {
                if !e.held || (e.key.vk != vk && e.key.mods & bit == 0) {
                    continue;
                }
                e.held = false;
                if e.ptt {
                    self.ptt_held = self.ptt_held.saturating_sub(1);
                    if self.ptt_held == 0 {
                        emit(Msg::Ptt(false));
                    }
                }
            }
        }
    }

    /// Push-to-talk is held.
    pub fn ptt_down(&self) -> bool {
        self.ptt_held > 0
    }

    /// Nothing is held any more (the window lost the focus with a key down: its release goes
    /// elsewhere). Push-to-talk ends.
    pub fn release_all(&mut self, emit: &mut dyn FnMut(Msg)) {
        for e in self.entries.iter_mut() {
            e.held = false;
        }
        if std::mem::take(&mut self.ptt_held) > 0 {
            emit(Msg::Ptt(false));
        }
    }

    /// Push-to-talk keys the system says are up although their release never came (it went to
    /// another desktop: Win+L, a UAC prompt, Ctrl+Alt+Del): push-to-talk ends, so the microphone
    /// never stays open. Toggles are left alone (a missed release costs one press, nothing more).
    #[cfg(any(windows, test))]
    pub fn release_stale_ptt(&mut self, is_down: impl Fn(u32) -> bool, emit: &mut dyn FnMut(Msg)) {
        for e in self.entries.iter_mut() {
            if !e.held || !e.ptt || is_down(e.key.vk) {
                continue;
            }
            e.held = false;
            self.ptt_held = self.ptt_held.saturating_sub(1);
            if self.ptt_held == 0 {
                emit(Msg::Ptt(false));
            }
        }
    }
}

// ---------------------------------------------------------------- the shared state

type Sink = Arc<dyn Fn(Msg) + Send + Sync>;

static MATCHER: Mutex<Matcher> = Mutex::new(Matcher::new());
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static TYPING: AtomicBool = AtomicBool::new(false);

fn matcher() -> std::sync::MutexGuard<'static, Matcher> {
    MATCHER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the key events go (the app sets it once).
pub fn set_sink(f: impl Fn(Msg) + Send + Sync + 'static) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

/// See [`Matcher::set`].
pub fn set_table(binds: &[Keybind], ptt_on: bool) -> (u32, bool) {
    matcher().set(binds, ptt_on)
}

pub fn begin_capture() {
    matcher().begin_capture();
}

pub fn cancel_capture() {
    matcher().cancel_capture();
}

/// A concord text field has the focus (and the window is in front).
pub fn set_typing(on: bool) {
    TYPING.store(on, Ordering::Relaxed);
}

/// One key event, from the hook or from the window (and from the autotest, synthetic).
/// `name`: the system's name of the key, asked only for a recorded key. Returns whether
/// push-to-talk is held now.
pub fn dispatch(vk: u32, down: bool, mods: impl FnOnce() -> u8, name: impl FnOnce() -> String) -> bool {
    let sink = SINK.lock().ok().and_then(|g| g.clone());
    let Some(sink) = sink else { return false };
    let typing = TYPING.load(Ordering::Relaxed);
    let mut name = Some(name);
    let mut g = matcher();
    g.on_key(vk, down, mods, typing, &mut |m| match m {
        Msg::Captured { combo, .. } => sink(Msg::Captured { combo, name: name.take().map(|f| f()).unwrap_or_default() }),
        other => sink(other),
    });
    g.ptt_down()
}

/// See [`Matcher::release_all`] (the window's own keys, without the hook).
pub fn release_all() {
    let sink = SINK.lock().ok().and_then(|g| g.clone());
    matcher().release_all(&mut |msg| {
        if let Some(s) = &sink {
            s(msg);
        }
    });
}

/// The hook thread's check while push-to-talk is held ([`Matcher::release_stale_ptt`]). Returns
/// whether it still is.
#[cfg(windows)]
fn recheck_ptt(is_down: impl Fn(u32) -> bool) -> bool {
    let sink = SINK.lock().ok().and_then(|g| g.clone());
    let mut m = matcher();
    m.release_stale_ptt(is_down, &mut |msg| {
        if let Some(s) = &sink {
            s(msg);
        }
    });
    m.ptt_down()
}

/// Synthetic key events (the autotest; never through the system).
pub fn feed(vk: u32, down: bool, mods: u8) {
    let _ = dispatch(vk, down, || mods, String::new);
}

// ---------------------------------------------------------------- egui keys (macOS, fallback)

/// The virtual key of an egui key (the same codes the Windows hook reports).
pub fn vk_of_egui(key: eframe::egui::Key) -> Option<u32> {
    use eframe::egui::Key as K;
    let vk = match key {
        K::A => 0x41,
        K::B => 0x42,
        K::C => 0x43,
        K::D => 0x44,
        K::E => 0x45,
        K::F => 0x46,
        K::G => 0x47,
        K::H => 0x48,
        K::I => 0x49,
        K::J => 0x4A,
        K::K => 0x4B,
        K::L => 0x4C,
        K::M => 0x4D,
        K::N => 0x4E,
        K::O => 0x4F,
        K::P => 0x50,
        K::Q => 0x51,
        K::R => 0x52,
        K::S => 0x53,
        K::T => 0x54,
        K::U => 0x55,
        K::V => 0x56,
        K::W => 0x57,
        K::X => 0x58,
        K::Y => 0x59,
        K::Z => 0x5A,
        K::Num0 => 0x30,
        K::Num1 => 0x31,
        K::Num2 => 0x32,
        K::Num3 => 0x33,
        K::Num4 => 0x34,
        K::Num5 => 0x35,
        K::Num6 => 0x36,
        K::Num7 => 0x37,
        K::Num8 => 0x38,
        K::Num9 => 0x39,
        K::F1 => 0x70,
        K::F2 => 0x71,
        K::F3 => 0x72,
        K::F4 => 0x73,
        K::F5 => 0x74,
        K::F6 => 0x75,
        K::F7 => 0x76,
        K::F8 => 0x77,
        K::F9 => 0x78,
        K::F10 => 0x79,
        K::F11 => 0x7A,
        K::F12 => 0x7B,
        K::F13 => 0x7C,
        K::F14 => 0x7D,
        K::F15 => 0x7E,
        K::F16 => 0x7F,
        K::F17 => 0x80,
        K::F18 => 0x81,
        K::F19 => 0x82,
        K::F20 => 0x83,
        K::Escape => VK_ESCAPE,
        K::Tab => 0x09,
        K::Backspace => 0x08,
        K::Enter => 0x0D,
        K::Space => 0x20,
        K::Insert => 0x2D,
        K::Delete => 0x2E,
        K::Home => 0x24,
        K::End => 0x23,
        K::PageUp => 0x21,
        K::PageDown => 0x22,
        K::ArrowLeft => 0x25,
        K::ArrowUp => 0x26,
        K::ArrowRight => 0x27,
        K::ArrowDown => 0x28,
        K::Semicolon => 0xBA,
        K::Equals | K::Plus => 0xBB,
        K::Comma => 0xBC,
        K::Minus => 0xBD,
        K::Period => 0xBE,
        K::Slash => 0xBF,
        K::Backtick => 0xC0,
        K::OpenBracket => 0xDB,
        K::Backslash => 0xDC,
        K::CloseBracket => 0xDD,
        K::Quote => 0xDE,
        _ => return None,
    };
    Some(vk)
}

/// egui's modifiers as ours (Cmd is the Mac's "Win").
pub fn mods_of_egui(m: eframe::egui::Modifiers) -> u8 {
    let mut out = 0;
    if m.ctrl {
        out |= MOD_CTRL;
    }
    if m.shift {
        out |= MOD_SHIFT;
    }
    if m.alt {
        out |= MOD_ALT;
    }
    if m.mac_cmd {
        out |= MOD_WIN;
    }
    out
}

// ---------------------------------------------------------------- the Windows hook

#[cfg(windows)]
pub use imp::KeyHook;

#[cfg(windows)]
mod imp {
    use std::cell::Cell;

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetKeyNameTextW, VK_CONTROL, VK_LCONTROL, VK_LWIN, VK_MENU, VK_RMENU, VK_RWIN, VK_SHIFT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, KillTimer, PostThreadMessageW, SetTimer, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
    };

    use super::{MOD_ALT, MOD_ALTGR, MOD_CTRL, MOD_SHIFT, MOD_WIN};

    /// While push-to-talk is held, how often the hook thread asks the system whether its key still
    /// is down (a release that went to another desktop never reaches the hook).
    const PTT_CHECK_MS: u32 = 250;

    thread_local! {
        /// This hook thread's push-to-talk check timer (0: none).
        static PTT_TIMER: Cell<usize> = const { Cell::new(0) };
    }

    /// Owns the hook thread; dropping it removes the hook.
    pub struct KeyHook {
        thread_id: u32,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl KeyHook {
        pub fn start() -> Option<KeyHook> {
            let (tx, rx) = std::sync::mpsc::channel::<Option<u32>>();
            let join = std::thread::Builder::new()
                .name("vox-key-hook".into())
                .spawn(move || {
                    // SAFETY: standard low-level hook install + message loop on this thread.
                    unsafe {
                        // Every key of the system waits for this thread: it must get the CPU at
                        // once, even next to a game using all of it (it only runs for a moment).
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0);
                        if hook.is_null() {
                            let _ = tx.send(None);
                            return;
                        }
                        let _ = tx.send(Some(GetCurrentThreadId()));
                        let mut msg: MSG = std::mem::zeroed();
                        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                            if msg.message == WM_TIMER && msg.hwnd.is_null() && msg.wParam == PTT_TIMER.get() {
                                if !super::recheck_ptt(key_down) {
                                    stop_ptt_check();
                                }
                                continue;
                            }
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        stop_ptt_check();
                        UnhookWindowsHookEx(hook);
                    }
                })
                .ok()?;
            match rx.recv().ok().flatten() {
                Some(thread_id) => {
                    tracing::info!(thread_id, "keyboard shortcuts: keyboard hook installed");
                    Some(KeyHook { thread_id, join: Some(join) })
                }
                None => {
                    let _ = join.join();
                    None
                }
            }
        }
    }

    impl Drop for KeyHook {
        fn drop(&mut self) {
            // SAFETY: posting WM_QUIT to our own hook thread.
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            tracing::info!("keyboard shortcuts: keyboard hook removed");
        }
    }

    fn key_name(k: &KBDLLHOOKSTRUCT) -> String {
        let extended = (k.flags & 1) << 24;
        let lparam = ((k.scanCode << 16) | extended) as i32;
        let mut buf = [0u16; 64];
        // SAFETY: `buf` is a valid output buffer of the stated length.
        let n = unsafe { GetKeyNameTextW(lparam, buf.as_mut_ptr(), buf.len() as i32) };
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            String::new()
        }
    }

    /// The key is down now, says the system (up, too, while another desktop has the keyboard:
    /// the lock screen, a UAC prompt).
    fn key_down(vk: u32) -> bool {
        // SAFETY: a plain read of the asynchronous key state.
        unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
    }

    fn held_mods() -> u8 {
        let down = |vk: u16| key_down(u32::from(vk));
        let mut m = 0;
        if down(VK_CONTROL) {
            m |= MOD_CTRL;
        }
        if down(VK_SHIFT) {
            m |= MOD_SHIFT;
        }
        if down(VK_MENU) {
            m |= MOD_ALT;
        }
        if down(VK_LWIN) || down(VK_RWIN) {
            m |= MOD_WIN;
        }
        if down(VK_RMENU) && down(VK_LCONTROL) {
            m |= MOD_ALTGR;
        }
        m
    }

    /// Push-to-talk went down: check it every [`PTT_CHECK_MS`] until it is up (on the hook thread,
    /// whose message loop gets the timer).
    fn start_ptt_check() {
        PTT_TIMER.with(|t| {
            if t.get() == 0 {
                // SAFETY: a thread timer (no window); WM_TIMER comes to this thread's loop.
                t.set(unsafe { SetTimer(std::ptr::null_mut(), 0, PTT_CHECK_MS, None) });
            }
        });
    }

    fn stop_ptt_check() {
        let id = PTT_TIMER.replace(0);
        if id != 0 {
            // SAFETY: the thread timer this thread made.
            unsafe { KillTimer(std::ptr::null_mut(), id) };
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == 0 {
            // SAFETY: for WH_KEYBOARD_LL with code HC_ACTION, lparam points at a KBDLLHOOKSTRUCT.
            let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let ptt = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => super::dispatch(k.vkCode, true, held_mods, || key_name(k)),
                WM_KEYUP | WM_SYSKEYUP => super::dispatch(k.vkCode, false, held_mods, || key_name(k)),
                _ => false,
            };
            if ptt {
                start_ptt_check();
            }
        }
        // SAFETY: the event goes on unchanged (never swallowed).
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(not(windows))]
pub struct KeyHook;

#[cfg(not(windows))]
impl KeyHook {
    pub fn start() -> Option<KeyHook> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u32 = 0x4D;
    const V: u32 = 0x56;
    const LCTRL: u32 = 0xA2;
    const LSHIFT: u32 = 0xA0;

    fn kb(action: Action, vk: u32, mods: u8) -> Keybind {
        Keybind { action, key: Combo::new(vk, mods), key_name: String::new() }
    }

    fn sound(server: &str, id: SoundId) -> Action {
        Action::PlaySound(SoundRef { server_id: Some(server.into()), server_url: "http://h:1".into(), sound: id, ..Default::default() })
    }

    /// Feed one event; what came out.
    fn ev(m: &mut Matcher, vk: u32, down: bool, mods: u8, typing: bool) -> Vec<Msg> {
        let mut out = Vec::new();
        m.on_key(vk, down, || mods, typing, &mut |x| out.push(x));
        out
    }

    fn fired(out: &[Msg]) -> Vec<u16> {
        out.iter().filter_map(|m| if let Msg::Fire { idx, .. } = m { Some(*idx) } else { None }).collect()
    }

    #[test]
    fn combos_are_normalized() {
        assert_eq!(Combo::new(LCTRL, MOD_CTRL | MOD_SHIFT), Combo { vk: LCTRL, mods: MOD_SHIFT });
        assert_eq!(Combo::new(M, 0xF0 | MOD_ALT), Combo { vk: M, mods: MOD_ALT });
        assert!(Combo::new(M, MOD_CTRL).has_command_mod());
        assert!(!Combo::new(M, MOD_SHIFT).has_command_mod());
        assert_eq!(Combo::new(M, MOD_CTRL | MOD_SHIFT).label(""), if cfg!(target_os = "macos") { "Control + Shift + M" } else { "Ctrl + Shift + M" });
        assert_eq!(Combo::new(0x70, 0).label(""), "F1");
        assert_eq!(Combo::default().label("x"), "(nenhuma tecla)");
    }

    #[test]
    fn a_toggle_fires_once_per_press_and_ignores_auto_repeat() {
        let mut m = Matcher::new();
        let (gen, _) = m.set(&[kb(Action::ToggleMute, M, 0)], true);
        assert_eq!(ev(&mut m, M, true, 0, false), vec![Msg::Fire { gen, idx: 0 }]);
        for _ in 0..5 {
            assert!(ev(&mut m, M, true, 0, false).is_empty(), "auto-repeat must not fire again");
        }
        assert!(ev(&mut m, M, false, 0, false).is_empty());
        assert_eq!(fired(&ev(&mut m, M, true, 0, false)), vec![0], "a new press fires again");
    }

    #[test]
    fn push_to_talk_is_held_and_released_once() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::PushToTalk, V, 0), kb(Action::PushToTalk, 0x70, 0)], true);
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Ptt(true)]);
        assert!(ev(&mut m, V, true, 0, false).is_empty(), "repeat");
        // A second push-to-talk key while the first is held: no new press, no early release.
        assert!(ev(&mut m, 0x70, true, 0, false).is_empty());
        assert!(ev(&mut m, V, false, 0, false).is_empty());
        assert_eq!(ev(&mut m, 0x70, false, 0, false), vec![Msg::Ptt(false)]);
        // Gamers hold Shift / Ctrl while talking: extra modifiers do not block a plain key.
        assert_eq!(ev(&mut m, V, true, MOD_SHIFT | MOD_CTRL, false), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, V, false, MOD_SHIFT | MOD_CTRL, false), vec![Msg::Ptt(false)]);
        // Off: push-to-talk bindings are not listened for.
        m.set(&[kb(Action::PushToTalk, V, 0)], false);
        assert!(ev(&mut m, V, true, 0, false).is_empty());
    }

    #[test]
    fn a_combination_needs_its_modifiers_and_the_most_specific_wins() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::ToggleMute, M, 0), kb(Action::ToggleDeafen, M, MOD_CTRL), kb(Action::LeaveVoice, M, MOD_CTRL | MOD_SHIFT)], true);
        assert_eq!(fired(&ev(&mut m, M, true, 0, false)), vec![0]);
        ev(&mut m, M, false, 0, false);
        assert_eq!(fired(&ev(&mut m, M, true, MOD_CTRL, false)), vec![1]);
        ev(&mut m, M, false, MOD_CTRL, false);
        assert_eq!(fired(&ev(&mut m, M, true, MOD_CTRL | MOD_SHIFT, false)), vec![2]);
        ev(&mut m, M, false, MOD_CTRL | MOD_SHIFT, false);
        assert_eq!(fired(&ev(&mut m, M, true, MOD_CTRL | MOD_ALT, false)), vec![1], "extra Alt: still the Ctrl one");
        ev(&mut m, M, false, 0, false);
        // Alt+M: only Ctrl-less bindings qualify.
        assert_eq!(fired(&ev(&mut m, M, true, MOD_ALT, false)), vec![0]);
        // Another key: nothing.
        assert!(ev(&mut m, V, true, MOD_CTRL, false).is_empty());
    }

    #[test]
    fn a_modifier_alone_is_a_key() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::PushToTalk, LCTRL, 0)], true);
        // While Ctrl is the key, the system reports Ctrl as held: still a plain match.
        assert_eq!(ev(&mut m, LCTRL, true, MOD_CTRL, false), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, LCTRL, false, 0, false), vec![Msg::Ptt(false)]);
    }

    #[test]
    fn releasing_a_modifier_of_a_held_combination_ends_it() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::PushToTalk, V, MOD_SHIFT)], true);
        assert_eq!(ev(&mut m, V, true, MOD_SHIFT, false), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, LSHIFT, false, 0, false), vec![Msg::Ptt(false)]);
        assert!(ev(&mut m, V, false, 0, false).is_empty());
    }

    #[test]
    fn typing_in_concord_pauses_plain_keys_but_not_command_combinations() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::ToggleMute, M, 0), kb(Action::ToggleDeafen, 0x44, MOD_SHIFT), kb(Action::LeaveVoice, 0x4C, MOD_CTRL), kb(Action::PushToTalk, V, 0)], true);
        assert!(ev(&mut m, M, true, 0, true).is_empty());
        assert!(ev(&mut m, 0x44, true, MOD_SHIFT, true).is_empty(), "Shift+letter is typing");
        assert!(ev(&mut m, V, true, 0, true).is_empty());
        assert_eq!(fired(&ev(&mut m, 0x4C, true, MOD_CTRL, true)), vec![2]);
        // The text box loses the focus while M is still down: the release is seen, the next
        // press fires.
        ev(&mut m, M, false, 0, false);
        assert_eq!(fired(&ev(&mut m, M, true, 0, false)), vec![0]);
        // Push-to-talk pressed, then the user clicks into the chat: the release still ends it.
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, V, false, 0, true), vec![Msg::Ptt(false)]);
    }

    #[test]
    fn keys_that_do_not_type_still_work_while_typing() {
        // v1.0.2's push-to-talk worked whatever had the focus: a key that types nothing keeps doing so.
        const RCTRL: u32 = 0xA3;
        const F13: u32 = 0x7C;
        const CAPS: u32 = 0x14;
        let mut m = Matcher::new();
        m.set(&[kb(Action::PushToTalk, RCTRL, 0), kb(Action::ToggleMute, F13, 0), kb(Action::ToggleDeafen, CAPS, MOD_SHIFT), kb(Action::LeaveVoice, 0x25, 0)], true);
        assert_eq!(ev(&mut m, RCTRL, true, MOD_CTRL, true), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, RCTRL, false, 0, true), vec![Msg::Ptt(false)]);
        assert_eq!(fired(&ev(&mut m, F13, true, 0, true)), vec![1]);
        assert_eq!(fired(&ev(&mut m, CAPS, true, MOD_SHIFT, true)), vec![2]);
        assert!(ev(&mut m, 0x25, true, 0, true).is_empty(), "the left arrow edits text");
        for vk in [0x41, 0x5A, 0x30, 0x20, 0x0D, 0x08, 0x60, 0xBA, 0xC0, 0xE2] {
            assert!(Combo::new(vk, MOD_SHIFT).is_typing(), "{vk:#x}");
            assert!(!Combo::new(vk, MOD_CTRL).is_typing(), "{vk:#x}");
        }
        for vk in [0x70, 0x87, 0xA0, 0xA3, 0xA5, 0x5B, 0x14, 0x2D, 0x13, 0xB3] {
            assert!(!Combo::new(vk, 0).is_typing(), "{vk:#x}");
        }
    }

    #[test]
    fn conflicting_bindings_fire_together_and_are_reported() {
        let list = vec![kb(Action::ToggleMute, M, MOD_CTRL), kb(Action::ToggleDeafen, M, MOD_CTRL), kb(Action::LeaveVoice, M, 0), Keybind::new(Action::ToggleGoLive), Keybind::new(Action::ToggleMute)];
        assert_eq!(conflicts(&list, 0).collect::<Vec<_>>(), vec![1]);
        assert_eq!(conflicts(&list, 1).collect::<Vec<_>>(), vec![0]);
        assert_eq!(conflicts(&list, 2).count(), 0);
        assert_eq!(conflicts(&list, 3).count(), 0, "two bindings without a key do not conflict");
        let mut m = Matcher::new();
        m.set(&list, true);
        assert_eq!(fired(&ev(&mut m, M, true, MOD_CTRL, false)), vec![0, 1]);
    }

    #[test]
    fn a_new_table_forgets_what_was_held_and_says_so() {
        let mut m = Matcher::new();
        let (g1, _) = m.set(&[kb(Action::PushToTalk, V, 0), kb(Action::ToggleMute, M, 0)], true);
        ev(&mut m, V, true, 0, false);
        let (g2, was) = m.set(&[kb(Action::ToggleMute, M, 0)], true);
        assert!(was);
        assert_ne!(g1, g2);
        assert!(ev(&mut m, V, false, 0, false).is_empty());
        assert_eq!(ev(&mut m, M, true, 0, false), vec![Msg::Fire { gen: g2, idx: 0 }]);
        // Indexes are the app's list positions (bindings without a key are skipped, not renumbered).
        let (g3, _) = m.set(&[Keybind::new(Action::ToggleDeafen), kb(Action::ToggleMute, V, 0)], true);
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Fire { gen: g3, idx: 1 }]);
    }

    #[test]
    fn recording_a_key() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::ToggleMute, M, 0)], true);
        m.begin_capture();
        // Ctrl, Shift, then M: the combination (and nothing fires meanwhile).
        assert!(ev(&mut m, LCTRL, true, 0, false).is_empty());
        assert!(ev(&mut m, LSHIFT, true, MOD_CTRL, false).is_empty());
        assert_eq!(ev(&mut m, M, true, MOD_CTRL | MOD_SHIFT, true), vec![Msg::Captured { combo: Combo::new(M, MOD_CTRL | MOD_SHIFT), name: String::new() }]);
        assert!(!m.capturing());
        ev(&mut m, M, false, MOD_CTRL | MOD_SHIFT, false);
        // A modifier pressed and released alone is the key.
        m.begin_capture();
        assert!(ev(&mut m, LCTRL, true, MOD_CTRL, false).is_empty());
        assert_eq!(ev(&mut m, LCTRL, false, 0, false), vec![Msg::Captured { combo: Combo::new(LCTRL, 0), name: String::new() }]);
        // Esc cancels; Shift+Esc is a key.
        m.begin_capture();
        assert_eq!(ev(&mut m, VK_ESCAPE, true, 0, false), vec![Msg::CaptureCancelled]);
        m.begin_capture();
        assert_eq!(ev(&mut m, VK_ESCAPE, true, MOD_SHIFT, false), vec![Msg::Captured { combo: Combo::new(VK_ESCAPE, MOD_SHIFT), name: String::new() }]);
        m.cancel_capture();
        assert!(!m.capturing());
    }

    #[test]
    fn saved_and_read_back_and_unknown_entries_are_skipped() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "de_list")]
            keybinds: Vec<Keybind>,
        }
        let list = vec![kb(Action::ToggleMute, M, MOD_CTRL), kb(Action::PushToTalk, V, 0), kb(sound("srv", 7), 0x70, MOD_ALT), Keybind::new(Action::ToggleGoLive)];
        let text = serde_json::json!({ "keybinds": list }).to_string();
        let back: Holder = serde_json::from_str(&text).unwrap();
        assert_eq!(back.keybinds, list);
        // A newer app's action, a broken entry and a key with stray bits: the rest still loads.
        let text = r#"{"keybinds":[{"action":{"kind":"toggle_mute"},"key":{"vk":77,"mods":241}},{"action":{"kind":"open_the_pod_bay_doors"},"key":{"vk":80}},7,{"action":{"kind":"leave_voice"}}]}"#;
        let back: Holder = serde_json::from_str(text).unwrap();
        assert_eq!(back.keybinds, vec![kb(Action::ToggleMute, M, MOD_CTRL), Keybind::new(Action::LeaveVoice)]);
        // Not even a list: nothing, no error.
        let back: Holder = serde_json::from_str(r#"{"keybinds":{"x":1}}"#).unwrap();
        assert!(back.keybinds.is_empty());
    }

    #[test]
    fn the_old_push_to_talk_key_is_adopted_and_mirrored_back() {
        // An old settings file: only ptt_vk.
        let mut list = Vec::new();
        assert!(adopt_legacy_ptt(&mut list, V, "V"));
        assert_eq!(list, vec![Keybind { action: Action::PushToTalk, key: Combo::new(V, 0), key_name: "V".into() }]);
        assert!(!adopt_legacy_ptt(&mut list, V, "V"), "already there");
        assert_eq!(legacy_ptt(&list), (V, "V".to_string()));
        // An old app changed the key since: it replaces the plain push-to-talk binding, the
        // other bindings stay.
        list.push(kb(Action::ToggleMute, M, 0));
        list.push(kb(Action::PushToTalk, 0x70, MOD_CTRL));
        assert!(adopt_legacy_ptt(&mut list, 0x71, "F2"));
        assert_eq!(list.iter().filter(|k| k.action.is_ptt()).map(|k| k.key).collect::<Vec<_>>(), vec![Combo::new(0x71, 0), Combo::new(0x70, MOD_CTRL)]);
        assert_eq!(list[0].key, Combo::new(0x71, 0), "in the old one's place");
        // No key in the old field: nothing to adopt.
        assert!(!adopt_legacy_ptt(&mut list, 0, ""));
        // A combination can not be told to an old app: it gets no key (and asks for one).
        let only_combo = vec![kb(Action::PushToTalk, V, MOD_CTRL)];
        assert_eq!(legacy_ptt(&only_combo).0, 0);
        assert!(has_ptt(&only_combo));
        assert_eq!(legacy_ptt(&[]), (0, String::new()));
        assert!(!has_ptt(&[Keybind::new(Action::PushToTalk)]));
    }

    #[test]
    fn the_older_app_gets_a_single_key_even_when_a_combination_comes_first() {
        let list = vec![kb(Action::PushToTalk, V, MOD_CTRL), kb(Action::ToggleMute, M, 0), kb(Action::PushToTalk, 0x70, 0)];
        assert_eq!(legacy_ptt(&list).0, 0x70);
        // And reading it back changes nothing.
        let mut back = list.clone();
        assert!(!adopt_legacy_ptt(&mut back, 0x70, "F1"));
        assert_eq!(back, list);
    }

    #[test]
    fn altgr_types_and_does_not_trigger_shortcuts() {
        // pt-BR ABNT2: AltGr+Q is "/", seen by the system as Ctrl+Alt+Q.
        const Q: u32 = 0x51;
        const RALT: u32 = 0xA5;
        let altgr = MOD_CTRL | MOD_ALT | MOD_ALTGR;
        let mut m = Matcher::new();
        m.set(&[kb(Action::ToggleMute, Q, MOD_CTRL), kb(Action::ToggleDeafen, Q, MOD_ALT), kb(Action::LeaveVoice, Q, 0)], true);
        assert!(ev(&mut m, Q, true, altgr, false).is_empty(), "AltGr+Q is a character, not Ctrl+Q / Alt+Q / Q");
        ev(&mut m, Q, false, 0, false);
        // A real Ctrl+Q still works.
        assert_eq!(fired(&ev(&mut m, Q, true, MOD_CTRL, false)), vec![0]);
        ev(&mut m, Q, false, 0, false);
        // A Ctrl+Alt binding takes AltGr (as the system's own hotkeys), except while typing here.
        m.set(&[kb(Action::ToggleMute, Q, MOD_CTRL | MOD_ALT)], true);
        assert!(ev(&mut m, Q, true, altgr, true).is_empty());
        ev(&mut m, Q, false, 0, false);
        assert_eq!(fired(&ev(&mut m, Q, true, altgr, false)), vec![0]);
        ev(&mut m, Q, false, 0, false);
        // The right Alt itself as push-to-talk (its own press may already read as AltGr).
        m.set(&[kb(Action::PushToTalk, RALT, 0)], true);
        assert_eq!(ev(&mut m, RALT, true, altgr, false), vec![Msg::Ptt(true)]);
        assert_eq!(ev(&mut m, RALT, false, 0, false), vec![Msg::Ptt(false)]);
        // Recording through AltGr: Ctrl+Alt+Q (the flag itself is never saved).
        m.begin_capture();
        assert_eq!(ev(&mut m, Q, true, altgr, false), vec![Msg::Captured { combo: Combo::new(Q, MOD_CTRL | MOD_ALT), name: String::new() }]);
    }

    #[test]
    fn a_push_to_talk_release_that_never_came_ends_it() {
        let mut m = Matcher::new();
        m.set(&[kb(Action::PushToTalk, V, 0), kb(Action::ToggleMute, M, 0)], true);
        let check = |m: &mut Matcher, down: bool| {
            let mut out = Vec::new();
            m.release_stale_ptt(|_| down, &mut |x| out.push(x));
            out
        };
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Ptt(true)]);
        assert!(m.ptt_down());
        assert!(check(&mut m, true).is_empty(), "still down: nothing");
        assert!(m.ptt_down());
        // Win+L: the release went to the lock screen.
        assert_eq!(check(&mut m, false), vec![Msg::Ptt(false)]);
        assert!(!m.ptt_down());
        assert!(check(&mut m, false).is_empty(), "once");
        // The late release changes nothing; the next press talks again.
        assert!(ev(&mut m, V, false, 0, false).is_empty());
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Ptt(true)]);
        ev(&mut m, V, false, 0, false);
        // A toggle's held state is left alone (no extra press when its repeat comes).
        assert_eq!(fired(&ev(&mut m, M, true, 0, false)), vec![1]);
        assert!(check(&mut m, false).is_empty());
        assert!(ev(&mut m, M, true, 0, false).is_empty());
        // The window (no hook) lost the focus with keys down: all of them are let go.
        assert_eq!(ev(&mut m, V, true, 0, false), vec![Msg::Ptt(true)]);
        let mut out = Vec::new();
        m.release_all(&mut |x| out.push(x));
        assert_eq!(out, vec![Msg::Ptt(false)]);
        assert_eq!(fired(&ev(&mut m, M, true, 0, false)), vec![1], "the next press of a toggle fires");
        out.clear();
        m.release_all(&mut |x| out.push(x));
        assert!(out.is_empty(), "push-to-talk was not held: nothing to end");
    }

    #[test]
    fn a_sound_belongs_to_its_server() {
        let Action::PlaySound(s) = sound("abc", 3) else { unreachable!() };
        assert!(s.on_server(Some("abc"), "http://other:1"), "same identity at another address");
        assert!(!s.on_server(Some("xyz"), "http://h:1"), "another server at the same address");
        assert!(s.on_server(None, "http://h:1"), "an older hub: by address");
        assert!(!s.on_server(None, "http://h:2"));
        let old = SoundRef { server_id: None, server_url: String::new(), ..s };
        assert!(!old.on_server(None, ""), "no address, no match");
    }

    #[test]
    fn egui_keys_map_to_the_hook_codes() {
        use eframe::egui::{Key, Modifiers};
        assert_eq!(vk_of_egui(Key::M), Some(M));
        assert_eq!(vk_of_egui(Key::F1), Some(0x70));
        assert_eq!(vk_of_egui(Key::Num0), Some(0x30));
        assert_eq!(vk_of_egui(Key::Escape), Some(VK_ESCAPE));
        assert_eq!(mods_of_egui(Modifiers { ctrl: true, shift: true, ..Default::default() }), MOD_CTRL | MOD_SHIFT);
        assert_eq!(mods_of_egui(Modifiers { mac_cmd: true, command: true, ..Default::default() }), MOD_WIN);
    }
}
