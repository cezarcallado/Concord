//! Which microphone a running session captures, and what happens when that has to change: the
//! person picks another one mid-call, the chosen one is unplugged and comes back, the system
//! default moves, or a stream dies without saying so.
//!
//! This is the decision part, on the audio thread (`audio::device`) and free of device code: it
//! talks to a [`MicBackend`] (cpal, or `audio::fake_mic` in the tests), so every path here runs in
//! the tests without opening a microphone. Each open builds the whole capture state from what the
//! device reports, so nothing of the previous device's format survives a change.
//!
//! The device layer only says "lost" when it knows, and how much it knows depends on the machine:
//! cpal watches a *default* stream on Windows, nothing on a device picked by name (its capture
//! thread waits on the driver's event, with no timeout, and only hears of an unplug if the driver
//! signals it), and on macOS a default input stream is simply bound to whatever the default was.
//! So, while a microphone is wanted and only then, a slow check ([`HEALTH_TICK`]) covers the rest:
//! the capture callback stopped coming, the default moved, the chosen device is back.

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

/// The slow check, only while a microphone stream is open. Nothing here runs without a mic.
pub(crate) const HEALTH_TICK: Duration = Duration::from_secs(2);
/// A stream whose callback has not run for this long is dead, whatever it reports.
pub(crate) const STALL_AFTER: Duration = Duration::from_secs(4);
/// A device that keeps stalling right after it is reopened is given longer each time.
const STALL_MAX_SHIFT: u32 = 3;
/// The chosen device is listed again but will not open: longest wait between attempts.
const RECOVER_MAX: Duration = Duration::from_secs(60);
/// Failed attempts (quick ones, a device changing rate is over in a moment) before saying so.
const QUIET_ATTEMPTS: u32 = 2;
/// A join without a microphone keeps trying at this attempt's pace (see [`reopen_delay`]).
const JOIN_ATTEMPT: u32 = 3;

/// Delays between attempts to reopen a lost stream: quick at first (a sample-rate change is over
/// in a moment), then every 5 s while the device stays missing.
pub(crate) fn reopen_delay(attempt: u32) -> Duration {
    const STEPS_MS: [u64; 5] = [300, 1000, 2000, 3000, 5000];
    Duration::from_millis(STEPS_MS[(attempt as usize).min(STEPS_MS.len() - 1)])
}

/// What changed about the microphone of a running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicStatus {
    /// The device the person just picked is capturing.
    Switched { device: String },
    /// The chosen device is not there (or will not open): the system default is capturing
    /// instead, and the chosen one takes over again if it comes back.
    FellBack { wanted: String, device: String },
    /// After a fallback or a time without any microphone, the wanted device is capturing again.
    Restored { device: String },
    /// Nothing can capture right now; it keeps trying.
    Lost,
}

/// An open microphone. Dropping it stops the capture.
pub(crate) struct MicOpen {
    /// Shown to the person.
    pub name: String,
    /// The device's stable id, to tell when the system default is no longer this one.
    pub id: Option<String>,
    /// Bumped by every capture callback (muted ones too: a muted device stays open).
    pub pulse: Arc<AtomicU64>,
    pub _stream: Box<dyn Any>,
}

/// The device side of [`MicRoute`].
pub(crate) trait MicBackend {
    /// Open `device` (`None`: the system default), reading its format now. `on_lost` is for the
    /// stream to call, from any thread, when it dies.
    fn open(&mut self, device: Option<&str>, on_lost: Box<dyn Fn() + Send>) -> Result<MicOpen, String>;
    /// Is a device of this name there now?
    fn present(&mut self, name: &str) -> bool;
    /// The current system default's stable id.
    fn default_id(&mut self) -> Option<String>;
}

pub(crate) struct MicRoute {
    backend: Box<dyn MicBackend>,
    /// Hands a stream's generation to the audio thread, which calls [`MicRoute::on_lost`].
    lost: Arc<dyn Fn(u64) + Send + Sync>,
    status: Option<Arc<dyn Fn(MicStatus) + Send + Sync>>,
    /// The voice session falls back to the default; the mic test reports the chosen one's failure.
    can_fall_back: bool,

    /// What the person chose (`None`: the system default).
    wanted: Option<String>,
    stream: Option<MicOpen>,
    /// The open stream's generation: anything an older stream still reports is ignored.
    gen: u64,
    next_gen: u64,
    /// The stream is the system default standing in for `wanted`.
    on_fallback: bool,
    fallback_name: String,
    /// The person just picked another device (answered with [`MicStatus::Switched`]).
    switching: bool,
    /// [`MicStatus::Lost`] was said (or the join reported it): the next open is news.
    told_lost: bool,

    retry_at: Option<Instant>,
    attempt: u32,

    health_at: Instant,
    last_pulse: u64,
    last_pulse_at: Instant,
    stalls: u32,
    recover_at: Instant,
    recover_fails: u32,
}

impl MicRoute {
    pub fn new(
        backend: Box<dyn MicBackend>,
        lost: Arc<dyn Fn(u64) + Send + Sync>,
        status: Option<Arc<dyn Fn(MicStatus) + Send + Sync>>,
        can_fall_back: bool,
        wanted: Option<String>,
    ) -> Self {
        let now = Instant::now();
        Self {
            backend,
            lost,
            status,
            can_fall_back,
            wanted,
            stream: None,
            gen: 0,
            next_gen: 1,
            on_fallback: false,
            fallback_name: String::new(),
            switching: false,
            told_lost: false,
            retry_at: None,
            attempt: 0,
            health_at: now + HEALTH_TICK,
            last_pulse: 0,
            last_pulse_at: now,
            stalls: 0,
            recover_at: now,
            recover_fails: 0,
        }
    }

    /// First open, at the join: no waiting, a chosen device that fails gives way to the default at
    /// once. On failure it keeps trying (the error is for the join's report).
    pub fn start(&mut self, now: Instant) -> Result<(), String> {
        let res = self.open_best(now, true);
        if res.is_err() {
            self.told_lost = true;
            self.attempt = JOIN_ATTEMPT;
            self.retry_at = Some(now + reopen_delay(JOIN_ATTEMPT));
        }
        res
    }

    /// The person picked `device` (`None`: the system default). The old stream stops first: it is
    /// no longer what they want captured, whether or not the new one opens.
    pub fn set_device(&mut self, device: Option<String>, now: Instant) {
        if device == self.wanted {
            return;
        }
        info!(device = device.as_deref().unwrap_or("(system default)"), "microphone switch requested");
        self.wanted = device;
        if self.wanted.is_none() && self.on_fallback && self.stream.is_some() {
            // The default was already standing in: it is simply what is wanted now.
            self.on_fallback = false;
            let device = self.fallback_name.clone();
            self.say(MicStatus::Switched { device });
            return;
        }
        self.stream = None;
        self.on_fallback = false;
        self.switching = true;
        self.attempt = 0;
        self.retry(now);
    }

    /// The stream of generation `gen` reported its death.
    pub fn on_lost(&mut self, gen: u64, now: Instant) {
        if gen == self.gen && self.stream.is_some() {
            warn!("microphone stream lost; reopening it");
            self.lost_now(now);
        }
    }

    /// How long the audio thread may sleep (`None`: until told something).
    pub fn next_wait(&self, now: Instant) -> Option<Duration> {
        let at = match (self.retry_at, self.stream.is_some()) {
            (Some(at), _) => at,
            (None, true) => self.health_at,
            (None, false) => return None,
        };
        Some(at.saturating_duration_since(now))
    }

    /// Whatever is due at `now`. Harmless to call early.
    pub fn on_timer(&mut self, now: Instant) {
        if self.retry_at.is_some_and(|at| now >= at) {
            self.retry(now);
        }
        if self.stream.is_some() && now >= self.health_at {
            self.health_at = now + HEALTH_TICK;
            self.health(now);
        }
    }

    fn say(&self, status: MicStatus) {
        if let Some(f) = &self.status {
            f(status);
        }
    }

    /// Open `device` and make it the stream; what it replaces (if anything) stops afterwards.
    fn open(&mut self, device: Option<&str>, now: Instant) -> Result<String, String> {
        let gen = self.next_gen;
        self.next_gen += 1;
        let lost = self.lost.clone();
        let opened = self.backend.open(device, Box::new(move || lost(gen)))?;
        let name = opened.name.clone();
        self.gen = gen;
        self.last_pulse = opened.pulse.load(Ordering::Relaxed);
        self.last_pulse_at = now;
        self.health_at = now + HEALTH_TICK;
        self.stream = Some(opened);
        Ok(name)
    }

    /// Open what is wanted, or the default in its place: when the chosen device is gone, or when
    /// `give_up` says it had its chance. The error is the wanted device's.
    fn open_best(&mut self, now: Instant, give_up: bool) -> Result<(), String> {
        let wanted = self.wanted.clone();
        let failure = match self.open(wanted.as_deref(), now) {
            Ok(device) => {
                let back = self.on_fallback || self.told_lost;
                self.on_fallback = false;
                self.told_lost = false;
                self.recover_fails = 0;
                if std::mem::take(&mut self.switching) {
                    self.say(MicStatus::Switched { device });
                } else if back {
                    self.say(MicStatus::Restored { device });
                }
                return Ok(());
            }
            Err(e) => e,
        };
        if let (Some(name), true) = (wanted, self.can_fall_back) {
            if give_up || !self.backend.present(&name) {
                match self.open(None, now) {
                    Ok(device) => {
                        warn!(wanted = %name, %device, error = %failure, "chosen microphone unavailable; using the system default");
                        // (The default changing under a fallback is no news unless it is another.)
                        let news = !self.on_fallback || self.switching || self.told_lost || self.fallback_name != device;
                        self.on_fallback = true;
                        self.switching = false;
                        self.told_lost = false;
                        self.fallback_name = device.clone();
                        self.recover_at = now + HEALTH_TICK;
                        if news {
                            self.say(MicStatus::FellBack { wanted: name, device });
                        }
                        return Ok(());
                    }
                    Err(e) => warn!(error = %e, "system default microphone unavailable too"),
                }
            }
        }
        Err(failure)
    }

    fn lost_now(&mut self, now: Instant) {
        self.stream = None;
        self.attempt = 0;
        self.retry_at = Some(now + reopen_delay(0));
    }

    fn retry(&mut self, now: Instant) {
        let n = self.attempt;
        self.retry_at = None;
        match self.open_best(now, n >= 1) {
            Ok(()) => {
                info!(attempt = n + 1, "microphone open");
                self.attempt = 0;
            }
            Err(e) => {
                if n < 3 || n % 12 == 0 {
                    warn!(attempt = n + 1, error = %e, "microphone not back yet");
                }
                self.attempt = n + 1;
                self.retry_at = Some(now + reopen_delay(n + 1));
                if self.attempt >= QUIET_ATTEMPTS && !self.told_lost {
                    self.told_lost = true;
                    self.switching = false;
                    self.say(MicStatus::Lost);
                }
            }
        }
    }

    /// The slow check: what the device layer does not say by itself.
    fn health(&mut self, now: Instant) {
        let Some(stream) = &self.stream else { return };
        let pulse = stream.pulse.load(Ordering::Relaxed);
        if pulse != self.last_pulse {
            self.last_pulse = pulse;
            self.last_pulse_at = now;
            self.stalls = 0;
        } else if now.duration_since(self.last_pulse_at) >= STALL_AFTER * (1 << self.stalls.min(STALL_MAX_SHIFT)) {
            warn!(stalls = self.stalls, "the microphone stopped delivering audio without an error; reopening it");
            self.stalls += 1;
            self.lost_now(now);
            return;
        }
        if self.wanted.is_none() || self.on_fallback {
            let current = self.backend.default_id();
            if current.is_some() && stream.id.is_some() && current != stream.id {
                info!("the system default microphone changed; following it");
                self.lost_now(now);
                return;
            }
        }
        if self.on_fallback && now >= self.recover_at {
            self.try_recover(now);
        }
    }

    /// While the default stands in: the chosen device, if it is back. The stand-in keeps running
    /// until the chosen one really opens.
    fn try_recover(&mut self, now: Instant) {
        let Some(name) = self.wanted.clone() else { return };
        if !self.backend.present(&name) {
            return;
        }
        match self.open(Some(&name), now) {
            Ok(device) => {
                info!(%device, "the chosen microphone is back");
                self.on_fallback = false;
                self.recover_fails = 0;
                self.say(MicStatus::Restored { device });
            }
            Err(e) => {
                self.recover_fails += 1;
                let wait = (HEALTH_TICK * (1 << self.recover_fails.min(5))).min(RECOVER_MAX);
                warn!(error = %e, wait_s = wait.as_secs(), "the chosen microphone is listed but does not open");
                self.recover_at = now + wait;
            }
        }
    }
}

#[cfg(test)]
impl MicRoute {
    pub fn wanted(&self) -> Option<&str> {
        self.wanted.as_deref()
    }

    pub fn current(&self) -> Option<&str> {
        self.stream.as_ref().map(|s| s.name.as_str())
    }

    pub fn is_on_fallback(&self) -> bool {
        self.on_fallback
    }

    pub fn is_retrying(&self) -> bool {
        self.retry_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashSet;

    /// Devices by name, with nothing behind them: the state machine alone. (`audio::fake_mic`
    /// runs the real capture chain under it.)
    #[derive(Default)]
    struct Script {
        listed: HashSet<String>,
        broken: HashSet<String>,
        default: Option<String>,
        /// Every open that succeeded; `alive` holds the streams that were not dropped.
        opened: Vec<String>,
        alive: Vec<String>,
        pulses: Vec<Arc<AtomicU64>>,
        on_lost: Vec<Box<dyn Fn() + Send>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Script>>);

    struct Closed(Fake, String);

    impl Drop for Closed {
        fn drop(&mut self) {
            let mut s = self.0 .0.lock();
            let i = s.alive.iter().position(|n| *n == self.1).expect("closed twice");
            s.alive.remove(i);
        }
    }

    impl MicBackend for Fake {
        fn open(&mut self, device: Option<&str>, on_lost: Box<dyn Fn() + Send>) -> Result<MicOpen, String> {
            let mut s = self.0.lock();
            let name = match device {
                Some(n) if s.listed.contains(n) => n.to_string(),
                Some(n) => return Err(format!("no input device matching {n:?}")),
                None => s.default.clone().ok_or("no default input device")?,
            };
            if s.broken.contains(&name) {
                return Err(format!("build mic stream: {name} refused the format"));
            }
            s.opened.push(name.clone());
            s.alive.push(name.clone());
            let pulse = Arc::new(AtomicU64::new(0));
            s.pulses.push(pulse.clone());
            s.on_lost.push(on_lost);
            Ok(MicOpen { name: name.clone(), id: Some(name.clone()), pulse, _stream: Box::new(Closed(self.clone(), name)) })
        }

        fn present(&mut self, name: &str) -> bool {
            self.0.lock().listed.contains(name)
        }

        fn default_id(&mut self) -> Option<String> {
            self.0.lock().default.clone()
        }
    }

    struct Rig {
        fake: Fake,
        route: MicRoute,
        said: Arc<Mutex<Vec<MicStatus>>>,
        lost: Arc<Mutex<Vec<u64>>>,
        now: Instant,
    }

    fn rig(wanted: Option<&str>, listed: &[&str], default: Option<&str>) -> Rig {
        let fake = Fake::default();
        {
            let mut s = fake.0.lock();
            s.listed = listed.iter().map(|n| n.to_string()).collect();
            s.default = default.map(str::to_string);
        }
        let said = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(Mutex::new(Vec::new()));
        let (s2, l2) = (said.clone(), lost.clone());
        let route = MicRoute::new(
            Box::new(fake.clone()),
            Arc::new(move |g| l2.lock().push(g)),
            Some(Arc::new(move |m| s2.lock().push(m))),
            true,
            wanted.map(str::to_string),
        );
        Rig { fake, route, said, lost, now: Instant::now() }
    }

    impl Rig {
        fn said(&self) -> Vec<MicStatus> {
            std::mem::take(&mut *self.said.lock())
        }

        fn alive(&self) -> Vec<String> {
            self.fake.0.lock().alive.clone()
        }

        /// Time goes by with the capture running.
        fn run(&mut self, d: Duration) {
            self.step(d, true);
        }

        /// Time goes by with the capture callback silent.
        fn stall(&mut self, d: Duration) {
            self.step(d, false);
        }

        fn step(&mut self, d: Duration, flowing: bool) {
            let end = self.now + d;
            while self.now < end {
                self.now += Duration::from_millis(100);
                if flowing {
                    if let Some(p) = self.fake.0.lock().pulses.last() {
                        p.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.deliver_lost();
                self.route.on_timer(self.now);
            }
        }

        /// What the audio thread would do with the streams' messages.
        fn deliver_lost(&mut self) {
            let lost = std::mem::take(&mut *self.lost.lock());
            for g in lost {
                self.route.on_lost(g, self.now);
            }
        }

        /// The newest stream reports its death, as cpal's error callback would.
        fn report_lost(&mut self) {
            let cb = self.fake.0.lock().on_lost.pop().expect("a stream");
            cb();
            self.fake.0.lock().on_lost.push(cb);
            self.deliver_lost();
        }

        fn unplug(&mut self, name: &str) {
            self.fake.0.lock().listed.remove(name);
        }

        fn plug(&mut self, name: &str) {
            self.fake.0.lock().listed.insert(name.to_string());
        }
    }

    const USB: &str = "Microfone (USB)";
    const CAM: &str = "Microfone (webcam)";
    const BUILTIN: &str = "Microfone interno";

    #[test]
    fn switching_stops_the_old_mic_and_opens_the_new_one_at_once() {
        let mut r = rig(None, &[USB, CAM, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        assert_eq!(r.alive(), [BUILTIN]);
        r.route.set_device(Some(CAM.into()), r.now);
        assert_eq!(r.alive(), [CAM], "the previous mic is no longer captured, right away");
        assert_eq!(r.said(), [MicStatus::Switched { device: CAM.into() }]);
        r.route.set_device(Some(USB.into()), r.now);
        assert_eq!(r.alive(), [USB]);
        r.route.set_device(None, r.now);
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.said(), [MicStatus::Switched { device: USB.into() }, MicStatus::Switched { device: BUILTIN.into() }]);
        // Picking what is already running touches nothing.
        let opens = r.fake.0.lock().opened.len();
        r.route.set_device(None, r.now);
        assert_eq!(r.fake.0.lock().opened.len(), opens);
    }

    #[test]
    fn the_old_stream_stops_before_the_new_one_opens_even_when_that_fails() {
        // Listed but refusing to open, and no default: the person wanted the old one gone.
        let mut r = rig(Some(USB), &[USB, CAM], None);
        r.route.start(r.now).unwrap();
        r.fake.0.lock().broken.insert(CAM.into());
        r.route.set_device(Some(CAM.into()), r.now);
        assert!(r.alive().is_empty());
        assert!(r.route.is_retrying());
        assert_eq!(r.route.wanted(), Some(CAM));
    }

    #[test]
    fn a_failed_build_falls_back_to_the_default_and_recovers_with_the_stand_in_running() {
        let mut r = rig(None, &[USB, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.fake.0.lock().broken.insert(USB.into());
        r.route.set_device(Some(USB.into()), r.now);
        assert!(r.alive().is_empty(), "first try failed; a second one comes in a moment");
        r.run(reopen_delay(1) + Duration::from_millis(100));
        assert_eq!(r.alive(), [BUILTIN], "the device is listed but keeps failing: the default takes over");
        assert!(r.route.is_on_fallback());
        assert_eq!(r.said(), [MicStatus::FellBack { wanted: USB.into(), device: BUILTIN.into() }]);
        // Listed and still broken: tried rarely, the stand-in never stops.
        let opens = r.fake.0.lock().opened.len();
        r.run(Duration::from_secs(30));
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.fake.0.lock().opened.len(), opens);
        // It starts working.
        r.fake.0.lock().broken.clear();
        r.run(RECOVER_MAX + HEALTH_TICK);
        assert_eq!(r.alive(), [USB]);
        assert!(!r.route.is_on_fallback());
        assert_eq!(r.said(), [MicStatus::Restored { device: USB.into() }]);
    }

    #[test]
    fn an_unplugged_mic_falls_back_at_once_and_comes_back_when_replugged() {
        let mut r = rig(Some(USB), &[USB, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.run(Duration::from_secs(5));
        r.unplug(USB);
        r.report_lost();
        assert!(r.alive().is_empty(), "the dead stream is released");
        r.run(reopen_delay(0) + Duration::from_millis(100));
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.route.wanted(), Some(USB), "the person's choice is kept");
        assert_eq!(r.said(), [MicStatus::FellBack { wanted: USB.into(), device: BUILTIN.into() }]);
        r.run(Duration::from_secs(20));
        assert!(r.said().is_empty(), "no repeated notice while it stays away");
        r.plug(USB);
        r.run(HEALTH_TICK * 2);
        assert_eq!(r.alive(), [USB]);
        assert_eq!(r.said(), [MicStatus::Restored { device: USB.into() }]);
    }

    #[test]
    fn a_stream_that_dies_without_an_error_is_noticed_by_the_slow_check() {
        let mut r = rig(Some(USB), &[USB, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.run(Duration::from_secs(10));
        // The driver never signals the unplug: no error, the callback just stops.
        r.unplug(USB);
        r.stall(STALL_AFTER + HEALTH_TICK + reopen_delay(0) + Duration::from_millis(100));
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.said(), [MicStatus::FellBack { wanted: USB.into(), device: BUILTIN.into() }]);
    }

    #[test]
    fn a_muted_or_quiet_mic_is_never_mistaken_for_a_stall() {
        let mut r = rig(Some(USB), &[USB], None);
        r.route.start(r.now).unwrap();
        r.run(Duration::from_secs(600));
        assert_eq!(r.fake.0.lock().opened.len(), 1);
    }

    #[test]
    fn a_device_that_keeps_stalling_is_reopened_more_and_more_rarely() {
        let mut r = rig(Some(USB), &[USB], None);
        r.route.start(r.now).unwrap();
        r.stall(Duration::from_secs(120));
        let opens = r.fake.0.lock().opened.len();
        assert!((3..=6).contains(&opens), "{opens} opens in 2 minutes");
        // Audio again: back to normal, no more reopens.
        r.run(Duration::from_secs(60));
        assert_eq!(r.fake.0.lock().opened.len(), opens);
    }

    #[test]
    fn a_stale_report_from_a_replaced_stream_does_not_touch_the_new_one() {
        let mut r = rig(Some(USB), &[USB, CAM], None);
        r.route.start(r.now).unwrap();
        let old = r.fake.0.lock().on_lost.pop().unwrap();
        r.route.set_device(Some(CAM.into()), r.now);
        old(); // the old stream's error callback, late
        r.run(Duration::from_secs(3));
        assert_eq!(r.alive(), [CAM]);
        assert_eq!(r.fake.0.lock().opened, [USB, CAM]);
    }

    #[test]
    fn the_stand_in_keeps_reporting_when_a_recovery_attempt_fails() {
        let mut r = rig(Some(USB), &[BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        assert_eq!(r.alive(), [BUILTIN]);
        r.plug(USB);
        r.fake.0.lock().broken.insert(USB.into());
        r.run(HEALTH_TICK * 2); // a recovery attempt fails
        r.unplug(USB);
        r.report_lost(); // the stand-in dies: this must still count
        assert!(r.alive().is_empty());
    }

    #[test]
    fn the_system_default_is_followed_where_nothing_reports_it() {
        // macOS: a default input stream stays bound to the device it opened on.
        let mut r = rig(None, &[USB, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.run(Duration::from_secs(5));
        r.fake.0.lock().default = Some(USB.into());
        r.run(HEALTH_TICK + reopen_delay(0) + Duration::from_millis(200));
        assert_eq!(r.alive(), [USB]);
        assert!(r.said().is_empty(), "this is what 'Padrão do sistema' means, nothing to announce");
    }

    #[test]
    fn a_device_picked_by_name_does_not_follow_the_default() {
        let mut r = rig(Some(CAM), &[USB, CAM, BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.fake.0.lock().default = Some(USB.into());
        r.run(Duration::from_secs(20));
        assert_eq!(r.alive(), [CAM]);
    }

    #[test]
    fn with_no_microphone_at_all_it_says_so_once_and_returns_alone() {
        let mut r = rig(Some(USB), &[USB], None);
        r.route.start(r.now).unwrap();
        r.run(Duration::from_secs(3));
        r.unplug(USB);
        r.report_lost();
        r.run(Duration::from_secs(1));
        assert!(r.said().is_empty(), "a moment of grace (a device changing rate reopens quickly)");
        r.run(Duration::from_secs(60));
        assert_eq!(r.said(), [MicStatus::Lost]);
        assert!(r.route.is_retrying());
        r.plug(USB);
        r.run(reopen_delay(9));
        assert_eq!(r.alive(), [USB]);
        assert_eq!(r.said(), [MicStatus::Restored { device: USB.into() }]);
        assert!(!r.route.is_retrying());
    }

    #[test]
    fn a_join_with_a_missing_device_starts_on_the_default() {
        let mut r = rig(Some(USB), &[BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.said(), [MicStatus::FellBack { wanted: USB.into(), device: BUILTIN.into() }]);
    }

    #[test]
    fn a_join_with_no_microphone_reports_it_and_a_mic_plugged_later_is_news() {
        let mut r = rig(None, &[], None);
        assert!(r.route.start(r.now).is_err());
        assert!(r.route.is_retrying());
        r.run(Duration::from_secs(20));
        assert!(r.said().is_empty(), "the join's warning already said it");
        r.fake.0.lock().default = Some(BUILTIN.into());
        r.plug(BUILTIN);
        r.run(reopen_delay(9));
        assert_eq!(r.alive(), [BUILTIN]);
        assert_eq!(r.said(), [MicStatus::Restored { device: BUILTIN.into() }]);
    }

    #[test]
    fn picking_the_default_while_it_stands_in_keeps_it_running() {
        let mut r = rig(Some(USB), &[BUILTIN], Some(BUILTIN));
        r.route.start(r.now).unwrap();
        r.said();
        let opens = r.fake.0.lock().opened.len();
        r.route.set_device(None, r.now);
        assert_eq!(r.fake.0.lock().opened.len(), opens, "no gap");
        assert_eq!(r.said(), [MicStatus::Switched { device: BUILTIN.into() }]);
        assert!(!r.route.is_on_fallback());
        r.plug(USB);
        r.run(Duration::from_secs(10));
        assert_eq!(r.alive(), [BUILTIN], "nothing to recover any more");
    }

    #[test]
    fn the_mic_test_reports_the_chosen_device_instead_of_testing_another() {
        let mut r = rig(Some(USB), &[BUILTIN], Some(BUILTIN));
        r.route.can_fall_back = false;
        let e = r.route.start(r.now).unwrap_err();
        assert!(e.contains("matching"), "{e}");
        assert!(r.alive().is_empty());
    }

    #[test]
    fn it_sleeps_until_told_when_there_is_nothing_to_watch_and_slowly_otherwise() {
        let mut r = rig(Some(USB), &[USB], None);
        assert_eq!(r.route.next_wait(r.now), None, "nothing open, nothing due");
        r.route.start(r.now).unwrap();
        assert_eq!(r.route.next_wait(r.now), Some(HEALTH_TICK));
        assert!(r.route.next_wait(r.now + Duration::from_secs(9)).is_some_and(|d| d.is_zero()), "overdue: at once");
        r.route.set_device(None, r.now); // no default: nothing opens
        assert!(r.route.is_retrying());
        assert_eq!(r.route.next_wait(r.now), Some(reopen_delay(1)));
    }
}
