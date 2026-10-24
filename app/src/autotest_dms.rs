//! Direct-message and private-call autotest (a child module of `autotest`).
//!
//! - `dm-a` (name `dmA...`): opens a DM with `dmB...` (same suffix), writes, waits for the reply,
//!   presses "Ligar", and once B answered checks it is in the private room `dm-<a>-<b>` with B;
//!   then goes "Só áudio" live with a synthetic tone (and turns the camera on when
//!   `VOX_FAKE_CAMERA=1`), waits for B's "ok", hangs up and checks the call line.
//! - `dm-b`: gets the DM (the "Mensagem direta" alert is requested), replies, gets the call
//!   (the ringtone is requested: silent, `VOX_NO_PLAYBACK=1`), answers, checks the room, that the
//!   hub's voice state shows neither of them, listens to A's tone (audio counters) and A's camera
//!   (frame counters), says "ok" in the DM and waits for A to hang up.
//!
//! Silent: `VOX_NO_PLAYBACK=1` and `VOX_NO_MIC=1` (the tone is synthetic and only measured).
//! `--show hold --hold N`: B lets the call ring for N s (at most 20) before answering, and A keeps
//! the call up N s after B's checks, for a look at the prompt and the call view.

use std::time::Instant;

use vox_proto::{dm_room, CallEndReason, CallState, DmMessageKind, UserId};

use super::Auto;
use crate::app::dms::DM_CALL_CHANNEL;
use crate::app::{App, Center, Share};

#[derive(Default)]
pub struct State {
    peer: Option<UserId>,
    dm: Option<i64>,
    chunks: u64,
    cam_frames: u64,
    at: Option<Instant>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST dms: {what}");
}

impl Auto {
    /// `--show hold`: seconds to linger at the interesting screens.
    fn dm_hold(&self) -> u64 {
        if self.args.show.as_deref() == Some("hold") { self.args.hold_secs.min(20) } else { 0 }
    }

    fn dm_tag(&self, what: &str) -> String {
        format!("dmtest {what} {}", self.args.run_id)
    }

    /// The other autotest's name: `dmA1` <-> `dmB1`.
    fn dm_peer_name(&self) -> String {
        let n = &self.args.name;
        if let Some(rest) = n.strip_prefix("dmA") {
            format!("dmB{rest}")
        } else if let Some(rest) = n.strip_prefix("dmB") {
            format!("dmA{rest}")
        } else {
            String::new()
        }
    }

    fn dm_has_line(app: &App, dm: i64, text: &str, author: Option<UserId>) -> bool {
        app.dms.logs.get(&dm).is_some_and(|l| l.msgs.iter().any(|m| m.content == text && author.is_none_or(|a| m.author.id == a)))
    }

    fn dm_say(app: &mut App, dm: i64, text: &str) {
        app.select_dm(dm);
        app.dms.input = text.to_string();
        app.send_dm();
    }

    /// In the DM call with the other person, in the right private room.
    fn dm_in_call(&self, app: &App) -> Result<bool, String> {
        let (Some(me), Some(peer)) = (app.my_id(), self.dm.peer) else { return Ok(false) };
        let joined = app.voice.channel == Some(DM_CALL_CHANNEL) && app.voice.joining.is_none();
        let active = app.dms.call.as_ref().is_some_and(|c| c.state == CallState::Active);
        if !(joined && active && app.members_here().len() == 2) {
            return Ok(false);
        }
        let want = dm_room(me, peer);
        match app.dms.room.as_deref() {
            Some(r) if r == want => Ok(true),
            Some(r) => Err(format!("in room {r}, expected {want}")),
            None => Ok(false),
        }
    }

    /// Logged in, the DM list arrived, the other person online.
    fn dm_ready(&mut self, app: &mut App) -> Option<Result<UserId, i32>> {
        if !(self.logged_in(app) && app.dms.listed) {
            if self.waited(30) {
                return Some(Err(self.fail("could not log in / no DM list").unwrap_or(1)));
            }
            return None;
        }
        if !app.dms.supported {
            return Some(Err(self.fail("the hub has no DMs").unwrap_or(1)));
        }
        let name = self.dm_peer_name();
        match app.online.iter().find(|u| u.username == name) {
            // v1.1 (E1): an encrypted call needs this computer registered and the other person's
            // device on the list first (otherwise the call is rightly a plain one).
            Some(u) if std::env::var("VOX_TEST_EXPECT_E2EE").as_deref() == Ok("1") && !app.privacy.view.can_seal_for(u.id) => {
                if self.waited(60) {
                    return Some(Err(self.fail(&format!("e2ee never ready for a call with {name}: {:?}", app.privacy.view.status)).unwrap_or(1)));
                }
                None
            }
            Some(u) => Some(Ok(u.id)),
            None if self.waited(60) => Some(Err(self.fail(&format!("{name} never came online")).unwrap_or(1))),
            None => None,
        }
    }

    pub(super) fn run_dm_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => match self.dm_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.dm.peer = Some(peer);
                    app.open_dm(peer);
                    self.goto(2, "opening the DM");
                }
                None => {}
            },
            2 => {
                if let Some(dm) = app.dms.selected.filter(|d| app.dms.conv(*d).is_some_and(|c| Some(c.peer.id) == self.dm.peer)) {
                    self.dm.dm = Some(dm);
                    let text = self.dm_tag("oi");
                    Self::dm_say(app, dm, &text);
                    self.goto(3, "wrote to B");
                } else if self.waited(15) {
                    return self.fail(&format!("the DM did not open: {:?}", app.notice));
                }
            }
            3 => {
                let dm = self.dm.dm?;
                if Self::dm_has_line(app, dm, &self.dm_tag("resposta"), self.dm.peer) {
                    let mine = Self::dm_has_line(app, dm, &self.dm_tag("oi"), app.my_id());
                    let conv = app.dms.conv(dm).cloned();
                    log(&format!("B replied; my line in the log: {mine}; list entry: unread={:?} preview={:?}", conv.as_ref().map(|c| c.unread), conv.as_ref().and_then(|c| c.last_message.as_ref().map(|m| m.content.clone()))));
                    if !mine {
                        return self.fail("my own message is not in the DM");
                    }
                    if app.dms.convs.first().map(|c| c.id) != Some(dm) {
                        return self.fail("the DM is not on top of the list");
                    }
                    app.start_dm_call(dm);
                    self.goto(4, "pressed Ligar");
                } else if self.waited(45) {
                    return self.fail("B never replied");
                }
            }
            4 => match self.dm_in_call(app) {
                Err(e) => return self.fail(&e),
                Ok(true) => {
                    log(&format!("in the private room {:?} with B; members {}", app.dms.room, app.members_here().len()));
                    if app.voice_members.contains_key(&app.my_id().unwrap_or_default()) {
                        return self.fail("the hub lists me in a voice channel during a private call");
                    }
                    app.start_audio_share_source(vox_client_core::golive::ShareSource::AudioTone, None, false);
                    if std::env::var("VOX_FAKE_CAMERA").is_ok_and(|v| v == "1") {
                        app.start_camera();
                    }
                    self.goto(5, "B answered; going live with a tone");
                }
                Ok(false) => {
                    if let Some(e) = app.voice.error.clone() {
                        return self.fail(&format!("the call failed: {e}"));
                    }
                    if self.waited(60) {
                        return self.fail(&format!("B never answered (call {:?}, voice {:?}, members {})", app.dms.call.as_ref().map(|c| c.state), app.voice.channel, app.members_here().len()));
                    }
                }
            },
            5 => {
                if matches!(app.voice.share, Share::Live { .. }) {
                    self.goto(6, "live; waiting for B's checks");
                } else if self.waited(30) {
                    return self.fail(&format!("the tone live did not start: {:?}", app.notice));
                }
            }
            6 => {
                let dm = self.dm.dm?;
                if Self::dm_has_line(app, dm, &self.dm_tag("ok"), self.dm.peer) {
                    if self.dm_hold() > 0 && self.dm.at.is_none() {
                        self.dm.at = Some(Instant::now());
                        app.center = Center::Call;
                    }
                    if self.dm.at.is_none_or(|t| t.elapsed().as_secs() >= self.dm_hold()) {
                        app.hang_up_dm_call();
                        self.goto(7, "B is done; hung up");
                    }
                } else if Self::dm_has_line(app, dm, &self.dm_tag("falhou"), self.dm.peer) {
                    app.hang_up_dm_call();
                    return self.fail("B's checks failed");
                } else if self.waited(120) {
                    return self.fail("B never finished its checks");
                }
            }
            7 => {
                let dm = self.dm.dm?;
                let ended = app.dms.logs.get(&dm).and_then(|l| l.msgs.last()).is_some_and(|m| m.kind == DmMessageKind::CallEnded && m.call_secs >= 1);
                if app.voice.channel.is_none() && app.dms.call.is_none() && ended {
                    log("the call is over and logged in the DM");
                    return self.pass();
                } else if self.waited(20) {
                    return self.fail("the call did not end cleanly");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_dm_b(&mut self, app: &mut App) -> Option<i32> {
        let tell_fail = |app: &mut App, me: &Auto, reason: &str| {
            if let Some(dm) = me.dm.dm {
                let text = me.dm_tag("falhou");
                Self::dm_say(app, dm, &text);
            }
            me.fail(reason)
        };
        match self.step {
            0 => self.connect(app),
            1 => match self.dm_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.dm.peer = Some(peer);
                    self.goto(2, "waiting for A's message");
                }
                None => {}
            },
            2 => {
                let peer = self.dm.peer?;
                if let Some(c) = app.dms.conv_with(peer).filter(|c| c.last_message.is_some()).cloned() {
                    log(&format!("A's DM is in my list: unread={} preview={:?}; DM alerts requested {}, taskbar flashes {}", c.unread, c.last_message.as_ref().map(|m| &m.content), app.dms.dm_alerts, app.dms.flashes));
                    self.dm.dm = Some(c.id);
                    app.select_dm(c.id);
                    self.goto(3, "opened A's DM");
                } else if self.waited(60) {
                    return self.fail("no DM from A");
                }
            }
            3 => {
                let dm = self.dm.dm?;
                if Self::dm_has_line(app, dm, &self.dm_tag("oi"), self.dm.peer) {
                    if app.dms.dm_alerts == 0 && app.settings.alerts.enabled {
                        return self.fail("no \"Mensagem direta\" alert was requested");
                    }
                    let text = self.dm_tag("resposta");
                    Self::dm_say(app, dm, &text);
                    self.goto(4, "replied; waiting for the call");
                } else if self.waited(20) {
                    return self.fail("A's message is not in the conversation");
                }
            }
            4 => {
                if let Some(call) = app.dms.incoming(app.my_id()).cloned() {
                    if (self.waited(3) || app.dms.rings >= 2) && self.waited(self.dm_hold()) {
                        log(&format!("{} is calling (call {}); ringtone requested {} times (silent)", call.caller.username, call.id, app.dms.rings));
                        if app.dms.rings == 0 {
                            return self.fail("the ringtone was never requested");
                        }
                        app.accept_dm_call();
                        self.goto(5, "answered");
                    }
                } else if self.waited(60) {
                    return self.fail("A never called");
                }
            }
            5 => match self.dm_in_call(app) {
                Err(e) => return tell_fail(app, self, &e),
                Ok(true) => {
                    let (a, b) = (self.dm.peer.unwrap_or_default(), app.my_id().unwrap_or_default());
                    if app.voice_members.contains_key(&a) || app.voice_members.contains_key(&b) {
                        return tell_fail(app, self, "the hub's voice state shows the private call");
                    }
                    if app.dms.rings > 0 && app.dms.incoming(app.my_id()).is_some() {
                        return tell_fail(app, self, "still ringing after answering");
                    }
                    log(&format!("in the private room {:?} with A; the hub's voice state shows neither of us", app.dms.room));
                    app.center = Center::Call;
                    self.goto(6, "waiting for A's live");
                }
                Ok(false) => {
                    if let Some(e) = app.voice.error.clone() {
                        return self.fail(&format!("could not join the call: {e}"));
                    }
                    if self.waited(60) {
                        return self.fail("the call never connected");
                    }
                }
            },
            6 => {
                let peer = self.dm.peer?.to_string();
                let live = app.members_here().iter().any(|m| m.user.id.to_string() == peer && m.streaming);
                if live && app.voice.audio_only.contains(&peer) {
                    if !app.watch(&peer) {
                        return tell_fail(app, self, "Ouvir was refused");
                    }
                    self.goto(7, "pressed Ouvir on A's tile");
                } else if self.waited(60) {
                    return tell_fail(app, self, "A's live never showed in the call");
                }
            }
            7 => {
                let peer = self.dm.peer?.to_string();
                if app.is_watching(&peer) && self.waited(4) && self.fresh_stats(app) {
                    self.dm.chunks = Self::ao_audio(app, &format!("watch-audio:{peer}")).map(|a| a.chunks).unwrap_or(0);
                    self.dm.cam_frames = Self::stat_count(app, &format!("cam-video:{peer}")).unwrap_or(0);
                    self.dm.at = Some(Instant::now());
                    self.goto(8, &format!("first sample: {} audio chunks, {} camera frames", self.dm.chunks, self.dm.cam_frames));
                } else if self.waited(30) {
                    return tell_fail(app, self, "listening never started");
                }
            }
            8 if self.waited(4) => {
                if !self.fresh_stats(app) {
                    return None;
                }
                let peer = self.dm.peer?.to_string();
                let secs = self.dm.at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(4.0);
                let info = Self::ao_audio(app, &format!("watch-audio:{peer}")).cloned();
                let Some(info) = info else { return tell_fail(app, self, "A's audio is not subscribed") };
                let rate = info.chunks.saturating_sub(self.dm.chunks) as f64 / secs;
                let cams = Self::stat_count(app, &format!("cam-video:{peer}"));
                let fps = cams.map(|f| f.saturating_sub(self.dm.cam_frames) as f64 / secs);
                log(&format!("audio {rate:.0} chunks/s, rms {:?} dBFS; A's camera on={} frames/s {fps:?}", info.rms_dbfs, app.voice.cameras.contains(&peer)));
                if rate < 50.0 || !info.rms_dbfs.is_some_and(|d| d > -60.0) {
                    return tell_fail(app, self, &format!("no audio from A in the call ({rate:.0} chunks/s, {:?} dBFS)", info.rms_dbfs));
                }
                if app.voice.cameras.contains(&peer) && fps.is_none_or(|f| f < 3.0) {
                    return tell_fail(app, self, &format!("A's camera is on but its frames do not arrive ({fps:?}/s)"));
                }
                // v1.1 (E1): between two 1.1 apps with device keys the call is end-to-end encrypted
                // (`VOX_TEST_EXPECT_E2EE=1`: it must be, and A's media must decrypt here).
                let state = app.voice.e2ee_states.get(&peer).copied();
                log(&format!("call end-to-end encrypted: {}; A's media state {state:?}", app.voice.e2ee));
                if std::env::var("VOX_TEST_EXPECT_E2EE").as_deref() == Ok("1") {
                    if !app.voice.e2ee {
                        return tell_fail(app, self, "the call is not end-to-end encrypted");
                    }
                    if state != Some(vox_client_core::MediaE2eeState::Ok) {
                        return tell_fail(app, self, &format!("A's media state is {state:?}, not Ok"));
                    }
                    super::shots::save(app, "e2v-chamada-dm");
                }
                let dm = self.dm.dm?;
                let text = self.dm_tag("ok");
                Self::dm_say(app, dm, &text);
                self.goto(9, "checks passed; told A");
            }
            9 => {
                let over = matches!(app.dms.last_end, Some((_, CallEndReason::Hangup)));
                if over && app.voice.channel.is_none() {
                    log("A hung up; I left the room");
                    return self.pass();
                } else if self.waited(30) {
                    return self.fail("A never hung up");
                }
            }
            _ => {}
        }
        None
    }
}
