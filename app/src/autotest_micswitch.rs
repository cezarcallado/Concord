//! Changing the microphone mid-call (a child module of `autotest`), with made-up devices: nothing
//! is opened or recorded (`VOX_NO_MIC=1` stays on), the audio is synthetic and only measured, and
//! nothing is played (`VOX_NO_PLAYBACK=1`). The capture chain and the room are the real ones.
//!
//! - `micswitch-a`: joins with no microphone at all (the warning, listen-only), plugs them in and
//!   is published without rejoining; then goes through [`PHASES`]: another device in another
//!   format from the settings' path, the system default moving, the chosen device unplugged with
//!   and without an error from the driver, one that refuses to open, all of them gone. After each
//!   change it checks what is being captured (one device, the right one) and what the person was
//!   told, then raises a flag in `--mode <dir>` and waits for B.
//! - `micswitch-b`: at each flag, measures A's voice for a few seconds and answers.
//!
//! Nobody leaves the room between phases.

use std::path::PathBuf;
use std::time::Instant;

use vox_client_core::audio::fake_mic::{self, FakeFormat, FakeMic, FakeMics};

use super::Auto;
use crate::app::App;

const M48: &str = "Fake 48 kHz mono";
const S44: &str = "Fake 44,1 kHz estereo";
const H16: &str = "Fake headset 16 kHz";
const Q96: &str = "Fake 96 kHz quatro canais";
const BROKEN: &str = "Fake quebrado";

const CHANGE_LIMIT_SECS: u64 = 25;
const LISTEN_LIMIT_SECS: u64 = 40;
/// Time for the new device's audio to reach B before it starts counting.
const SETTLE_SECS: u64 = 2;

/// Something happens to the microphones.
enum Change {
    /// Picked in the settings (`None`: "Padrão do sistema").
    Pick(Option<&'static str>),
    Default(&'static str),
    /// Unplugged, and the driver says so.
    Unplug(&'static str),
    /// Unplugged, and the stream just stops calling back.
    UnplugSilently(&'static str),
    Replug(&'static str),
}

struct Phase {
    what: &'static str,
    change: Change,
    /// What must be capturing after it.
    capturing: &'static str,
    /// What the notice must say (`""`: none is needed).
    notice: &'static str,
}

const PHASES: &[Phase] = &[
    Phase { what: "pick 44.1 kHz stereo i16", change: Change::Pick(Some(S44)), capturing: S44, notice: "" },
    Phase { what: "pick a 16 kHz headset", change: Change::Pick(Some(H16)), capturing: H16, notice: "" },
    Phase { what: "pick 96 kHz 4 channels", change: Change::Pick(Some(Q96)), capturing: Q96, notice: "" },
    Phase { what: "back to the system default", change: Change::Pick(None), capturing: M48, notice: "" },
    Phase { what: "the system default moves", change: Change::Default(S44), capturing: S44, notice: "" },
    Phase { what: "pick the headset again", change: Change::Pick(Some(H16)), capturing: H16, notice: "" },
    Phase { what: "the headset unplugged, driver says so", change: Change::Unplug(H16), capturing: S44, notice: "indisponível" },
    Phase { what: "the headset back", change: Change::Replug(H16), capturing: H16, notice: "de volta" },
    Phase { what: "the headset unplugged, not a word", change: Change::UnplugSilently(H16), capturing: S44, notice: "indisponível" },
    Phase { what: "the headset back again", change: Change::Replug(H16), capturing: H16, notice: "de volta" },
    Phase { what: "pick one that will not open", change: Change::Pick(Some(BROKEN)), capturing: S44, notice: "indisponível" },
    Phase { what: "back to the default, by hand", change: Change::Pick(None), capturing: S44, notice: "" },
    Phase { what: "the default unplugged: the next one", change: Change::Unplug(S44), capturing: M48, notice: "" },
];

#[derive(Default)]
pub struct State {
    bank: Option<FakeMics>,
    phase: usize,
    peer: Option<String>,
    chunks: u64,
    at: Option<Instant>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST micswitch: {what}");
}

impl Auto {
    fn sw_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn sw_flag(&self, name: &str) -> bool {
        self.sw_dir().join(name).exists()
    }

    fn sw_set(&self, name: &str) {
        if let Err(e) = std::fs::write(self.sw_dir().join(name), "ok") {
            tracing::warn!(error = %e, file = name, "AUTOTEST micswitch: cannot write a flag file");
        }
    }

    fn sw_apply(&self, app: &mut App, bank: &FakeMics, change: &Change) {
        match *change {
            Change::Pick(device) => {
                // What the settings window does when the combo box changes.
                app.settings.input_device = device.map(str::to_string);
                app.apply_input_device();
            }
            Change::Default(name) => bank.set_default(Some(name)),
            Change::Unplug(name) => {
                bank.set_listed(name, false);
                // (Only a default that really went away moves: Windows names another one.)
                if name == S44 {
                    bank.set_default(Some(M48));
                }
                bank.report_lost(name);
            }
            Change::UnplugSilently(name) => {
                bank.set_listed(name, false);
                bank.set_silent(name, true);
            }
            Change::Replug(name) => {
                bank.set_silent(name, false);
                bank.set_listed(name, true);
            }
        }
    }

    pub(super) fn run_micswitch_a(&mut self, app: &mut App) -> Option<i32> {
        const PHASE_START: u32 = 6;
        match self.step {
            0 => {
                let bank = fake_mic::install(FakeMics::new(vec![
                    FakeMic::new(M48, 48_000, 1, FakeFormat::F32).speaking(),
                    FakeMic::new(S44, 44_100, 2, FakeFormat::I16).speaking(),
                    FakeMic::new(H16, 16_000, 1, FakeFormat::I16).speaking(),
                    FakeMic::new(Q96, 96_000, 4, FakeFormat::F32).speaking(),
                    FakeMic::new(BROKEN, 48_000, 2, FakeFormat::F32).speaking(),
                ]));
                for n in [M48, S44, H16, Q96, BROKEN] {
                    bank.set_listed(n, false);
                }
                bank.set_broken(BROKEN, true);
                bank.set_default(None);
                self.micsw.bank = Some(bank);
                self.connect(app);
            }
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.settings.input_device = None;
                    app.join_voice(vc);
                    self.goto(2, "joining voice with no microphone anywhere");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                if app.voice.channel.is_some() {
                    let warned = app.voice.warning.as_deref().is_some_and(|w| w.starts_with("Entrou sem microfone"));
                    log(&format!("joined: no_mic={} warning={:?}", app.voice.no_mic, app.voice.warning));
                    if !app.voice.no_mic || !warned {
                        return self.fail("a join without any microphone must say so");
                    }
                    self.goto(3, "listen-only; waiting for B");
                } else if let Some(e) = app.voice.error.clone() {
                    return self.fail(&format!("voice join failed: {e}"));
                } else if self.waited(60) {
                    return self.fail("voice join timed out");
                }
            }
            3 => {
                if app.members_here().len() >= 2 && self.waited(1) {
                    let bank = self.micsw.bank.clone()?;
                    for n in [M48, S44, H16, Q96, BROKEN] {
                        bank.set_listed(n, true);
                    }
                    bank.set_default(Some(M48));
                    self.goto(4, "microphones plugged in, without rejoining");
                } else if self.waited(90) {
                    return self.fail("B never joined voice");
                }
            }
            4 => {
                let bank = self.micsw.bank.clone()?;
                if !app.voice.no_mic && bank.open_names() == [M48] {
                    log(&format!("published after the join; warning={:?} notice={:?}", app.voice.warning, app.notice));
                    if app.voice.warning.is_some() {
                        return self.fail("the 'no microphone' warning stayed up");
                    }
                    if app.voice.channel.is_none() {
                        return self.fail("left the room");
                    }
                    self.goto(5, "microphone live; letting it reach B");
                } else if self.waited(CHANGE_LIMIT_SECS) {
                    return self.fail(&format!("the mic plugged in mid-call was not used: capturing {:?}, no_mic {}", bank.open_names(), app.voice.no_mic));
                }
            }
            5 if self.waited(SETTLE_SECS) => {
                self.sw_set("phase-joined");
                self.goto(50, "asked B to listen");
            }
            50 => {
                if self.sw_flag("heard-joined") {
                    self.goto(PHASE_START, "B hears the late microphone");
                } else if self.sw_flag("deaf") {
                    return self.fail("B does not hear the microphone plugged in after the join");
                } else if self.waited(LISTEN_LIMIT_SECS) {
                    return self.fail("B never answered");
                }
            }
            s if s == PHASE_START => {
                let Some(phase) = PHASES.get(self.micsw.phase) else {
                    self.goto(9, "every change done");
                    return None;
                };
                let bank = self.micsw.bank.clone()?;
                app.notice = None;
                self.sw_apply(app, &bank, &phase.change);
                self.goto(7, &format!("phase {}: {}", self.micsw.phase, phase.what));
            }
            7 => {
                let phase = &PHASES[self.micsw.phase];
                let bank = self.micsw.bank.clone()?;
                let open = bank.open_names();
                let captured = open == [phase.capturing];
                let told = phase.notice.is_empty() || app.notice.as_deref().is_some_and(|n| n.contains(phase.notice));
                if captured && told {
                    log(&format!("phase {}: capturing {open:?} after {:.1} s; notice {:?}", self.micsw.phase, self.step_at.elapsed().as_secs_f32(), app.notice));
                    if app.voice.channel.is_none() {
                        return self.fail("the change took us out of the room");
                    }
                    self.goto(8, "letting it reach B");
                } else if self.waited(CHANGE_LIMIT_SECS) {
                    return self.fail(&format!("phase {} ({}): capturing {open:?}, notice {:?}", self.micsw.phase, phase.what, app.notice));
                }
            }
            8 if self.waited(SETTLE_SECS) => {
                self.sw_set(&format!("phase-{}", self.micsw.phase));
                self.goto(80, "asked B to listen");
            }
            80 => {
                if self.sw_flag(&format!("heard-{}", self.micsw.phase)) {
                    self.micsw.phase += 1;
                    self.goto(PHASE_START, "B heard it");
                } else if self.sw_flag("deaf") {
                    return self.fail(&format!("B does not hear phase {}: {}", self.micsw.phase, PHASES[self.micsw.phase].what));
                } else if self.waited(LISTEN_LIMIT_SECS) {
                    return self.fail("B never answered");
                }
            }
            9 => {
                self.sw_set("done");
                log(&format!("all {} changes done; {} streams opened, no rejoin", PHASES.len(), self.micsw.bank.as_ref()?.opens()));
                app.leave_voice();
                self.goto(10, "leaving voice");
            }
            10 if self.waited(2) => {
                if !self.micsw.bank.as_ref()?.open_names().is_empty() {
                    return self.fail("leaving the room did not close the microphone");
                }
                return self.pass();
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_micswitch_b(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                if self.logged_in(app) {
                    let Some(vc) = Self::first_voice(app) else { return self.fail("no voice channel") };
                    app.join_voice(vc);
                    self.goto(2, "joining voice");
                } else if self.waited(30) || app.form.error.is_some() {
                    return self.fail(&format!("could not log in: {}", app.form.error.clone().unwrap_or_default()));
                }
            }
            2 => {
                let me = app.my_id();
                if app.voice.channel.is_some() {
                    if let Some(a) = app.members_here().iter().find(|m| Some(m.user.id) != me).map(|m| m.user.id.to_string()) {
                        self.micsw.peer = Some(a);
                        self.micsw.phase = usize::MAX; // the join's phase
                        self.goto(3, "in the room with A");
                    }
                }
                if self.waited(90) {
                    return self.fail("A never showed up in the room");
                }
            }
            3 => {
                let name = self.sw_phase_name();
                if self.sw_flag("done") {
                    self.goto(6, "A is done");
                } else if self.sw_flag(&format!("phase-{name}")) {
                    self.goto(4, &format!("A's phase {name}: listening"));
                } else if self.waited(90) {
                    return self.fail(&format!("A never reached phase {name}"));
                }
            }
            4 => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let peer = self.micsw.peer.clone()?;
                self.micsw.chunks = Self::ao_audio(app, &format!("voice:{peer}")).map(|a| a.chunks).unwrap_or(0);
                self.micsw.at = Some(Instant::now());
                self.goto(5, "first sample");
            }
            5 if self.waited(3) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let peer = self.micsw.peer.clone()?;
                let name = self.sw_phase_name();
                let info = Self::ao_audio(app, &format!("voice:{peer}")).cloned();
                let secs = self.micsw.at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(3.0);
                let rate = info.as_ref().map_or(0.0, |i| i.chunks.saturating_sub(self.micsw.chunks) as f64 / secs);
                let rms = info.as_ref().and_then(|i| i.rms_dbfs);
                log(&format!("phase {name}: A's voice {rate:.0} blocks/s at {rms:?} dBFS"));
                if rate < 50.0 || !rms.is_some_and(|d| d > -60.0) {
                    self.sw_set("deaf");
                    return self.fail(&format!("phase {name}: A's voice does not arrive ({rate:.0} blocks/s, {rms:?} dBFS)"));
                }
                self.sw_set(&format!("heard-{name}"));
                self.micsw.phase = self.micsw.phase.wrapping_add(1);
                self.goto(3, "answered A");
            }
            6 => {
                if app.voice.channel.is_none() || self.waited(3) {
                    app.leave_voice();
                    return self.pass();
                }
            }
            _ => {}
        }
        None
    }

    /// B's position in A's phases: "joined", then the numbers.
    fn sw_phase_name(&self) -> String {
        match self.micsw.phase {
            usize::MAX => "joined".into(),
            n => n.to_string(),
        }
    }
}
