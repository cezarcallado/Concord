//! concord desktop app (egui + glow). Logic lives in vox-client-core; this crate is the window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alerts;
mod app;
mod avatars;
mod autotest;
mod backend;
mod brand;
mod call;
mod camera_cfg;
mod chat_images;
mod clipboard;
mod context_menu;
mod donate;
mod e2ee_store;
mod filedlg;
mod forms;
mod keybinds;
mod host;
mod layout;
mod links;
mod meter;
mod migrate;
mod platform;
mod previews;
mod profile;
mod repaint_watch;
mod roles;
mod servers;
mod settings;
mod single_instance;
mod soundboard;
mod soundtrim;
mod sources;
mod stream_cards;
mod theme;
mod tray;
mod ui_kit;
#[cfg(target_os = "macos")]
mod video_gl;
mod voice_fx;

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

use eframe::egui;

use crate::autotest::{Auto, AutoArgs, Role};
use crate::backend::{Backend, Repaint};
use crate::settings::Settings;

/// Process exit code (set by `--autotest`).
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

#[derive(Default)]
struct Args {
    autotest: Option<String>,
    server: String,
    name: String,
    password: String,
    hwnd: Option<isize>,
    switch_hwnd: Option<isize>,
    hold: Option<u64>,
    run_id: String,
    show: Option<String>,
    ptt_vk: Option<u32>,
    input_device: Option<String>,
    output_device: Option<String>,
    log_dir: Option<PathBuf>,
    popout: bool,
    watch_secs: Option<u64>,
    preview: bool,
    pv_mode: Option<String>,
    mode: Option<String>,
    audio_pid: Option<u32>,
    switch_pid: Option<u32>,
    audio_high: bool,
    kinds: bool,
    settings_dir: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--autotest" => a.autotest = it.next(),
            "--server" => a.server = it.next().unwrap_or_default(),
            "--name" => a.name = it.next().unwrap_or_default(),
            "--password" => a.password = it.next().unwrap_or_default(),
            "--hwnd" => a.hwnd = it.next().and_then(|v| v.parse().ok()),
            "--switch-hwnd" => a.switch_hwnd = it.next().and_then(|v| v.parse().ok()),
            "--hold" => a.hold = it.next().and_then(|v| v.parse().ok()),
            "--run-id" => a.run_id = it.next().unwrap_or_default(),
            "--show" => a.show = it.next(),
            "--input-device" => a.input_device = it.next(),
            "--output-device" => a.output_device = it.next(),
            "--ptt-vk" => a.ptt_vk = it.next().and_then(|v| v.parse().ok()),
            "--log-dir" => a.log_dir = it.next().map(PathBuf::from),
            "--popout" => a.popout = true,
            "--preview" => a.preview = true,
            "--pv-mode" => a.pv_mode = it.next(),
            "--mode" => a.mode = it.next(),
            "--audio-pid" => a.audio_pid = it.next().and_then(|v| v.parse().ok()),
            "--switch-pid" => a.switch_pid = it.next().and_then(|v| v.parse().ok()),
            "--audio-high" => a.audio_high = true,
            "--kinds" => a.kinds = true,
            "--settings-dir" => a.settings_dir = it.next().map(PathBuf::from),
            "--watch-secs" => a.watch_secs = it.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    a
}

fn main() {
    let args = parse_args();
    if args.autotest.is_some() {
        // Autotests never take the foreground (`vox_client_core::testmode`). Set before any thread
        // starts, and before anything reads it.
        std::env::set_var(vox_client_core::testmode::ENV, "1");
    }
    // v1.1: one concord at a time. A second launch brings the running one to the front and quits
    // (before anything else: no migration, no settings read). Never in a test mode
    // (`single_instance::key`): the tests run many concords at once.
    let test_mode = args.autotest.is_some()
        || vox_client_core::testmode::background()
        || args.settings_dir.is_some()
        || std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some();
    let mut instance = None;
    let mut instance_note = None;
    if let Some(key) = single_instance::key_from_env(test_mode) {
        match single_instance::acquire(&key) {
            single_instance::Start::First(g) => instance = Some(g),
            single_instance::Start::Running { signalled } => {
                let log = vox_log::init("app", args.log_dir.as_deref(), false);
                tracing::info!(signalled, key = %key, "concord is already running: asked it to show its window; this launch quits");
                // (The log is written by its own thread: flushed when the guard goes.)
                drop(log);
                std::process::exit(0);
            }
            single_instance::Start::Unchecked(why) => instance_note = Some(why),
        }
    }
    // v1.0.1: the first start of concord copies what voxhub kept (`migrate.rs`), before anything
    // reads it. Never in an autotest (they must not touch the real profile).
    let migrated = if args.autotest.is_none() { migrate::run_at_startup() } else { None };
    let log = vox_log::init("app", args.log_dir.as_deref(), false);
    if let Some(line) = migrated {
        tracing::info!("{line}");
    }
    if let Some(why) = instance_note {
        tracing::warn!(%why, "single instance: not checked");
    }
    // Configurações → Sobre → "Abrir pasta dos registros".
    app::set_log_dir(args.log_dir.clone().unwrap_or_else(|| vox_log::default_log_dir("app")));

    // winit allows one event loop per process and egui owns it, so the Go Live viewer uses a
    // plain Win32 window instead.
    vox_client_core::golive::Viewer::prefer_native_window();

    let auto = args.autotest.as_deref().map(|r| {
        let role = match r {
            "a" | "A" => Role::A,
            "b" | "B" => Role::B,
            "voice" => Role::Voice,
            "mictest" => Role::MicTest,
            "cam-a" => Role::CamA,
            "cam-b" => Role::CamB,
            "multi-a" => Role::MultiA,
            "multi-b" => Role::MultiB,
            "audio-a" => Role::AudioA,
            "audio-b" => Role::AudioB,
            "audio-c" => Role::AudioC,
            "mod-a" => Role::ModA,
            "mod-b" => Role::ModB,
            "avatar-a" => Role::AvatarA,
            "avatar-b" => Role::AvatarB,
            "profile-a" => Role::ProfileA,
            "profile-b" => Role::ProfileB,
            "micswitch-a" => Role::MicSwitchA,
            "micswitch-b" => Role::MicSwitchB,
            "sound-a" => Role::SoundA,
            "sound-b" => Role::SoundB,
            "sound-trim" => Role::SoundTrim,
            "dm-a" => Role::DmA,
            "dm-b" => Role::DmB,
            "menu-a" => Role::MenuA,
            "menu-b" => Role::MenuB,
            "img-a" => Role::ImgA,
            "img-b" => Role::ImgB,
            "notify-a" => Role::NotifyA,
            "notify-b" => Role::NotifyB,
            "notify-c" => Role::NotifyC,
            "chatx-a" => Role::ChatxA,
            "chatx-b" => Role::ChatxB,
            "fix-a" => Role::FixA,
            "fix-b" => Role::FixB,
            "priv-a" => Role::PrivA,
            "priv-b" => Role::PrivB,
            "priv-c" => Role::PrivC,
            "host-a" => Role::HostA,
            "host-b" => Role::HostB,
            "host-c" => Role::HostC,
            "camwin" => Role::CamWin,
            "srvlist" => Role::SrvList,
            "kit" => Role::Kit,
            "shots" => Role::Shots,
            "shots3" => Role::Shots3,
            "nav-a" => Role::NavA,
            "nav-b" => Role::NavB,
            "stage-b" => Role::StageB,
            "shots3b-a" => Role::Shots3bA,
            "shots3b-b" => Role::Shots3bB,
            "shots3b-host" => Role::Shots3bHost,
            "calladex" => Role::Calladex,
            "mac-view" => Role::MacView,
            "uix-a" => Role::UixA,
            "uix-b" => Role::UixB,
            "keys-a" => Role::KeysA,
            "thumbs-a" => Role::ThumbsA,
            "thumbs-b" => Role::ThumbsB,
            "thumbs-c" => Role::ThumbsC,
            "tls-a" => Role::TlsA,
            "tls-b" => Role::TlsB,
            "tls-mitm" => Role::TlsMitm,
            "tls-oldhub" => Role::TlsOldHub,
            "e2ee-a" => Role::E2eeA,
            "e2ee-a2" => Role::E2eeA2,
            "e2ee-b" => Role::E2eeB,
            "e2dm-a" => Role::E2dmA,
            "e2dm-b" => Role::E2dmB,
            "e2dm-a2" => Role::E2dmA2,
            "e2dm-new" => Role::E2dmNew,
            "e2v-a" => Role::E2vA,
            "e2v-b" => Role::E2vB,
            "e2v-c" => Role::E2vC,
            "e2v-admin" => Role::E2vAdmin,
            "emoji-a" => Role::EmojiA,
            "resp-a" => Role::RespA,
            "resp-b" => Role::RespB,
            #[cfg(feature = "donate-test")]
            "donate" => Role::Donate,
            _ => Role::Idle,
        };
        tracing::info!(role = r, own_settings_dir = args.settings_dir.is_some(), "AUTOTEST mode (the real settings are not read or written)");
        Auto::new(
            role,
            AutoArgs {
                server: args.server.clone(),
                name: args.name.clone(),
                password: args.password.clone(),
                hwnd: args.hwnd,
                switch_hwnd: args.switch_hwnd,
                hold_secs: args.hold.unwrap_or(25),
                run_id: args.run_id.clone(),
                show: args.show.clone(),
                popout: args.popout,
                watch_secs: args.watch_secs.unwrap_or(9),
                preview: args.preview,
                pv_mode: args.pv_mode.clone(),
                mode: args.mode.clone(),
                audio_pid: args.audio_pid,
                switch_pid: args.switch_pid,
                audio_high: args.audio_high,
                kinds: args.kinds,
            },
        )
    });
    let persist = auto.is_none();
    if let Some(dir) = args.settings_dir.clone().filter(|_| !persist) {
        // Autotest only: settings (and the saved server list) read from and written to this folder,
        // so a run can restart the app and find them (v0.9.1). The real ones are never touched.
        crate::settings::set_dir(dir);
    }
    let mut settings = if persist || crate::settings::dir_overridden() { Settings::load() } else { Settings::default() };
    // v1.1 (E0): the identity keys live in e2ee.json next to settings.json (memory only otherwise).
    crate::e2ee_store::set_persistent(persist || crate::settings::dir_overridden());
    // macOS: saved passwords live in secrets.json (moved out of the Keychain once); drop the ones
    // no settings file mentions.
    #[cfg(target_os = "macos")]
    if persist || crate::settings::dir_overridden() {
        crate::settings::secrets_gc();
    }
    if !persist {
        // Autotest: alert sounds only run when VOX_NO_PLAYBACK is set (then they are just logged),
        // so an unattended test can never make a noise.
        settings.alerts.enabled = std::env::var("VOX_NO_PLAYBACK").is_ok_and(|v| !v.is_empty() && v != "0");
        settings.alerts.presence = true;
        if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some() {
            // Design-review screenshots: never "Ausente" just because the PC happens to be idle.
            settings.profile.idle_away_minutes = 0;
        }
    }
    if let Some(vk) = args.ptt_vk.filter(|_| !persist) {
        // Autotest only: push-to-talk on this key without opening the settings dialog.
        settings.ptt_enabled = true;
        settings.ptt_vk = vk;
        settings.ptt_name = format!("VK {vk}");
    }

    if let Some(dev) = args.input_device.clone().filter(|_| !persist) {
        // Autotest only: pick the microphone (a name that does not exist exercises the
        // "joined without a microphone" path).
        settings.input_device = Some(dev);
    }

    if let Some(dev) = args.output_device.clone().filter(|_| !persist) {
        // Autotest only: pick the speakers (tests use an inaudible virtual device).
        settings.output_device = Some(dev);
    }

    if args.popout && !persist {
        // Autotest only: the "Abrir transmissões em janela separada" setting.
        settings.stream_popout = true;
    }
    // Watched streams show inside the call view unless the user prefers separate windows.
    vox_client_core::golive::Viewer::set_open_inline(!settings.stream_popout);

    let repaint = Repaint::default();
    {
        // The viewer thread wakes the UI when a stream changes place by itself (pop-out closed).
        let r = repaint.clone();
        // macOS: this is every new video frame (the app draws the video): one repaint each, not
        // the two a plain request asks egui for.
        #[cfg(target_os = "macos")]
        vox_client_core::golive::Viewer::set_waker(move || r.wake_once());
        #[cfg(not(target_os = "macos"))]
        vox_client_core::golive::Viewer::set_waker(move || r.wake());
    }
    if let Some(mut g) = instance {
        // A second launch's signal shows the window (`notify_tick`), even from the tray. Held
        // until the app starts closing (`notify_on_exit`).
        let r = repaint.clone();
        g.listen(move || r.wake());
        single_instance::hold(g);
        tracing::info!("single instance: this is the running concord");
    }
    let backend = Backend::start(repaint.clone());

    // (1060 × 660, or the design-review screenshots' size and display scale.)
    let (inner, zoom) = autotest::shots::window();
    let mut viewport = egui::ViewportBuilder::default();
    if let Some(icon) = brand::window_icon() {
        // The window and taskbar icon (eframe's own egui logo otherwise).
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport: viewport
            .with_title(brand::APP_NAME)
            .with_inner_size(inner)
            .with_min_inner_size([760.0, 440.0])
            // Background test mode: shown without taking the foreground (then kept behind, `app.rs`).
            .with_active(!vox_client_core::testmode::background())
            .with_visible(true),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    let rp = repaint.clone();
    let result = eframe::run_native(
        "concord",
        options,
        Box::new(move |cc| {
            rp.set(cc.egui_ctx.clone());
            // v1.0: the night-room theme, its fonts and the icon font (`theme.rs`).
            theme::apply(&cc.egui_ctx);
            if zoom != 1.0 {
                cc.egui_ctx.set_zoom_factor(zoom);
            }
            Ok(Box::new(app::App::new(backend, repaint, settings, persist, auto)))
        }),
    );
    if let Err(e) = result {
        tracing::error!(error = %e, "ui failed");
    }
    tracing::info!("concord exited");
    single_instance::release();
    drop(log);
    std::process::exit(EXIT_CODE.load(Ordering::Relaxed));
}
