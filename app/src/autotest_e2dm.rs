//! v1.1 (E2): end-to-end encrypted DMs, end to end in real apps (a child module of `autotest`).
//! The processes share files in `--mode <dir>`. Names: A is `dmA<n>`, B is `dmB<n>` (the same
//! scheme as the `dm` scenario, so the released v1.0.2 app's `dm-b` can join as B's second device
//! and log the placeholder it sees).
//!
//! - `e2dm-a` (A's first computer): waits until its second computer (`e2dm-a2`) and B are on the
//!   device list, opens the DM with B, sends a text and then a generated photo with a caption.
//!   Both must be sealed (never the "Enviar sem criptografia?" question) and come back opened on
//!   this computer; writes `a-sent.txt`; then waits for B's encrypted reply.
//! - `e2dm-b`: gets both lines opened (text, and the photo decrypted, drawn as a thumbnail and
//!   decoded full size in the viewer), replies (sealed), takes the pictures `e2dm-conversa` and
//!   `e2dm-codigo` (the "Verificar código de segurança" dialog, with `VOX_AUTOTEST_SHOT_DIR`),
//!   marks the code verified (`e2dm-codigo-verificado`, `b-verified.txt`), then waits for A's new computer: the local lines
//!   "O código de segurança de dmA.. mudou" / "dmA.. adicionou um aparelho: Aparelho novo" and the
//!   "Verifique de novo" banner (`e2dm-verifique-de-novo`).
//! - `e2dm-a2` (A's second computer, registered before A sends): reads A's own sent lines (text
//!   and photo), which A's first computer sealed for it too.
//! - `e2dm-new` (A's third computer, logs in once B verified): the lines from before it existed
//!   say "Mensagem enviada antes deste aparelho existir".
//!
//! Generated pictures only; silent (`VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`).

use std::path::PathBuf;
use std::time::Duration;

use vox_proto::{Attachment, DmId, DmMessage, UserId};

use super::e2eetest::{save_shots, shot};
use super::Auto;
use crate::app::App;
use crate::backend::e2ee::dm as e2dm;
use crate::backend::e2ee::E2eeStatus;
use crate::chat_images::{ImgKey, Target};

/// The texts: the gate looks for each of them in the hub's database and files (never there).
pub const SECRET_TEXT: &str = "e2dm segredo: a senha do cofre é 4-8-15-16";
pub const PHOTO_CAPTION: &str = "e2dm legenda secreta da foto";
pub const REPLY_TEXT: &str = "e2dm resposta cifrada da Bia";
const A2_LABEL: &str = "Notebook de teste";
const NEW_LABEL: &str = "Aparelho novo";

#[derive(Default)]
pub struct State {
    peer: Option<UserId>,
    dm: Option<DmId>,
    att: Option<Attachment>,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST e2dm: {what}");
}

/// A 1200x800 photo-like JPEG (gradients and grain; never anybody's real picture).
fn photo() -> Vec<u8> {
    let mut seed = 11u32;
    let img = image::RgbImage::from_fn(1200, 800, |x, y| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = (seed >> 28) as u8;
        image::Rgb([(x / 5 % 256) as u8 / 2 + n, (y / 4 % 256) as u8 / 2 + n, 90u8.wrapping_add(n)])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88).encode_image(&img).expect("encode the test photo");
    jpeg
}

impl Auto {
    fn ed_dir(&self) -> PathBuf {
        PathBuf::from(self.args.mode.clone().unwrap_or_else(|| ".".into()))
    }

    fn ed_flag(&self, name: &str) -> bool {
        self.ed_dir().join(name).exists()
    }

    fn ed_set(&self, name: &str) {
        if let Err(e) = std::fs::write(self.ed_dir().join(name), "ok") {
            tracing::warn!(error = %e, file = name, "AUTOTEST e2dm: cannot write a flag file");
        }
    }

    fn ed_secs(&self, secs: f32) -> bool {
        self.step_at.elapsed() >= Duration::from_secs_f32(secs)
    }

    /// `dmA1` <-> `dmB1`.
    fn ed_peer_name(&self) -> String {
        let n = &self.args.name;
        match (n.strip_prefix("dmA"), n.strip_prefix("dmB")) {
            (Some(rest), _) => format!("dmB{rest}"),
            (_, Some(rest)) => format!("dmA{rest}"),
            _ => String::new(),
        }
    }

    /// Logged in, this computer registered, the DM list arrived and the other person online.
    fn ed_ready(&mut self, app: &App) -> Option<Result<UserId, i32>> {
        let v = &app.privacy.view;
        if !(self.logged_in(app) && app.dms.listed && v.status == E2eeStatus::Active) {
            if self.waited(45) {
                return Some(Err(self.fail(&format!("not ready: logged in {}, list {}, e2ee {:?} {:?}", self.logged_in(app), app.dms.listed, v.status, v.error)).unwrap_or(1)));
            }
            return None;
        }
        let name = self.ed_peer_name();
        match app.online.iter().find(|u| u.username == name) {
            Some(u) => Some(Ok(u.id)),
            None if self.waited(90) => Some(Err(self.fail(&format!("{name} never came online")).unwrap_or(1))),
            None => None,
        }
    }

    fn ed_line<'a>(app: &'a App, dm: DmId, f: impl Fn(&DmMessage) -> bool) -> Option<&'a DmMessage> {
        app.dms.logs.get(&dm).and_then(|l| l.msgs.iter().find(|m| f(m)))
    }

    /// The conversation with `peer` is listed: show it.
    fn ed_open(&mut self, app: &mut App, peer: UserId) -> Option<DmId> {
        let dm = app.dms.conv_with(peer).map(|c| c.id)?;
        if app.dms.selected != Some(dm) {
            app.select_dm(dm);
        }
        Some(dm)
    }

    /// A line must be an opened, sealed one with this text.
    fn ed_opened(m: &DmMessage, text: &str) -> bool {
        m.sealed.as_deref() == Some(e2dm::OPENED) && m.caption() == text
    }

    pub(super) fn run_e2dm_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        ctx.request_repaint_after(Duration::from_millis(100));
        if let Some((_, q)) = &app.dms.ask_plain {
            return self.fail(&format!("asked to send without encryption: {q}"));
        }
        match self.step {
            0 => self.connect(app),
            1 => match self.ed_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.e2dm.peer = Some(peer);
                    self.goto(2, "waiting for my second computer and B's device on the list");
                }
                None => {}
            },
            2 => {
                let v = &app.privacy.view;
                let peer = self.e2dm.peer?;
                if v.mine.len() == 2 && v.can_seal_for(peer) {
                    log(&format!("devices: mine {}, B's {}", v.mine.len(), v.devices_of.get(&peer).map_or(0, Vec::len)));
                    app.open_dm(peer);
                    self.goto(3, "opening the DM with B");
                } else if self.waited(90) {
                    return self.fail(&format!("devices: mine {}, B can be sealed for: {}", v.mine.len(), v.can_seal_for(peer)));
                }
            }
            3 => {
                let peer = self.e2dm.peer?;
                if let Some(dm) = self.ed_open(app, peer) {
                    self.e2dm.dm = Some(dm);
                    app.dms.input = SECRET_TEXT.to_string();
                    app.send_dm();
                    self.goto(4, "sent the text");
                } else if self.waited(30) {
                    return self.fail("the DM never opened");
                }
            }
            4 => {
                let dm = self.e2dm.dm?;
                if let Some(m) = Self::ed_line(app, dm, |m| m.caption() == SECRET_TEXT) {
                    if !Self::ed_opened(m, SECRET_TEXT) {
                        return self.fail(&format!("my line came back as {:?}", m.sealed));
                    }
                    log(&format!("line {} sealed and opened again here", m.id));
                    app.attach_bytes(Target::Dm(dm), "foto.jpg".into(), photo(), &ctx);
                    self.goto(5, "attached a photo; compressing");
                } else if self.waited(30) {
                    return self.fail(&format!("the text never came back (notice {:?})", app.notice));
                }
            }
            5 => {
                let target = Target::Dm(self.e2dm.dm?);
                if app.images.ready(target) {
                    app.dms.input = PHOTO_CAPTION.to_string();
                    app.send_dm();
                    if !app.images.sending_to(target) {
                        return self.fail("the photo did not start sending");
                    }
                    self.goto(6, "sending the photo");
                } else if self.waited(60) {
                    return self.fail("the photo was not compressed");
                }
            }
            6 => {
                let dm = self.e2dm.dm?;
                match Self::ed_line(app, dm, |m| m.caption() == PHOTO_CAPTION).cloned() {
                    Some(m) if Self::ed_opened(&m, PHOTO_CAPTION) && m.attachments.len() == 1 && m.attachments[0].sealed => {
                        let a = &m.attachments[0];
                        log(&format!("photo line {}: image {} {}x{} {} (sealed)", m.id, a.id, a.width, a.height, a.mime));
                        self.ed_set("a-sent.txt");
                        self.goto(7, "waiting for B's reply");
                    }
                    Some(m) => return self.fail(&format!("the photo line came back as {:?} with {} images", m.sealed, m.attachments.len())),
                    None if self.waited(60) => return self.fail(&format!("the photo line never came back (notice {:?})", app.notice)),
                    None => {}
                }
            }
            7 => {
                let dm = self.e2dm.dm?;
                if Self::ed_line(app, dm, |m| Some(m.author.id) == self.e2dm.peer && Self::ed_opened(m, REPLY_TEXT)).is_some() {
                    log("B's encrypted reply opened");
                    self.goto(8, "waiting for the others");
                } else if self.waited(120) {
                    return self.fail("B's reply never arrived (opened)");
                }
            }
            8 => {
                if self.ed_flag("b-done.txt") && self.ed_flag("a2-done.txt") && self.ed_flag("new-done.txt") {
                    return self.pass();
                }
                if self.waited(180) {
                    return self.fail("the others did not finish");
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_e2dm_b(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        save_shots(&ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
        if let Some((_, q)) = &app.dms.ask_plain {
            return self.fail(&format!("asked to send without encryption: {q}"));
        }
        match self.step {
            0 => self.connect(app),
            1 => match self.ed_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.e2dm.peer = Some(peer);
                    self.goto(2, "waiting for A's DM");
                }
                None => {}
            },
            2 => {
                let peer = self.e2dm.peer?;
                if let Some(dm) = self.ed_open(app, peer) {
                    self.e2dm.dm = Some(dm);
                    self.goto(3, "waiting for A's lines");
                } else if self.waited(120) {
                    return self.fail("no DM from A");
                }
            }
            3 => {
                let dm = self.e2dm.dm?;
                let text = Self::ed_line(app, dm, |m| Self::ed_opened(m, SECRET_TEXT)).is_some();
                let photo = Self::ed_line(app, dm, |m| Self::ed_opened(m, PHOTO_CAPTION)).cloned();
                if let Some(m) = Self::ed_line(app, dm, |m| e2dm::is_unreadable(m)) {
                    return self.fail(&format!("a line did not open: {:?} \"{}\"", m.sealed, m.content));
                }
                match photo {
                    Some(m) if text => {
                        let [a] = m.attachments.as_slice() else { return self.fail(&format!("the photo line has {} images", m.attachments.len())) };
                        if !a.sealed || a.mime != "image/webp" && a.mime != "image/jpeg" || a.width == 0 {
                            return self.fail(&format!("the image is {a:?}"));
                        }
                        log(&format!("both lines opened; image {} {}x{} {}", a.id, a.width, a.height, a.mime));
                        self.e2dm.att = Some(a.clone());
                        self.goto(4, "waiting for the thumbnail to be drawn");
                    }
                    _ if self.waited(120) => return self.fail(&format!("A's lines did not arrive opened (text {text})")),
                    _ => {}
                }
            }
            4 => {
                let a = self.e2dm.att.clone()?;
                if app.images.store.thumb_shown(ImgKey::of(&a)) {
                    log("the decrypted thumbnail was drawn");
                    app.open_viewer(vec![a], 0);
                    self.goto(5, "viewer opened");
                } else if self.waited(30) {
                    return self.fail("the thumbnail was not drawn");
                }
            }
            5 => {
                let a = self.e2dm.att.clone()?;
                match app.images.store.full_size(ImgKey::of(&a)) {
                    Some(size) if size == (a.width, a.height) => {
                        log(&format!("the decrypted full image decoded at {}x{}", size.0, size.1));
                        app.close_viewer();
                        app.dms.input = REPLY_TEXT.to_string();
                        app.send_dm();
                        self.goto(6, "replied");
                    }
                    Some(size) => return self.fail(&format!("full image {size:?}, expected {}x{}", a.width, a.height)),
                    None if self.waited(30) => return self.fail("the full image never loaded"),
                    None => {}
                }
            }
            6 => {
                let dm = self.e2dm.dm?;
                if Self::ed_line(app, dm, |m| Self::ed_opened(m, REPLY_TEXT)).is_some() && self.ed_secs(1.5) {
                    if !app.privacy.view.can_seal_for(self.e2dm.peer?) {
                        return self.fail("the header would not say \"Criptografada de ponta a ponta\"");
                    }
                    shot(&ctx, "e2dm-conversa");
                    self.goto(7, "picture of the encrypted DM");
                } else if self.waited(30) {
                    return self.fail("my reply did not come back opened");
                }
            }
            7 if self.ed_secs(1.0) => {
                app.dms.safety_for = self.e2dm.peer;
                self.goto(8, "safety code dialog");
            }
            8 if self.ed_secs(1.5) => {
                let peer = self.e2dm.peer?;
                let Some(s) = app.privacy.view.safety.get(&peer).cloned() else { return self.fail("no safety code for A") };
                let groups: Vec<&str> = s.code.split(' ').collect();
                if groups.len() != 6 || groups.iter().any(|g| g.len() != 5 || !g.chars().all(|c| c.is_ascii_digit())) {
                    return self.fail(&format!("the code looks wrong: {}", s.code));
                }
                log(&format!("safety code {} ({})", s.code, if s.verified { "verified" } else { "not verified" }));
                shot(&ctx, "e2dm-codigo");
                self.goto(9, "picture of the safety code");
            }
            // (The picture is of a later frame: the dialog stays open until it is taken.)
            9 if self.ed_secs(1.5) => {
                let peer = self.e2dm.peer?;
                app.backend.send(crate::backend::Cmd::E2ee(crate::backend::e2ee::E2eeCmd::Verify { user: peer, on: true }));
                self.goto(10, "marking it verified");
            }
            10 => {
                let peer = self.e2dm.peer?;
                if app.privacy.view.safety.get(&peer).is_some_and(|s| s.verified) {
                    log("safety code verified");
                    shot(&ctx, "e2dm-codigo-verificado");
                    self.goto(20, "picture of the verified code");
                } else if self.waited(15) {
                    return self.fail("the code was not marked verified");
                }
            }
            20 if self.ed_secs(1.5) => {
                app.dms.safety_for = None;
                self.ed_set("b-verified.txt");
                self.goto(21, "waiting for A's new computer");
            }
            21 => {
                let peer = self.e2dm.peer?;
                let name = self.ed_peer_name();
                let notes = app.dms.notes.get(&peer).cloned().unwrap_or_default();
                let changed = crate::app::dms::code_changed_text(&name);
                let added = crate::app::dms::device_added_text(&name, NEW_LABEL);
                let reverify = app.privacy.view.safety.get(&peer).is_some_and(|s| s.reverify && !s.verified);
                if notes.iter().any(|(_, t)| *t == changed) && notes.iter().any(|(_, t)| *t == added) && reverify {
                    log(&format!("local lines: {:?}; \"Verifique de novo\" shown", notes.iter().map(|(_, t)| t).collect::<Vec<_>>()));
                    self.goto(11, "picture of the banner");
                } else if self.waited(150) {
                    return self.fail(&format!("notes {notes:?}, reverify {reverify}"));
                }
            }
            11 if self.ed_secs(1.5) => {
                shot(&ctx, "e2dm-verifique-de-novo");
                self.goto(12, "done");
            }
            12 if self.ed_secs(1.5) => {
                self.ed_set("b-done.txt");
                return self.pass();
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_e2dm_a2(&mut self, app: &mut App) -> Option<i32> {
        crate::backend::e2ee::set_label_override(A2_LABEL.into());
        match self.step {
            0 => self.connect(app),
            1 => match self.ed_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.e2dm.peer = Some(peer);
                    self.goto(2, "registered; waiting for A to send");
                }
                None => {}
            },
            2 => {
                if self.ed_flag("a-sent.txt") {
                    let peer = self.e2dm.peer?;
                    if let Some(dm) = self.ed_open(app, peer) {
                        self.e2dm.dm = Some(dm);
                        self.goto(3, "reading my own lines");
                    }
                } else if self.waited(150) {
                    return self.fail("A never sent");
                }
            }
            3 => {
                let dm = self.e2dm.dm?;
                let me = app.my_id();
                let text = Self::ed_line(app, dm, |m| Some(m.author.id) == me && Self::ed_opened(m, SECRET_TEXT)).is_some();
                let photo = Self::ed_line(app, dm, |m| Some(m.author.id) == me && Self::ed_opened(m, PHOTO_CAPTION) && m.attachments.len() == 1).cloned();
                match photo {
                    Some(m) if text => {
                        let a = &m.attachments[0];
                        if app.images.store.thumb_shown(ImgKey::of(a)) {
                            log("my own lines from my other computer opened here, thumbnail drawn");
                            self.ed_set("a2-done.txt");
                            return self.pass();
                        }
                    }
                    _ => {}
                }
                if self.waited(60) {
                    let lines: Vec<_> = app.dms.logs.get(&dm).map(|l| l.msgs.iter().map(|m| (m.id, m.sealed.clone())).collect()).unwrap_or_default();
                    return self.fail(&format!("my own lines did not open here: {lines:?}"));
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn run_e2dm_new(&mut self, app: &mut App) -> Option<i32> {
        crate::backend::e2ee::set_label_override(NEW_LABEL.into());
        match self.step {
            0 => {
                if self.ed_flag("b-verified.txt") {
                    self.connect(app);
                } else if self.waited(240) {
                    return self.fail("B never verified the code");
                }
            }
            1 => match self.ed_ready(app) {
                Some(Err(code)) => return Some(code),
                Some(Ok(peer)) => {
                    self.e2dm.peer = Some(peer);
                    self.goto(2, "registered as a new computer");
                }
                None => {}
            },
            2 => {
                let peer = self.e2dm.peer?;
                if let Some(dm) = self.ed_open(app, peer) {
                    self.e2dm.dm = Some(dm);
                    self.goto(3, "reading the lines from before");
                } else if self.waited(30) {
                    return self.fail("no DM with B on the new computer");
                }
            }
            3 => {
                let dm = self.e2dm.dm?;
                let lines: Vec<DmMessage> = app.dms.logs.get(&dm).map(|l| l.msgs.clone()).unwrap_or_default();
                let sealed: Vec<&DmMessage> = lines.iter().filter(|m| e2dm::is_e2e(m)).collect();
                if sealed.len() >= 3 {
                    if let Some(m) = sealed.iter().find(|m| m.sealed.as_deref() != Some(e2dm::BEFORE_DEVICE) || m.content != e2dm::BEFORE_DEVICE_TEXT) {
                        return self.fail(&format!("line {} is {:?} \"{}\"", m.id, m.sealed, m.content));
                    }
                    if sealed.iter().any(|m| !m.attachments.is_empty()) {
                        return self.fail("an image of a line from before was shown");
                    }
                    log(&format!("{} lines from before: \"{}\"", sealed.len(), e2dm::BEFORE_DEVICE_TEXT));
                    self.ed_set("new-done.txt");
                    return self.pass();
                } else if self.waited(30) {
                    return self.fail(&format!("{} sealed lines loaded", sealed.len()));
                }
            }
            _ => {}
        }
        None
    }
}
