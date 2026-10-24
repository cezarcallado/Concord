//! Webcam configuration (v0.6): the native format, the driver's image controls (brightness,
//! exposure, focus...) and our own picture processing (mirror, rotation, digital zoom), saved
//! **per camera** in `settings.json` and applied live to the running camera and the preview.
//!
//! - [`CameraProfile`]: one camera's saved choices, keyed by the device's symbolic link (or its
//!   name when the link is unknown). A camera plugged into another USB port gets a new link; it
//!   is found again by name and the entry moves to the new key.
//! - Only what differs from the defaults is stored (controls left alone are not written, so the
//!   camera keeps its own automatic behavior).
//! - "Configurar câmera" (v0.8.1): a window with a big live preview (the call camera, or a
//!   preview opened only while the window is open), the driver's controls grouped as Imagem /
//!   Luz / Lente with Auto where the camera has it, "Priorizar fluidez" (keep 30 fps in a dim
//!   room), format and quality, mirror / rotation / zoom, and the driver's own dialog
//!   ("Configurações avançadas do fabricante…"). Opened from the voice bar's ⚙, my camera tile's
//!   menu and Settings → Câmera (which keeps only a summary).
//! - `impl App`: that window, the Settings section, and the commands that push the profile to
//!   the backend whenever it changes.

use std::collections::BTreeMap;

use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};
use vox_capture::{CameraAdjust, CameraControl, CameraControlId, CameraMode, CameraRotation, CameraSubtype, CameraTransform};
use vox_client_core::golive::camera::CameraSetup;

use crate::app::{App, CameraPreviewUi, ICON_CAMERA, RED};
use crate::backend::{CameraLiveInfo, Cmd};
use crate::forms;
use crate::settings::CameraPreset;
use crate::theme::{self, ph, radius, size, space};
use crate::ui_kit::{self, Kind, Size};

/// A native mode, as saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMode {
    pub width: u32,
    pub height: u32,
    /// Frames per second times 1000.
    pub fps_milli: u32,
    /// "NV12", "YUY2" or "MJPG".
    pub format: String,
}

impl SavedMode {
    pub fn of(m: &CameraMode) -> Self {
        Self { width: m.width, height: m.height, fps_milli: m.fps_milli, format: m.subtype.name().to_string() }
    }

    pub fn mode(&self) -> Option<CameraMode> {
        Some(CameraMode { width: self.width, height: self.height, fps_milli: self.fps_milli, subtype: CameraSubtype::from_name(&self.format)? })
    }
}

/// A control the user set: a value, or the driver's automatic mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedControl {
    pub value: i32,
    pub auto: bool,
}

/// One camera's saved configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraProfile {
    /// Friendly name (finds the camera again when its link changed).
    pub name: String,
    /// `None` = "Automático": the closest native mode to the quality preset.
    pub mode: Option<SavedMode>,
    /// By [`CameraControlId::key`]; absent = left as the camera has it.
    pub controls: BTreeMap<String, SavedControl>,
    /// "Espelhar minha prévia" (on by default).
    pub mirror_preview: bool,
    /// "Espelhar para os outros".
    pub mirror_others: bool,
    /// Degrees clockwise: 0, 90, 180 or 270.
    pub rotation: u32,
    /// Digital zoom in percent, 100..=200.
    pub zoom_pct: u32,
    /// "Priorizar fluidez" was turned on (v0.8.1). On a camera with an auto-exposure priority
    /// the setting itself is that control (in `controls`); without one this marks the manual
    /// exposure as the one "Priorizar fluidez" chose.
    pub smooth: bool,
}

impl Default for CameraProfile {
    fn default() -> Self {
        Self { name: String::new(), mode: None, controls: BTreeMap::new(), mirror_preview: true, mirror_others: false, rotation: 0, zoom_pct: 100, smooth: false }
    }
}

impl CameraProfile {
    /// Nothing differs from the defaults (then it is not stored at all).
    pub fn is_default(&self) -> bool {
        Self { name: self.name.clone(), ..Self::default() } == *self
    }

    pub fn transform(&self) -> CameraTransform {
        CameraTransform {
            mirror_preview: self.mirror_preview,
            mirror_others: self.mirror_others,
            rotation: CameraRotation::from_degrees(self.rotation),
            zoom_pct: self.zoom_pct,
        }
        .sanitized()
    }

    pub fn adjustments(&self) -> Vec<CameraAdjust> {
        self.controls
            .iter()
            .filter_map(|(k, c)| Some(CameraAdjust { id: CameraControlId::from_key(k)?, value: c.value, auto: c.auto }))
            .collect()
    }

    /// What the backend needs to open (or update) this camera.
    pub fn to_setup(&self) -> CameraSetup {
        CameraSetup { mode: self.mode.as_ref().and_then(SavedMode::mode), transform: self.transform(), controls: self.adjustments() }
    }

    pub fn set_control(&mut self, id: CameraControlId, value: i32, auto: bool) {
        self.controls.insert(id.key().to_string(), SavedControl { value, auto });
    }
}

/// Every camera's profile, by device key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CameraProfiles(pub BTreeMap<String, CameraProfile>);

/// The key a camera is stored under: its symbolic link, else its name.
pub fn profile_key(id: &str, name: &str) -> String {
    if id.is_empty() { name.to_string() } else { id.to_string() }
}

impl CameraProfiles {
    /// The camera's profile: by link, else by name (the same camera on another USB port).
    pub fn get(&self, id: &str, name: &str) -> Option<&CameraProfile> {
        self.0.get(&profile_key(id, name)).or_else(|| self.find_by_name(name).map(|k| &self.0[&k]))
    }

    fn find_by_name(&self, name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        self.0.iter().find(|(_, p)| p.name.eq_ignore_ascii_case(name)).map(|(k, _)| k.clone())
    }

    /// The profile to change; created when the camera has none. A profile found by name under
    /// another link moves to the camera's current key.
    pub fn edit(&mut self, id: &str, name: &str) -> &mut CameraProfile {
        let key = profile_key(id, name);
        if !self.0.contains_key(&key) {
            let moved = self.find_by_name(name).and_then(|old| self.0.remove(&old));
            self.0.insert(key.clone(), moved.unwrap_or_default());
        }
        let p = self.0.get_mut(&key).expect("inserted above");
        if !name.is_empty() {
            p.name = name.to_string();
        }
        p
    }

    /// Forget a camera whose profile is back to the defaults.
    pub fn tidy(&mut self, id: &str, name: &str) {
        let key = profile_key(id, name);
        if self.0.get(&key).is_some_and(|p| p.is_default()) {
            self.0.remove(&key);
        }
    }
}


/// The sections of "Configurar câmera", in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlGroup {
    Image,
    Light,
    Lens,
}

impl ControlGroup {
    pub const ALL: [ControlGroup; 3] = [ControlGroup::Image, ControlGroup::Light, ControlGroup::Lens];

    pub fn title(self) -> &'static str {
        match self {
            ControlGroup::Image => "Imagem",
            ControlGroup::Light => "Luz",
            ControlGroup::Lens => "Lente",
        }
    }

    /// The group's icon in "Configurar câmera".
    pub fn icon(self) -> &'static str {
        match self {
            ControlGroup::Image => ph::IMAGE,
            ControlGroup::Light => ph::SUN,
            ControlGroup::Lens => ph::APERTURE,
        }
    }

    /// The controls of this section, in the order they are listed.
    pub fn members(self) -> &'static [CameraControlId] {
        use CameraControlId::*;
        match self {
            ControlGroup::Image => &[Brightness, Contrast, Saturation, Sharpness, Gamma],
            ControlGroup::Light => &[Exposure, Gain, Backlight, WhiteBalance],
            ControlGroup::Lens => &[Focus, Zoom],
        }
    }
}

/// The camera's controls by section; sections the camera has nothing for are left out. (The
/// auto-exposure priority is the "Priorizar fluidez" switch, not a slider.)
pub fn group_controls(controls: &[CameraControl]) -> Vec<(ControlGroup, Vec<CameraControl>)> {
    ControlGroup::ALL
        .into_iter()
        .filter_map(|g| {
            let list: Vec<CameraControl> = g.members().iter().filter_map(|id| controls.iter().find(|c| c.id == *id).copied()).collect();
            (!list.is_empty()).then_some((g, list))
        })
        .collect()
}

/// What a control shows: (value, "Auto" ticked). With Auto on the value is the camera's own,
/// as last read back; a manual value the user saved is shown as saved (the camera may round it
/// a moment later). Auto is only ever ticked on a control that has it.
pub fn shown_setting(c: &CameraControl, saved: Option<SavedControl>) -> (i32, bool) {
    let auto = match saved {
        Some(s) => s.auto && c.can_auto,
        None => c.auto && c.can_auto,
    };
    let value = match saved {
        Some(s) if !auto => s.value,
        _ => c.value,
    };
    (c.snap(value), auto)
}

/// How "Priorizar fluidez" is done on this camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmoothMethod {
    /// The camera's auto-exposure priority: set to "keep the frame rate", exposure stays automatic.
    Priority,
    /// No such control: a manual exposure short enough for the frame rate (gain automatic when
    /// the camera has that).
    ManualExposure,
    /// Neither: nothing to do.
    Unsupported,
}

pub fn smooth_method(controls: &[CameraControl]) -> SmoothMethod {
    let find = |id| controls.iter().find(|c| c.id == id);
    if find(CameraControlId::ExposurePriority).is_some_and(|c| c.can_manual) {
        SmoothMethod::Priority
    } else if find(CameraControlId::Exposure).is_some_and(|c| c.can_manual) {
        SmoothMethod::ManualExposure
    } else {
        SmoothMethod::Unsupported
    }
}

/// The control changes that turn "Priorizar fluidez" on or off for a camera running at `fps`.
pub fn smooth_plan(controls: &[CameraControl], fps: f32, on: bool) -> Vec<CameraAdjust> {
    let find = |id| controls.iter().find(|c| c.id == id).copied();
    match smooth_method(controls) {
        SmoothMethod::Priority => {
            let p = find(CameraControlId::ExposurePriority).expect("method checked");
            vec![CameraAdjust { id: p.id, value: if on { p.min } else { p.max }, auto: false }]
        }
        SmoothMethod::ManualExposure => {
            let e = find(CameraControlId::Exposure).expect("method checked");
            if on {
                let mut v = vec![CameraAdjust { id: e.id, value: vox_capture::exposure_for_fps(&e, fps), auto: false }];
                if let Some(g) = find(CameraControlId::Gain).filter(|g| g.can_auto) {
                    v.push(CameraAdjust { id: g.id, value: g.value, auto: true });
                }
                v
            } else if e.can_auto {
                vec![CameraAdjust { id: e.id, value: e.value, auto: true }]
            } else {
                Vec::new()
            }
        }
        SmoothMethod::Unsupported => Vec::new(),
    }
}

/// Is "Priorizar fluidez" on, as the profile and the camera tell?
pub fn smooth_active(controls: &[CameraControl], profile: &CameraProfile) -> bool {
    let saved = |id: CameraControlId| profile.controls.get(id.key()).copied();
    match smooth_method(controls) {
        SmoothMethod::Priority => {
            let p = controls.iter().find(|c| c.id == CameraControlId::ExposurePriority).expect("method checked");
            shown_setting(p, saved(p.id)).0 == p.min
        }
        SmoothMethod::ManualExposure => {
            let e = controls.iter().find(|c| c.id == CameraControlId::Exposure).expect("method checked");
            profile.smooth && !shown_setting(e, saved(e.id)).1
        }
        SmoothMethod::Unsupported => false,
    }
}

/// What the driver's own dialog changed: controls whose automatic mode flipped, or whose manual
/// value moved (an automatic value drifting by itself is not a change).
pub fn native_changes(before: &[CameraControl], after: &[CameraControl]) -> Vec<CameraAdjust> {
    after
        .iter()
        .filter(|a| {
            before.iter().find(|b| b.id == a.id).is_some_and(|b| a.auto != b.auto || (!a.auto && a.value != b.value))
        })
        .map(|a| CameraAdjust { id: a.id, value: a.value, auto: a.auto })
        .collect()
}

/// The rate "Priorizar fluidez" aims at: what the opened mode promises, else 30.
fn target_fps(l: &CameraLiveInfo) -> f32 {
    if l.requested >= 1.0 { l.requested } else { 30.0 }
}

/// What the camera UI knows about the running camera / preview.
#[derive(Default)]
pub struct CamUi {
    /// Native modes of the selected camera (`formats_for` = its device key).
    pub formats: Vec<CameraMode>,
    pub formats_for: String,
    /// The latest from the running capture (call camera, else the preview).
    pub live: Option<crate::backend::CameraLiveInfo>,
    /// A change not written to settings.json yet (written when the mouse button is released).
    pub dirty: bool,
    /// The low-light hint was already shown as a notice for this run of the camera.
    pub low_light_noticed: bool,
    /// "Configurar câmera" is open.
    pub window: bool,
    /// The window's picture follows the call camera (on) or a local preview (off): a change
    /// restarts the preview.
    pub call_seen: Option<bool>,
    /// The driver's dialog is open; the controls as they were before it opened.
    pub native_open: bool,
    pub native_before: Option<Vec<CameraControl>>,
    /// After the driver's dialog closed: values read back until then are taken into the profile.
    pub native_adopt_until: Option<std::time::Instant>,
    pub native_error: Option<String>,
}

pub const LOW_LIGHT_TIP: &str = "Pouca luz: a câmera reduziu os quadros por segundo para clarear a imagem. Acenda uma luz ou ligue \"Priorizar fluidez\" em Configurar câmera (a engrenagem ao lado de Câmera).";

const ROTATIONS: [(u32, &str); 4] = [(0, "Sem rotação"), (90, "90° (horário)"), (180, "180°"), (270, "270° (anti-horário)")];

/// The window's preview at most this big (16:9).
const PREVIEW_W: f32 = 640.0;
const PREVIEW_H: f32 = 360.0;

impl App {
    /// The camera the settings refer to: the chosen one, else the first listed (what "Padrão"
    /// opens). Empty strings when nothing is known yet.
    pub fn camera_ident(&self) -> (String, String) {
        match (&self.settings.camera_device, &self.settings.camera_name) {
            (Some(id), name) => (id.clone(), name.clone().unwrap_or_default()),
            (None, Some(name)) => (String::new(), name.clone()),
            (None, None) => self.settings_ui.cameras.first().map(|c| (c.id.clone(), c.name.clone())).unwrap_or_default(),
        }
    }

    pub fn camera_profile(&self) -> CameraProfile {
        let (id, name) = self.camera_ident();
        self.settings.camera_profiles.get(&id, &name).cloned().unwrap_or_default()
    }

    pub fn camera_setup(&self) -> CameraSetup {
        self.camera_profile().to_setup()
    }

    /// Tell the backend the current camera's setup (it applies what changed to the running
    /// camera and uses it for the next start).
    pub fn send_camera_setup(&self) {
        self.backend.send(Cmd::CameraSetup {
            id: self.settings.camera_device.clone(),
            name: self.settings.camera_name.clone(),
            setup: self.camera_setup(),
        });
    }

    /// Change the current camera's profile, push it to the camera and mark settings.json dirty.
    pub fn edit_camera_profile(&mut self, f: impl FnOnce(&mut CameraProfile)) {
        let (id, name) = self.camera_ident();
        f(self.settings.camera_profiles.edit(&id, &name));
        self.settings.camera_profiles.tidy(&id, &name);
        self.settings_ui.cam.dirty = true;
        self.send_camera_setup();
    }

    /// Write settings.json once the mouse button is up (a slider drag changes the profile every
    /// frame; the file is written once).
    pub fn flush_camera_settings(&mut self, pointer_down: bool) {
        if self.settings_ui.cam.dirty && !pointer_down {
            self.settings_ui.cam.dirty = false;
            self.save_settings();
        }
    }

    /// The running camera reported (once a second, when something changed). Dim light while
    /// the camera is in the call also gets a notice, once per run. Right after the driver's
    /// dialog closed, what it changed is taken into the profile.
    pub fn camera_live(&mut self, l: CameraLiveInfo) {
        let cam = &mut self.settings_ui.cam;
        if !l.low_light {
            cam.low_light_noticed = false;
        } else if l.from_call && !cam.low_light_noticed {
            cam.low_light_noticed = true;
            self.notice = Some(format!("{ICON_CAMERA} {LOW_LIGHT_TIP}"));
        }
        if self.settings_ui.cam.native_adopt_until.is_some_and(|t| std::time::Instant::now() < t) {
            if let Some(before) = self.settings_ui.cam.native_before.clone() {
                self.adopt_native_changes(&before, &l.controls);
            }
        }
        self.settings_ui.cam.live = Some(l);
    }

    /// Put what the driver's dialog changed into the profile (only what the profile does not
    /// say already, so this can run on every report).
    fn adopt_native_changes(&mut self, before: &[CameraControl], after: &[CameraControl]) {
        let profile = self.camera_profile();
        let todo: Vec<CameraAdjust> = native_changes(before, after)
            .into_iter()
            .filter(|a| profile.controls.get(a.id.key()) != Some(&SavedControl { value: a.value, auto: a.auto }))
            .collect();
        if todo.is_empty() {
            return;
        }
        tracing::info!(changes = ?todo, "camera: taking the driver dialog's changes into the profile");
        self.edit_camera_profile(|p| {
            for a in &todo {
                p.set_control(a.id, a.value, a.auto);
                if a.id == CameraControlId::Exposure && a.auto {
                    p.smooth = false;
                }
            }
        });
    }

    /// Ask the backend for the selected camera's native modes.
    pub fn request_camera_formats(&mut self) {
        let (id, name) = self.camera_ident();
        self.settings_ui.cam.formats_for = profile_key(&id, &name);
        self.backend.send(Cmd::ListCameraFormats {
            id: self.settings.camera_device.clone(),
            name: self.settings.camera_name.clone(),
            key: self.settings_ui.cam.formats_for.clone(),
        });
    }

    // ------------------------------------------------------------ "Configurar câmera"

    /// Open "Configurar câmera" (the voice bar's ⚙, my camera tile's menu, Settings → Câmera).
    /// The picture is the call camera when it is on, else a preview opened only while the window
    /// is open. The Settings window, when open, waits behind it.
    pub fn open_camera_window(&mut self) {
        let cam = &mut self.settings_ui.cam;
        if cam.window {
            return;
        }
        cam.window = true;
        cam.call_seen = None;
        cam.native_error = None;
        tracing::info!(call_camera = self.camera_on(), "camera window opened");
        self.backend.send(Cmd::CameraWatch(true));
        // Names for the picker (and which camera "Padrão" is); the formats come after the
        // preview started (`camera_window_keep_preview`).
        self.backend.send(Cmd::ListCameras);
    }

    /// Close it: the preview (if it was ours) releases the camera.
    pub fn close_camera_window(&mut self) {
        if !self.settings_ui.cam.window {
            return;
        }
        self.settings_ui.cam.window = false;
        self.settings_ui.cam.call_seen = None;
        self.set_camera_preview(false);
        self.backend.send(Cmd::CameraWatch(false));
        self.flush_camera_settings(false);
        tracing::info!("camera window closed");
    }

    /// The window shows a picture (call.rs adds the preview's video tile for it).
    pub fn camera_window_preview(&self) -> bool {
        self.settings_ui.cam.window && self.settings_ui.camera_preview == CameraPreviewUi::On
    }

    /// Keep a picture in the window: the call camera's self-view while it is on, else a local
    /// preview. Restarted when the call camera goes on or off, or a preview ended.
    fn camera_window_keep_preview(&mut self) {
        let on = self.camera_on();
        let starting = self.voice.camera == crate::app::CameraUi::Starting;
        let changed = self.settings_ui.cam.call_seen != Some(on);
        if !starting && (changed || self.settings_ui.camera_preview == CameraPreviewUi::Off) {
            self.settings_ui.cam.call_seen = Some(on);
            self.set_camera_preview(true);
            // Once the capture runs it answers with its own list (no second open of the device).
            self.request_camera_formats();
        }
    }

    /// "Priorizar fluidez" on or off for the running camera (saved in its profile).
    pub fn set_camera_smooth(&mut self, on: bool) {
        let Some(l) = self.settings_ui.cam.live.clone() else { return };
        let plan = smooth_plan(&l.controls, target_fps(&l), on);
        tracing::info!(on, method = ?smooth_method(&l.controls), plan = ?plan, "camera: priorizar fluidez");
        self.edit_camera_profile(|p| {
            for a in &plan {
                p.set_control(a.id, a.value, a.auto);
            }
            p.smooth = on;
        });
    }

    /// "Restaurar padrão": every image control back to the camera's default (automatic where it
    /// has one) and "Priorizar fluidez" off. Mirror, rotation and digital zoom stay.
    pub fn reset_camera_controls(&mut self) {
        self.edit_camera_profile(|p| {
            p.controls.clear();
            p.smooth = false;
        });
        self.backend.send(Cmd::CameraResetControls {
            id: self.settings.camera_device.clone(),
            name: self.settings.camera_name.clone(),
        });
    }

    /// "Configurações avançadas do fabricante…".
    fn open_camera_native_dialog(&mut self) {
        let cam = &mut self.settings_ui.cam;
        cam.native_open = true;
        cam.native_error = None;
        cam.native_before = cam.live.as_ref().map(|l| l.controls.clone());
        cam.native_adopt_until = None;
        let (id, name) = self.camera_ident();
        self.backend.send(Cmd::CameraNativeDialog { id: (!id.is_empty()).then_some(id), name: (!name.is_empty()).then_some(name) });
    }

    /// The driver's dialog was closed: read the controls back and take what changed.
    pub fn camera_native_closed(&mut self, res: Result<(), String>) {
        let cam = &mut self.settings_ui.cam;
        cam.native_open = false;
        match res {
            Ok(()) => {
                cam.native_adopt_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
                self.backend.send(Cmd::CameraRefreshControls);
                // The window reads the controls back every second, so what the dialog changed
                // may already be here (and the backend only reports changes).
                if let (Some(before), Some(l)) = (self.settings_ui.cam.native_before.clone(), self.settings_ui.cam.live.clone()) {
                    self.adopt_native_changes(&before, &l.controls);
                }
            }
            Err(e) => cam.native_error = Some(e),
        }
    }

    /// The camera picker; returns whether another camera was chosen.
    fn camera_device_combo(&mut self, ui: &mut egui::Ui, salt: &str) -> bool {
        let mut picked = false;
        let current = match (&self.settings.camera_device, &self.settings.camera_name) {
            (Some(_), Some(n)) => n.clone(),
            (Some(id), None) => id.clone(),
            _ => "Padrão (a primeira câmera)".to_string(),
        };
        egui::ComboBox::from_id_salt(salt).width(forms::combo_w(ui)).selected_text(current).show_ui(ui, |ui| {
            if ui.selectable_label(self.settings.camera_device.is_none(), "Padrão (a primeira câmera)").clicked() {
                self.settings.camera_device = None;
                self.settings.camera_name = None;
                picked = true;
            }
            for c in self.settings_ui.cameras.clone() {
                if ui.selectable_label(self.settings.camera_device.as_deref() == Some(c.id.as_str()), &c.name).clicked() {
                    self.settings.camera_device = Some(c.id.clone());
                    self.settings.camera_name = Some(c.name.clone());
                    picked = true;
                }
            }
        });
        if picked {
            // Another camera: its own formats, controls and processing.
            self.settings_ui.cam.live = None;
            self.settings_ui.cam.formats.clear();
            self.send_camera_setup();
            self.save_settings();
        }
        picked
    }

    /// "Câmera" in Configurações → Vídeo: the camera, a summary and the button to "Configurar
    /// câmera". Returns whether a setting changed.
    pub fn camera_section(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        forms::group(ui, "Câmera");
        forms::row(ui, "Câmera", |ui| {
            if self.camera_device_combo(ui, "camera_device") {
                changed = true;
                self.request_camera_formats();
            }
        });
        if let Some(e) = &self.settings_ui.cameras_error {
            forms::note_colored(ui, format!("{} {e}", ph::WARNING), true, theme::AVISO);
        } else if self.settings_ui.cameras.is_empty() {
            forms::note(ui, "Nenhuma câmera encontrada.", true);
        }
        let p = self.camera_profile();
        let mut summary = vec![
            format!("Formato: {}", p.mode.as_ref().and_then(SavedMode::mode).map_or("Automático".to_string(), |m| m.label())),
            format!("Qualidade enviada: {}", self.settings.camera_preset.label()),
        ];
        let adjusted = p.controls.keys().filter(|k| *k != CameraControlId::ExposurePriority.key()).count();
        if adjusted > 0 {
            summary.push(format!("{adjusted} ajuste(s) de imagem"));
        }
        if p.smooth {
            summary.push("fluidez priorizada".into());
        }
        forms::note(ui, summary.join(" · "), true);
        ui.add_space(space::XS);
        forms::indented(ui, |ui| {
            if ui_kit::button(ui, Kind::Secondary, Some(ph::SLIDERS_HORIZONTAL), "Configurar câmera…")
                .on_hover_text("Prévia grande, brilho, exposição, foco, balanço de branco, formato, girar e espelhar")
                .clicked()
            {
                self.open_camera_window();
            }
        });
        changed
    }

    /// The "Configurar câmera" window: the preview on top, then the camera's choices and its
    /// controls by group (Imagem / Luz / Lente) in a scroll, the buttons at the bottom.
    pub fn camera_window(&mut self, ctx: &egui::Context) {
        if !self.settings_ui.cam.window {
            return;
        }
        self.camera_window_keep_preview();
        let mut close = false;
        let screen = ctx.content_rect();
        let size = egui::vec2(PREVIEW_W + 36.0, (screen.height() - 24.0).min(860.0));
        let open = forms::Dialog::new("camera_window", "Configurar câmera").icon(ph::VIDEO_CAMERA).size(size).show(ctx, |ui| {
            forms::fit_sliders(ui);
            self.camera_window_preview_ui(ui);
            self.camera_rate_ui(ui);
            let error = self.settings_ui.cam.native_error.clone();
            // The footer (and the driver's error above it) stay under the scroll.
            let footer_h = 58.0 + if error.is_some() { 24.0 } else { 0.0 };
            ui.add_space(space::S);
            egui::ScrollArea::vertical()
                .id_salt("camera_window_body")
                .max_height((ui.available_height() - footer_h).max(40.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width() - 14.0);
                    self.camera_window_body(ui);
                });
            if let Some(e) = error {
                forms::note_colored(ui, format!("{} {e}", ph::WARNING), false, theme::AVISO);
            }
            // (I6: shorter labels in a narrow window, so the footer keeps its three buttons.)
            let narrow = ui.available_width() < 560.0;
            forms::footer(ui, |ui| {
                if ui_kit::button(ui, Kind::Secondary, None, "Fechar").clicked() {
                    close = true;
                }
                let (id, _) = self.camera_ident();
                let synthetic = id == vox_capture::SYNTHETIC_CAMERA_ID;
                let native_open = self.settings_ui.cam.native_open;
                let label = match (native_open, narrow) {
                    (true, false) => "Configurações do fabricante abertas…",
                    (false, false) => "Configurações avançadas do fabricante…",
                    (true, true) => "Fabricante: aberto…",
                    (false, true) => "Ajustes do fabricante…",
                };
                let enabled = !native_open && !synthetic && !self.settings_ui.cameras.is_empty();
                let r = ui.add_enabled_ui(enabled, |ui| ui_kit::button(ui, Kind::Secondary, Some(ph::WRENCH), label)).inner;
                let r = if synthetic {
                    r.on_disabled_hover_text("A câmera de teste não tem configurações do fabricante.")
                } else {
                    r.on_hover_text("Abre a janela de ajustes do próprio driver da câmera (a mesma do OBS). O que você mudar lá aparece aqui quando ela fechar.")
                };
                if r.clicked() {
                    self.open_camera_native_dialog();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let reset = if narrow { "Restaurar" } else { "Restaurar padrão" };
                    if ui_kit::button(ui, Kind::Ghost, Some(ph::ARROW_COUNTER_CLOCKWISE), reset)
                        .on_hover_text("Brilho, exposição, foco e os outros ajustes voltam ao padrão da câmera (automático onde existir). Girar, espelhar e zoom digital não mudam.")
                        .clicked()
                    {
                        self.reset_camera_controls();
                    }
                });
            });
        });
        let down = ctx.input(|i| i.pointer.any_down());
        self.flush_camera_settings(down);
        if !open || close {
            self.close_camera_window();
        }
    }

    /// The big picture (or why there is none).
    fn camera_window_preview_ui(&mut self, ui: &mut egui::Ui) {
        let portrait = matches!(self.camera_profile().rotation, 90 | 270);
        // Up to 640x360, and at most ~36% of a short app window's height (the controls need room).
        // (I6: and never more than what leaves the rate, the controls' scroll and the footer their
        // room: a short window gets a smaller picture, a very short one none. The video is a
        // window of its own, so it never goes in the scroll.)
        let max_h = (ui.ctx().content_rect().height() * 0.36).min(ui.available_height() - 170.0);
        if max_h < 40.0 {
            return;
        }
        let w = ui.available_width().min(max_h * PREVIEW_W / PREVIEW_H).clamp(120.0, PREVIEW_W);
        let h = w * PREVIEW_H / PREVIEW_W;
        let (slot, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let frame = egui::Rect::from_center_size(slot.center(), egui::vec2(w, h));
        let size = if portrait { egui::vec2(h * h / w, h) } else { egui::vec2(w, h) };
        let rect = egui::Rect::from_center_size(frame.center(), size);
        ui.painter().rect_filled(frame, radius::CARD, theme::FUNDO);
        let note = |ui: &egui::Ui, icon: &str, text: &str, color: egui::Color32| {
            let p = ui.painter();
            p.text(frame.center() - egui::vec2(0.0, 14.0), egui::Align2::CENTER_CENTER, icon, theme::body(26.0), color);
            p.text(frame.center() + egui::vec2(0.0, 16.0), egui::Align2::CENTER_CENTER, text, theme::body(size::BODY), color);
        };
        match self.settings_ui.camera_preview.clone() {
            CameraPreviewUi::Failed(e) => note(ui, ph::VIDEO_CAMERA_SLASH, &e, RED),
            CameraPreviewUi::On if self.screen == crate::app::Screen::Main => {
                let key = if self.camera_on() {
                    vox_client_core::voice::SELF_CAMERA_KEY.to_string()
                } else {
                    vox_client_core::golive::camera::PREVIEW_KEY.to_string()
                };
                // The video is a native window placed over this rectangle (see `call.rs`).
                self.video.put(key.clone(), rect.shrink(1.0), ui.painter());
                self.video.want_layer.insert(key, ui.layer_id());
            }
            CameraPreviewUi::On => note(ui, ph::VIDEO_CAMERA, "A prévia aparece depois de entrar no servidor.", theme::NEVOA),
            _ => note(ui, ph::VIDEO_CAMERA, "Abrindo a câmera...", theme::NEVOA),
        }
        if matches!(self.settings_ui.camera_preview, CameraPreviewUi::Failed(_)) {
            ui.add_space(space::S);
            let retry = ui.vertical_centered(|ui| ui_kit::button_sized(ui, Kind::Secondary, Size::Small, Some(ph::ARROWS_CLOCKWISE), "Tentar de novo").clicked()).inner;
            if retry {
                self.set_camera_preview(true);
            }
        }
    }

    /// "Câmera entregando 15 fps (pouca luz)", the one-click fix, and "Priorizar fluidez".
    fn camera_rate_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(space::M);
        let Some(l) = self.settings_ui.cam.live.clone() else {
            forms::note(ui, "Esperando a câmera...", false);
            return;
        };
        let profile = self.camera_profile();
        let smooth = smooth_active(&l.controls, &profile);
        let target = target_fps(&l);
        let method = smooth_method(&l.controls);
        let hover = match method {
            SmoothMethod::Priority => format!(
                "Pede à câmera para manter {} quadros por segundo mesmo com pouca luz: a imagem pode ficar mais escura ou granulada, mas não fica lenta. A exposição continua automática.",
                vox_capture::fps_text(target)
            ),
            SmoothMethod::ManualExposure => {
                let e = l.controls.iter().find(|c| c.id == CameraControlId::Exposure).copied().expect("method checked");
                format!(
                    "Esta câmera não tem esse ajuste: a exposição fica fixa em {} (o máximo para {} fps) e o ganho no automático, se houver.",
                    e.format_value(vox_capture::exposure_for_fps(&e, target)),
                    vox_capture::fps_text(target)
                )
            }
            SmoothMethod::Unsupported => "Esta câmera não deixa ajustar a exposição.".to_string(),
        };
        let whose = if l.from_call { "Câmera (na chamada)" } else { "Câmera" };
        let (dot, text) = if l.fps <= 0.0 {
            (theme::NEVOA, format!("{whose}: medindo quadros por segundo..."))
        } else if l.low_light {
            (theme::AVISO, format!("{whose} entregando {:.0} fps (pouca luz)", l.fps))
        } else if l.fps < target * 0.85 {
            (theme::AVISO, format!("{whose} entregando {:.0} de {} fps", l.fps, vox_capture::fps_text(target)))
        } else {
            (theme::ONLINE, format!("{whose} entregando {:.0} fps", l.fps))
        };
        let check = format!("Priorizar fluidez ({} fps)", vox_capture::fps_text(target));
        // (I6: side by side while the row has room for both, else the checkbox on a line of its own.
        // Both texts are laid out once: measured here, then drawn as they are.)
        let font = egui::TextStyle::Body.resolve(ui.style());
        let text_g = ui.painter().layout_no_wrap(text, font.clone(), theme::TEXTO);
        let check_g = ui.painter().layout_no_wrap(check, font, egui::Color32::PLACEHOLDER);
        let side = ui.available_width() >= text_g.size().x + check_g.size().x + 80.0;
        let smooth_toggle = |ui: &mut egui::Ui| {
            let mut on = smooth;
            let r = ui.add_enabled(method != SmoothMethod::Unsupported, egui::Checkbox::new(&mut on, check_g.clone()));
            let r = if method == SmoothMethod::Unsupported { r.on_disabled_hover_text(hover.as_str()) } else { r.on_hover_text(hover.as_str()) };
            r.changed().then_some(on)
        };
        let mut changed = None;
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 18.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot);
            ui.label(text_g);
            if side {
                changed = ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| smooth_toggle(ui)).inner;
            }
        });
        if !side {
            changed = smooth_toggle(ui);
        }
        if let Some(on) = changed {
            self.set_camera_smooth(on);
        }
        if l.low_light && !smooth && method != SmoothMethod::Unsupported {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Pouca luz: a câmera alonga a exposição e perde quadros. Acenda uma luz ou:").color(theme::AVISO).size(size::SMALL + 0.5));
                if ui_kit::button_sized(ui, Kind::Secondary, Size::Small, None, "Priorizar fluidez").on_hover_text("Mantém os quadros por segundo; com pouca luz a imagem fica mais escura").clicked() {
                    self.set_camera_smooth(true);
                }
            });
        } else if l.low_light {
            forms::note_colored(ui, LOW_LIGHT_TIP, false, theme::AVISO);
        }
    }

    /// Camera, format, quality, the driver's controls by group, and our picture processing.
    fn camera_window_body(&mut self, ui: &mut egui::Ui) {
        let mut restart = false;
        forms::group_icon(ui, ph::VIDEO_CAMERA, "Câmera");
        forms::row(ui, "Câmera", |ui| {
            restart |= self.camera_device_combo(ui, "camera_window_device");
        });
        forms::row(ui, "Formato", |ui| {
            restart |= self.camera_format_combo(ui);
        });
        forms::row(ui, "Qualidade enviada", |ui| {
            let before = self.settings.camera_preset;
            egui::ComboBox::from_id_salt("camera_window_preset").width(forms::combo_w(ui)).selected_text(self.settings.camera_preset.label()).show_ui(ui, |ui| {
                for p in CameraPreset::ALL {
                    ui.selectable_value(&mut self.settings.camera_preset, p, p.label());
                }
            });
            if self.settings.camera_preset != before {
                self.save_settings();
                restart = true;
            }
        });
        if self.camera_on() {
            forms::note(ui, "A câmera está ligada na chamada: câmera, formato e qualidade valem na próxima vez que você ligar.", true);
        }
        let real_camera = self.camera_ident().0 != vox_capture::SYNTHETIC_CAMERA_ID;
        if self.settings_ui.cam.formats.len() == 1 && real_camera {
            forms::note_colored(
                ui,
                "O Windows só oferece 1 formato desta câmera, já convertido por ele. Isso acontece quando outro programa está usando a câmera ou quando o Windows a compartilha entre aplicativos (veja Configurações do Windows > Bluetooth e dispositivos > Câmeras).",
                true,
                theme::AVISO,
            );
        }
        if restart && !self.camera_on() {
            // The preview follows the new camera / format / quality.
            self.set_camera_preview(true);
            self.request_camera_formats();
        } else if restart {
            self.request_camera_formats();
        }

        // The driver's controls: only the ones this camera has, with their real ranges.
        match self.settings_ui.cam.live.clone() {
            Some(l) if l.controls.iter().any(|c| c.id != CameraControlId::ExposurePriority) => self.camera_controls_ui(ui, &l.controls),
            Some(_) => {
                forms::divider(ui);
                forms::note(ui, "Esta câmera não tem ajustes de imagem.", false);
            }
            None => {
                forms::divider(ui);
                forms::note(ui, "Os ajustes aparecem quando a câmera abrir.", false);
            }
        }

        // Our own picture processing (GPU, free).
        forms::divider(ui);
        forms::group_icon(ui, ph::ARROWS_CLOCKWISE, "Girar e espelhar");
        let mut p = self.camera_profile();
        let mut edited = false;
        forms::row(ui, "Espelhar", |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().interact_size.y = forms::ROW_H;
                ui.spacing_mut().item_spacing.y = 0.0;
                edited |= ui.checkbox(&mut p.mirror_preview, "Espelhar minha prévia").on_hover_text("Você se vê como num espelho. Só aparece para você: os outros veem a imagem normal.").changed();
                edited |= ui.checkbox(&mut p.mirror_others, "Espelhar para os outros").on_hover_text("Os outros também veem a imagem espelhada (útil para mostrar texto escrito à mão).").changed();
            });
        });
        forms::row(ui, "Girar", |ui| {
            edited |= forms::segmented(ui, &mut p.rotation, &ROTATIONS);
        });
        forms::row(ui, "Zoom digital", |ui| {
            let mut zoom = p.zoom_pct as f32 / 100.0;
            let r = ui.add(egui::Slider::new(&mut zoom, 1.0..=2.0).step_by(0.05).custom_formatter(|v, _| format!("{v:.2}x").replace('.', ",")).custom_parser(|s| s.trim_end_matches('x').replace(',', ".").trim().parse::<f64>().ok()));
            if r.changed() {
                p.zoom_pct = (zoom * 100.0).round() as u32;
                edited = true;
            }
            r.on_hover_text("Recorte no centro da imagem (1,0x a 2,0x), feito pela placa de vídeo");
        });
        if self.settings_ui.cam.live.as_ref().is_some_and(|l| !l.transform_ok) {
            forms::note_colored(ui, "Esta placa de vídeo não consegue espelhar nem girar: só o zoom funciona.", true, theme::AVISO);
        }
        if edited {
            self.edit_camera_profile(|cur| {
                cur.mirror_preview = p.mirror_preview;
                cur.mirror_others = p.mirror_others;
                cur.rotation = p.rotation;
                cur.zoom_pct = p.zoom_pct;
            });
        }
    }

    /// "Formato": Automático, or one of the camera's own modes. Returns whether it changed.
    fn camera_format_combo(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let chosen = self.camera_profile().mode.as_ref().and_then(SavedMode::mode);
        let mut menu = vox_capture::camera_mode_menu(&self.settings_ui.cam.formats);
        if let Some(c) = chosen.filter(|c| !menu.iter().any(|m| m.matches(c))) {
            // A saved mode this menu leaves out (a slower rate of a listed size) stays pickable.
            menu.insert(0, c);
        }
        let text = chosen.map_or("Automático".to_string(), |m| m.label());
        egui::ComboBox::from_id_salt("camera_window_format").width(forms::combo_w(ui)).selected_text(text).show_ui(ui, |ui| {
            if ui.selectable_label(chosen.is_none(), "Automático").on_hover_text("O formato da câmera mais próximo da qualidade enviada").clicked() && chosen.is_some() {
                self.edit_camera_profile(|p| p.mode = None);
                changed = true;
            }
            for m in menu {
                let selected = chosen.is_some_and(|c| c.matches(&m));
                if ui.selectable_label(selected, m.label()).clicked() && !selected {
                    self.edit_camera_profile(|p| p.mode = Some(SavedMode::of(&m)));
                    changed = true;
                }
            }
        });
        changed
    }

    /// Sliders for the supported controls, by group (Imagem / Luz / Lente): the label in the
    /// label column, the slider, then "Auto" where the camera has it.
    fn camera_controls_ui(&mut self, ui: &mut egui::Ui, controls: &[CameraControl]) {
        let profile = self.camera_profile();
        let mut edits: Vec<CameraAdjust> = Vec::new();
        for (group, list) in group_controls(controls) {
            forms::divider(ui);
            forms::group_icon(ui, group.icon(), group.title());
            for c in &list {
                let (mut value, mut auto) = shown_setting(c, profile.controls.get(c.id.key()).copied());
                let mut edited = false;
                let range = format!("Faixa da câmera: {} a {}, passo {}, padrão {}", c.format_value(c.min), c.format_value(c.max), c.step, c.format_value(c.default));
                forms::row(ui, c.id.label_pt(), |ui| {
                    if c.is_toggle() {
                        let mut on = value > c.min;
                        if ui.checkbox(&mut on, c.format_value(value)).on_hover_text(&range).changed() {
                            value = if on { c.max } else { c.min };
                            edited = true;
                        }
                    } else {
                        // (I6: the slider leaves its number and "Auto" their room in a narrow window.)
                        let auto_w = if c.can_auto { 64.0 } else { 0.0 };
                        ui.spacing_mut().slider_width = ui.spacing().slider_width.min(ui.available_width() - 72.0 - auto_w).max(60.0);
                        let cc = *c;
                        let r = ui.add_enabled(
                            c.can_manual && !auto,
                            egui::Slider::new(&mut value, c.min..=c.max).step_by(c.step.max(1) as f64).custom_formatter(move |v, _| cc.format_value(v.round() as i32)),
                        );
                        if r.changed() {
                            edited = true;
                        }
                        if auto {
                            r.on_disabled_hover_text("Automático: a câmera decide (o valor mostrado é o atual)");
                        } else {
                            r.on_hover_text(&range);
                        }
                    }
                    if c.can_auto {
                        edited |= ui.checkbox(&mut auto, "Auto").changed();
                    }
                });
                if edited {
                    edits.push(CameraAdjust { id: c.id, value: c.snap(value), auto });
                }
            }
        }
        for a in edits {
            self.edit_camera_profile(|p| {
                p.set_control(a.id, a.value, a.auto);
                // Exposure back to automatic undoes a manual "Priorizar fluidez".
                if a.id == CameraControlId::Exposure && a.auto {
                    p.smooth = false;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK_A: &str = r"\\?\usb#vid_046d&pid_082d&mi_00#8&1a2b3c4d&0&0000#{e5323777}\global";
    const LINK_A2: &str = r"\\?\usb#vid_046d&pid_082d&mi_00#7&2b3c4d5e&0&0000#{e5323777}\global";
    const LINK_B: &str = r"\\?\usb#vid_0c45&pid_6366&mi_00#6&aaaa&0&0000#{e5323777}\global";

    #[test]
    fn profiles_are_kept_per_camera() {
        let mut ps = CameraProfiles::default();
        ps.edit(LINK_A, "HD Pro Webcam C920").set_control(CameraControlId::Brightness, 140, false);
        let b = ps.edit(LINK_B, "Integrated Webcam");
        b.set_control(CameraControlId::Brightness, 90, false);
        b.rotation = 180;
        assert_eq!(ps.0.len(), 2);
        assert_eq!(ps.get(LINK_A, "HD Pro Webcam C920").unwrap().controls["brightness"].value, 140);
        assert_eq!(ps.get(LINK_B, "Integrated Webcam").unwrap().controls["brightness"].value, 90);
        assert_eq!(ps.get(LINK_A, "HD Pro Webcam C920").unwrap().rotation, 0);
        // A camera with no profile: nothing.
        assert!(ps.get(r"\\?\usb#other", "Other cam").is_none());
    }

    #[test]
    fn the_same_camera_on_another_port_is_found_by_name_and_moves() {
        let mut ps = CameraProfiles::default();
        ps.edit(LINK_A, "HD Pro Webcam C920").zoom_pct = 150;
        // Lookups by the new link fall back to the name...
        assert_eq!(ps.get(LINK_A2, "hd pro webcam c920").unwrap().zoom_pct, 150);
        // ...and an edit re-keys the entry (no duplicate).
        ps.edit(LINK_A2, "HD Pro Webcam C920").mirror_others = true;
        assert_eq!(ps.0.len(), 1);
        let p = &ps.0[LINK_A2];
        assert_eq!((p.zoom_pct, p.mirror_others), (150, true));
        // Two different cameras are not mixed up by name.
        ps.edit(LINK_B, "Integrated Webcam").rotation = 90;
        assert_eq!(ps.get(LINK_A2, "HD Pro Webcam C920").unwrap().rotation, 0);
        // Without a link the name is the key.
        ps.edit("", "Name only").rotation = 270;
        assert_eq!(ps.get("", "Name only").unwrap().rotation, 270);
    }

    #[test]
    fn default_profiles_are_not_stored() {
        let mut ps = CameraProfiles::default();
        ps.edit(LINK_A, "C920").set_control(CameraControlId::Gain, 10, false);
        ps.tidy(LINK_A, "C920");
        assert_eq!(ps.0.len(), 1);
        ps.edit(LINK_A, "C920").controls.clear();
        ps.tidy(LINK_A, "C920");
        assert!(ps.0.is_empty(), "back to the defaults leaves no entry");
        assert!(CameraProfile::default().is_default());
        assert!(CameraProfile { name: "x".into(), ..Default::default() }.is_default());
        assert!(!CameraProfile { mirror_preview: false, ..Default::default() }.is_default());
    }

    #[test]
    fn profile_converts_to_the_camera_setup() {
        let mut p = CameraProfile::default();
        let s = p.to_setup();
        assert!(s.mode.is_none() && s.controls.is_empty());
        assert!(s.transform.mirror_preview && !s.transform.mirror_others, "preview mirrored, others not");
        p.mode = Some(SavedMode { width: 1280, height: 720, fps_milli: 30_000, format: "MJPG".into() });
        p.set_control(CameraControlId::Exposure, -5, false);
        p.set_control(CameraControlId::Focus, 0, true);
        p.rotation = 90;
        p.zoom_pct = 500;
        p.controls.insert("nonsense".into(), SavedControl { value: 1, auto: false });
        let s = p.to_setup();
        assert_eq!(s.mode.unwrap().label(), "1280×720 · 30 fps (MJPEG)");
        assert_eq!(s.transform.rotation, CameraRotation::Cw90);
        assert_eq!(s.transform.zoom_pct, 200, "clamped");
        assert_eq!(s.controls.len(), 2, "unknown keys are ignored");
        assert!(s.controls.contains(&CameraAdjust { id: CameraControlId::Focus, value: 0, auto: true }));
        // A format this version does not know is ignored, not fatal.
        p.mode = Some(SavedMode { format: "H264".into(), ..p.mode.clone().unwrap() });
        assert!(p.to_setup().mode.is_none());
    }

    #[test]
    fn profiles_persist_in_settings_json_per_device() {
        let mut s = crate::settings::Settings::default();
        s.camera_profiles.edit(LINK_A, "HD Pro Webcam C920").set_control(CameraControlId::WhiteBalance, 4500, false);
        s.camera_profiles.edit(LINK_B, "Integrated Webcam").mirror_preview = false;
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: crate::settings::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.camera_profiles, s.camera_profiles);
        assert_eq!(back.camera_profiles.get(LINK_A, "").unwrap().controls["white_balance"], SavedControl { value: 4500, auto: false });
        assert!(!back.camera_profiles.get(LINK_B, "").unwrap().mirror_preview);
        // Older settings.json files (no profiles) and unknown future fields still load.
        let old: crate::settings::Settings = serde_json::from_str(r#"{"camera_preset":"P540"}"#).unwrap();
        assert!(old.camera_profiles.0.is_empty());
        let newer: CameraProfile = serde_json::from_str(r#"{"rotation":90,"future_field":1}"#).unwrap();
        assert_eq!((newer.rotation, newer.mirror_preview, newer.zoom_pct), (90, true, 100));
    }

    /// What a C920 reports (its log: brightness 0..255, white balance 2000..6500 auto...).
    fn c920() -> Vec<CameraControl> {
        let mk = |id, min, max, step, default, value, auto: bool, can_auto: bool| CameraControl { id, min, max, step, default, value, auto, can_auto, can_manual: true };
        use CameraControlId::*;
        vec![
            mk(Brightness, 0, 255, 1, 128, 128, false, false),
            mk(Contrast, 0, 255, 1, 128, 127, false, false),
            mk(Saturation, 0, 255, 1, 128, 79, false, false),
            mk(Sharpness, 0, 255, 1, 128, 145, false, false),
            mk(WhiteBalance, 2000, 6500, 1, 4000, 3066, true, true),
            mk(Backlight, 0, 1, 1, 0, 0, false, false),
            mk(Exposure, -11, -2, 1, -5, -4, true, true),
            mk(Gain, 0, 255, 1, 0, 255, false, false),
            mk(Focus, 0, 250, 5, 0, 0, true, true),
            mk(Zoom, 100, 500, 1, 100, 100, false, false),
            mk(ExposurePriority, 0, 1, 1, 1, 1, false, false),
        ]
    }

    #[test]
    fn controls_are_grouped_as_imagem_luz_lente() {
        let g = group_controls(&c920());
        let names: Vec<(&str, Vec<&str>)> = g.iter().map(|(g, v)| (g.title(), v.iter().map(|c| c.id.key()).collect())).collect();
        assert_eq!(
            names,
            vec![
                ("Imagem", vec!["brightness", "contrast", "saturation", "sharpness"]),
                ("Luz", vec!["exposure", "gain", "backlight", "white_balance"]),
                ("Lente", vec!["focus", "zoom"]),
            ]
        );
        // The priority is the "Priorizar fluidez" switch, never a slider.
        assert!(g.iter().all(|(_, v)| v.iter().all(|c| c.id != CameraControlId::ExposurePriority)));
        // Every slider control belongs to exactly one section.
        for id in CameraControlId::ALL.into_iter().filter(|i| *i != CameraControlId::ExposurePriority) {
            assert_eq!(ControlGroup::ALL.iter().filter(|g| g.members().contains(&id)).count(), 1, "{id:?}");
        }
        // A camera with only image controls: just "Imagem"; none at all: nothing.
        let only: Vec<CameraControl> = c920().into_iter().filter(|c| c.id == CameraControlId::Brightness).collect();
        assert_eq!(group_controls(&only).len(), 1);
        assert!(group_controls(&[]).is_empty());
    }

    #[test]
    fn auto_shows_the_cameras_value_and_manual_the_saved_one() {
        let cs = c920();
        let get = |id| *cs.iter().find(|c| c.id == id).unwrap();
        let wb = get(CameraControlId::WhiteBalance);
        // Nothing saved: what the camera says (auto, its current 3066 K).
        assert_eq!(shown_setting(&wb, None), (3066, true));
        // Saved manual 4500 K: shown as saved even before the camera reports it.
        assert_eq!(shown_setting(&wb, Some(SavedControl { value: 4500, auto: false })), (4500, false));
        // Saved auto: the camera's own value moves with the light.
        assert_eq!(shown_setting(&wb, Some(SavedControl { value: 4500, auto: true })), (3066, true));
        // No automatic mode: "Auto" is never ticked, even if a file says so.
        let b = get(CameraControlId::Brightness);
        assert_eq!(shown_setting(&b, Some(SavedControl { value: 90, auto: true })), (90, false));
        assert_eq!(shown_setting(&b, Some(SavedControl { value: 300, auto: false })), (255, false), "snapped into the range");
        let f = get(CameraControlId::Focus);
        assert_eq!(shown_setting(&f, Some(SavedControl { value: 123, auto: false })), (125, false), "on the 5-step grid");
    }

    #[test]
    fn priorizar_fluidez_uses_the_priority_control_when_there_is_one() {
        let cs = c920();
        assert_eq!(smooth_method(&cs), SmoothMethod::Priority);
        let on = smooth_plan(&cs, 30.0, true);
        assert_eq!(on, vec![CameraAdjust { id: CameraControlId::ExposurePriority, value: 0, auto: false }]);
        let off = smooth_plan(&cs, 30.0, false);
        assert_eq!(off, vec![CameraAdjust { id: CameraControlId::ExposurePriority, value: 1, auto: false }]);
        // The switch follows the profile (and the camera when nothing is saved).
        let mut p = CameraProfile::default();
        assert!(!smooth_active(&cs, &p), "the C920 lets exposure lower the rate by default");
        for a in &on {
            p.set_control(a.id, a.value, a.auto);
        }
        p.smooth = true;
        assert!(smooth_active(&cs, &p));
        // Exposure stays automatic: only the priority changes.
        assert!(!p.controls.contains_key("exposure"));
    }

    #[test]
    fn priorizar_fluidez_without_the_priority_control_fixes_the_exposure() {
        let mut cs: Vec<CameraControl> = c920().into_iter().filter(|c| c.id != CameraControlId::ExposurePriority).collect();
        assert_eq!(smooth_method(&cs), SmoothMethod::ManualExposure);
        // 30 fps: 1/32 s at most; the C920's gain has no auto, so it is left alone.
        assert_eq!(smooth_plan(&cs, 30.0, true), vec![CameraAdjust { id: CameraControlId::Exposure, value: -5, auto: false }]);
        assert_eq!(smooth_plan(&cs, 60.0, true)[0].value, -6);
        // A camera whose gain has auto gets it.
        cs.iter_mut().find(|c| c.id == CameraControlId::Gain).unwrap().can_auto = true;
        let on = smooth_plan(&cs, 30.0, true);
        assert_eq!(on.len(), 2);
        assert_eq!((on[1].id, on[1].auto), (CameraControlId::Gain, true));
        // Off: exposure back to automatic.
        assert_eq!(smooth_plan(&cs, 30.0, false), vec![CameraAdjust { id: CameraControlId::Exposure, value: -4, auto: true }]);
        // On = the flag and a manual exposure; exposure set back to auto by hand turns it off.
        let mut p = CameraProfile::default();
        for a in &on {
            p.set_control(a.id, a.value, a.auto);
        }
        p.smooth = true;
        assert!(smooth_active(&cs, &p));
        p.set_control(CameraControlId::Exposure, -4, true);
        assert!(!smooth_active(&cs, &p));
        // Nothing to adjust: unsupported, no plan, never on.
        let none: Vec<CameraControl> = cs.into_iter().filter(|c| c.id != CameraControlId::Exposure).collect();
        assert_eq!(smooth_method(&none), SmoothMethod::Unsupported);
        assert!(smooth_plan(&none, 30.0, true).is_empty());
        assert!(!smooth_active(&none, &CameraProfile { smooth: true, ..Default::default() }));
    }

    #[test]
    fn the_driver_dialogs_changes_are_found() {
        let before = c920();
        let mut after = before.clone();
        let set = |v: &mut Vec<CameraControl>, id, value, auto| {
            let c = v.iter_mut().find(|c| c.id == id).unwrap();
            c.value = value;
            c.auto = auto;
        };
        // Automatic exposure moving by itself is not a change.
        set(&mut after, CameraControlId::Exposure, -6, true);
        assert!(native_changes(&before, &after).is_empty());
        // Brightness moved, white balance went manual, focus stayed auto.
        set(&mut after, CameraControlId::Brightness, 150, false);
        set(&mut after, CameraControlId::WhiteBalance, 5000, false);
        let ch = native_changes(&before, &after);
        assert_eq!(
            ch,
            vec![
                CameraAdjust { id: CameraControlId::Brightness, value: 150, auto: false },
                CameraAdjust { id: CameraControlId::WhiteBalance, value: 5000, auto: false },
            ]
        );
        // Exposure switched to automatic in the dialog.
        let mut manual = before.clone();
        set(&mut manual, CameraControlId::Exposure, -5, false);
        assert_eq!(native_changes(&manual, &before), vec![CameraAdjust { id: CameraControlId::Exposure, value: -4, auto: true }]);
    }

    #[test]
    fn priorizar_fluidez_persists_and_old_files_load() {
        let mut s = crate::settings::Settings::default();
        let p = s.camera_profiles.edit(LINK_A, "HD Pro Webcam C920");
        p.smooth = true;
        p.set_control(CameraControlId::ExposurePriority, 0, false);
        assert!(!p.is_default());
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"smooth\":true") && json.contains("exposure_priority"));
        let back: crate::settings::Settings = serde_json::from_str(&json).unwrap();
        let p = back.camera_profiles.get(LINK_A, "").unwrap();
        assert!(p.smooth);
        assert_eq!(p.controls["exposure_priority"], SavedControl { value: 0, auto: false });
        assert_eq!(p.to_setup().controls, vec![CameraAdjust { id: CameraControlId::ExposurePriority, value: 0, auto: false }]);
        // A v0.6-v0.8 profile (no "smooth") loads with it off.
        let old: CameraProfile = serde_json::from_str(r#"{"name":"C920","controls":{"brightness":{"value":140,"auto":false}},"mirror_preview":true,"mirror_others":false,"rotation":0,"zoom_pct":100}"#).unwrap();
        assert!(!old.smooth);
        assert_eq!(old.controls["brightness"].value, 140);
        // "smooth" alone is not the default (it is kept).
        assert!(!CameraProfile { smooth: true, ..Default::default() }.is_default());
    }
}
