//! The Go Live picker (item 8c): a grid of cards, one per window ("Aplicativos") or monitor
//! ("Telas"), each with a thumbnail taken once in the background.
//!
//! * Thumbnails come from `vox_capture::ThumbnailGrabber` (one Windows.Graphics.Capture frame,
//!   scaled on the GPU) on a small pool of worker threads that lives only while the list is being
//!   filled. They arrive one by one through a channel and become egui textures on the UI thread.
//! * Nothing is refreshed on a timer: the pictures are taken when the dialog opens and when
//!   "Atualizar lista" is pressed. When every job is done no thread, timer or repaint is left.
//! * A minimized window has no picture, so the card shows the app's icon and "(minimizada)".
//! * The cache lives as long as the dialog. Closing it drops the pool (workers stop after the
//!   picture they are on) and every texture. Pictures stay in memory: never written, never logged.
//! * A worker that gets stuck (a window that never answers) cannot block the others: after
//!   `HARD_LIMIT` its card falls back to the icon and a fresh worker takes its place.
//!
//! "Só áudio" (audio-only Go Live) uses the same machinery for another list: the apps, one card
//! per process (the windows list deduplicated, see [`dedupe_apps`]) showing the app's icon,
//! plus a card for all the PC's sound. No picture is taken of anything (only icons are read).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use vox_capture::{MonitorInfo, RgbaImage, WindowInfo};

use crate::app::{Pick, RED};
use crate::forms;
use crate::theme::{self, ph, radius, size, space, stroke};
use crate::ui_kit;

/// Worker threads taking thumbnails at the same time.
const WORKERS: usize = 3;
/// Biggest thumbnail asked from the capture (16:9 card at up to 2x pixel density).
const THUMB_MAX: (u32, u32) = (320, 180);
/// How long one capture waits for the target's first frame.
const FRAME_TIMEOUT: Duration = Duration::from_millis(2500);
/// After this a worker counts as stuck: its card gets the icon and a new worker is started.
const HARD_LIMIT: Duration = Duration::from_secs(6);
/// While jobs are outstanding the UI wakes this often to look for stuck workers.
const WATCHDOG_TICK: Duration = Duration::from_millis(750);
/// Biggest icon read from a window (it is drawn at its natural size, at most this).
const ICON_MAX: u32 = 64;

const CARD_MIN_W: f32 = 200.0;
/// I6: the grid keeps at least this height in a short window (the dialog's body scrolls instead).
const GRID_MIN_H: f32 = 140.0;
/// The audio-only picker's cards are smaller (an icon, not a picture).
const AUDIO_CARD_MIN_W: f32 = 170.0;
const CARD_GAP: f32 = 10.0;
/// A card's corners (v1.0: radius 8), and its picture's inside it.
const CARD_RADIUS: u8 = 8;
const PIC_RADIUS: u8 = 5;
/// Characters of a window title kept before the layout cuts it to the card width.
const TITLE_MAX_CHARS: usize = 100;

// ------------------------------------------------------------------------------------------ pure

/// One app of the audio-only picker: a process and the first of its windows (for the icon and
/// the title).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub pid: u32,
    /// A window of the process (the first listed, i.e. the topmost), to read the icon from.
    pub hwnd: isize,
    pub exe: String,
    pub title: String,
    /// How many windows the process has in the list.
    pub windows: usize,
}

/// The windows list as an apps list: one entry per process, in the order each process first
/// appears. Windows without a process are skipped.
pub fn dedupe_apps(windows: &[WindowInfo]) -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = Vec::new();
    for w in windows {
        if w.process_id == 0 {
            continue;
        }
        match out.iter_mut().find(|a| a.pid == w.process_id) {
            Some(a) => a.windows += 1,
            None => out.push(AppEntry { pid: w.process_id, hwnd: w.hwnd, exe: w.exe_name.clone(), title: w.title.clone(), windows: 1 }),
        }
    }
    out
}

/// "Spotify.exe" -> "Spotify" (the app's name as a person would say it).
pub fn app_name(exe: &str) -> String {
    let lower = exe.to_ascii_lowercase();
    match lower.strip_suffix(".exe") {
        Some(_) if exe.len() > 4 => exe[..exe.len() - 4].to_string(),
        _ if exe.is_empty() => "—".to_string(),
        _ => exe.to_string(),
    }
}

/// Columns and card width for a grid that fills `avail` pixels: as many columns as fit cards of
/// at least `min_w`, then the cards widen to use the whole row.
pub fn grid_layout(avail: f32, min_w: f32, gap: f32) -> (usize, f32) {
    let avail = avail.max(1.0);
    let cols = (((avail + gap) / (min_w + gap)).floor() as usize).max(1);
    let w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).max(1.0);
    (cols, w)
}

/// Height of the 16:9 picture area of a card `card_w` wide.
pub fn thumb_height(card_w: f32) -> f32 {
    (card_w * 9.0 / 16.0).round()
}

/// Largest size with the aspect of `img` that fits inside `box_` (never enlarged past `img`
/// unless `allow_upscale`).
pub fn fit_in_box(img: (f32, f32), box_: (f32, f32), allow_upscale: bool) -> (f32, f32) {
    if img.0 <= 0.0 || img.1 <= 0.0 {
        return (0.0, 0.0);
    }
    let mut s = (box_.0 / img.0).min(box_.1 / img.1);
    if !allow_upscale {
        s = s.min(1.0);
    }
    ((img.0 * s).round(), (img.1 * s).round())
}

/// A window title made safe to show on one line: control characters and runs of whitespace become
/// single spaces and anything past `max_chars` characters is replaced by an ellipsis.
pub fn truncate_title(title: &str, max_chars: usize) -> String {
    let mut norm: Vec<char> = Vec::new();
    let mut pending_space = false;
    for c in title.trim().chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = true;
            continue;
        }
        if pending_space && !norm.is_empty() {
            norm.push(' ');
        }
        pending_space = false;
        norm.push(c);
    }
    if norm.len() <= max_chars {
        return norm.into_iter().collect();
    }
    let cut: String = norm[..max_chars].iter().collect();
    format!("{}…", cut.trim_end())
}

/// Thumbnails by source, for as long as the dialog is open. Generic over the stored value so the
/// lifetime rules are testable without a GPU (the app stores textures).
pub struct ThumbCache<T> {
    items: HashMap<Pick, T>,
}

impl<T> Default for ThumbCache<T> {
    fn default() -> Self {
        Self { items: HashMap::new() }
    }
}

impl<T> ThumbCache<T> {
    pub fn get(&self, k: &Pick) -> Option<&T> {
        self.items.get(k)
    }

    /// Insert or replace (the replaced value is dropped).
    pub fn insert(&mut self, k: Pick, v: T) {
        self.items.insert(k, v);
    }

    /// After a refresh: drop what is no longer listed (a closed window), keep the rest so the
    /// card does not flash empty while its new picture is on the way.
    pub fn retain_listed(&mut self, listed: &HashSet<Pick>) {
        self.items.retain(|k, _| listed.contains(k));
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

// ------------------------------------------------------------------------------------- workers

/// What a card shows.
pub enum Face {
    /// A picture of the window or screen.
    Picture(RgbaImage),
    /// No picture: the app's icon (when one could be read), and why.
    Icon { icon: Option<RgbaImage>, minimized: bool },
}

struct ThumbResult {
    key: Pick,
    face: Face,
}

#[derive(Default)]
struct Queue {
    /// Jobs not started yet.
    todo: Vec<Pick>,
    /// Jobs a worker is on: worker id -> (source, when it started).
    running: HashMap<u64, (Pick, Instant)>,
    /// Workers declared stuck; they finish whatever they are on and exit without reporting.
    abandoned: HashSet<u64>,
    /// Take screens before windows (the "Telas" tab is showing).
    monitors_first: bool,
}

struct PoolShared {
    cancel: AtomicBool,
    queue: Mutex<Queue>,
    next_worker: AtomicU64,
    tx: Mutex<Sender<ThumbResult>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// Audio-only picker: the window each app's icon is read from.
    app_hwnd: HashMap<u32, isize>,
}

/// One round of thumbnails: the jobs, the workers and the channel the pictures come back on.
/// Dropping it cancels the round.
struct ThumbPool {
    shared: Arc<PoolShared>,
    rx: Receiver<ThumbResult>,
    /// Sources whose picture has not come back yet.
    outstanding: HashSet<Pick>,
    started: Instant,
    total: usize,
    pictures: usize,
    icons: usize,
    reported: bool,
}

impl ThumbPool {
    fn start(jobs: Vec<Pick>, monitors_first: bool, app_hwnd: HashMap<u32, isize>, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = mpsc::channel();
        let outstanding: HashSet<Pick> = jobs.iter().cloned().collect();
        let total = jobs.len();
        let shared = Arc::new(PoolShared {
            cancel: AtomicBool::new(false),
            queue: Mutex::new(Queue { todo: jobs, monitors_first, ..Default::default() }),
            next_worker: AtomicU64::new(0),
            tx: Mutex::new(tx),
            wake,
            app_hwnd,
        });
        for _ in 0..WORKERS.min(total) {
            spawn_worker(&shared);
        }
        ThumbPool { shared, rx, outstanding, started: Instant::now(), total, pictures: 0, icons: 0, reported: total == 0 }
    }

    fn set_monitors_first(&self, v: bool) {
        lock(&self.shared.queue).monitors_first = v;
    }

    fn pending(&self) -> usize {
        self.outstanding.len()
    }

    /// Everything that arrived since the last call, plus a card gone to its icon for each
    /// worker that got stuck.
    fn poll(&mut self) -> Vec<ThumbResult> {
        let mut out = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            if self.outstanding.remove(&r.key) {
                match &r.face {
                    Face::Picture(_) => self.pictures += 1,
                    Face::Icon { .. } => self.icons += 1,
                }
                out.push(r);
            }
        }
        // Stuck workers.
        let stuck: Vec<(u64, Pick)> = {
            let q = lock(&self.shared.queue);
            q.running.iter().filter(|(_, (_, t))| t.elapsed() > HARD_LIMIT).map(|(id, (k, _))| (*id, k.clone())).collect()
        };
        for (id, key) in stuck {
            {
                let mut q = lock(&self.shared.queue);
                q.running.remove(&id);
                q.abandoned.insert(id);
            }
            tracing::warn!("a Go Live thumbnail got stuck; using the icon and starting a new worker");
            if self.outstanding.remove(&key) {
                self.icons += 1;
                out.push(ThumbResult { key, face: Face::Icon { icon: None, minimized: false } });
            }
            if !lock(&self.shared.queue).todo.is_empty() {
                spawn_worker(&self.shared);
            }
        }
        if self.outstanding.is_empty() && !self.reported {
            self.reported = true;
            // Counts and time only: never what the windows are.
            tracing::info!(
                targets = self.total,
                pictures = self.pictures,
                icons = self.icons,
                ms = self.started.elapsed().as_millis() as u64,
                "Go Live thumbnails ready"
            );
        }
        out
    }
}

impl Drop for ThumbPool {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        lock(&self.shared.queue).todo.clear();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn spawn_worker(shared: &Arc<PoolShared>) {
    let id = shared.next_worker.fetch_add(1, Ordering::Relaxed);
    let sh = shared.clone();
    if let Err(e) = std::thread::Builder::new().name("vox-thumb".into()).spawn(move || worker(sh, id)) {
        tracing::warn!(error = %e, "could not start a thumbnail worker");
    }
}

/// Next job: from the tab that is showing first.
fn take_job(q: &mut Queue) -> Option<Pick> {
    let want_monitor = q.monitors_first;
    let i = q.todo.iter().position(|k| matches!(k, Pick::Monitor(_)) == want_monitor).or(if q.todo.is_empty() { None } else { Some(0) })?;
    Some(q.todo.remove(i))
}

fn worker(shared: Arc<PoolShared>, id: u64) {
    // One capture device per worker, made on first use and reused for every picture.
    let mut grabber: Option<vox_capture::ThumbnailGrabber> = None;
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            break;
        }
        let key = {
            let mut q = lock(&shared.queue);
            if q.abandoned.contains(&id) {
                break;
            }
            let Some(k) = take_job(&mut q) else { break };
            q.running.insert(id, (k.clone(), Instant::now()));
            k
        };
        let face = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| make_face(&mut grabber, &key, &shared.app_hwnd)))
            .unwrap_or(Face::Icon { icon: None, minimized: false });
        let still_mine = {
            let mut q = lock(&shared.queue);
            let mine = q.running.remove(&id).is_some() && !q.abandoned.contains(&id);
            if !mine {
                q.abandoned.remove(&id);
            }
            mine
        };
        if !still_mine || shared.cancel.load(Ordering::Relaxed) {
            break; // declared stuck meanwhile (its card was already handled) or dialog closed
        }
        if lock(&shared.tx).send(ThumbResult { key, face }).is_err() {
            break;
        }
        (shared.wake)();
    }
}

fn make_face(grabber: &mut Option<vox_capture::ThumbnailGrabber>, key: &Pick, app_hwnd: &HashMap<u32, isize>) -> Face {
    use vox_capture::VideoTarget;
    let (target, hwnd) = match key {
        Pick::Window(h) => (VideoTarget::Window(*h), Some(*h)),
        Pick::Monitor(i) => (VideoTarget::Monitor(*i), None),
        // Audio-only picker: only the app's icon, no capture of anything.
        Pick::App(pid) => {
            let icon = app_hwnd.get(pid).and_then(|h| vox_capture::window_icon(*h, ICON_MAX).ok());
            return Face::Icon { icon, minimized: false };
        }
        Pick::AllSound => return Face::Icon { icon: None, minimized: false },
    };
    let icon = |h: isize| vox_capture::window_icon(h, ICON_MAX).ok();
    if let Some(h) = hwnd {
        if vox_capture::is_window_minimized(h) {
            return Face::Icon { icon: icon(h), minimized: true };
        }
    }
    if grabber.is_none() {
        *grabber = vox_capture::ThumbnailGrabber::new()
            .map(|mut g| {
                g.set_timeout(FRAME_TIMEOUT);
                g
            })
            .map_err(|e| tracing::warn!(error = %e, "thumbnail capture unavailable"))
            .ok();
    }
    match grabber.as_mut().map(|g| g.grab(target, THUMB_MAX.0, THUMB_MAX.1)) {
        Some(Ok(img)) => Face::Picture(img),
        // Minimized between the check and the capture counts as minimized.
        _ => match hwnd {
            Some(h) => Face::Icon { icon: icon(h), minimized: vox_capture::is_window_minimized(h) },
            None => Face::Icon { icon: None, minimized: false },
        },
    }
}

// --------------------------------------------------------------------------------------- the UI

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Apps,
    Screens,
}

/// What one card has on screen.
struct Card {
    tex: TextureHandle,
    /// A picture of the window / screen (fills the card), or an icon (centered on a neutral card).
    icon: bool,
    minimized: bool,
}

/// State of the picker dialog: the active tab, the running round of thumbnails and the cache.
/// Everything here is released by [`Picker::release`] when the dialog closes.
#[derive(Default)]
pub struct Picker {
    pub tab: Tab,
    pool: Option<ThumbPool>,
    cards: ThumbCache<Card>,
}

impl Picker {
    /// The lists came in (dialog opened or "Atualizar lista"): drop cards of sources that are
    /// gone and take new pictures of the rest.
    ///
    /// `audio`: the "Só áudio" picker, whose cards are apps (icons only) instead of pictures of
    /// windows and screens. Changing mode calls this again, which frees the other mode's cards.
    pub fn sources_listed(&mut self, windows: &[WindowInfo], monitors: &[MonitorInfo], wake: Arc<dyn Fn() + Send + Sync>, audio: bool) {
        let mut app_hwnd = HashMap::new();
        let jobs: Vec<Pick> = if audio {
            let apps = dedupe_apps(windows);
            app_hwnd = apps.iter().map(|a| (a.pid, a.hwnd)).collect();
            apps.iter().map(|a| Pick::App(a.pid)).collect()
        } else {
            let mut jobs: Vec<Pick> = windows.iter().map(|w| Pick::Window(w.hwnd)).collect();
            jobs.extend(monitors.iter().map(|m| Pick::Monitor(m.index)));
            jobs
        };
        self.cards.retain_listed(&jobs.iter().cloned().collect());
        self.pool = Some(ThumbPool::start(jobs, self.tab == Tab::Screens, app_hwnd, wake));
    }

    /// The dialog closed: stop the workers and free every picture (and its texture).
    pub fn release(&mut self) {
        self.pool = None;
        self.cards.clear();
    }

    /// Nothing held (used to make `release` free to call every frame).
    pub fn is_idle(&self) -> bool {
        self.pool.is_none() && self.cards.len() == 0
    }

    /// Turn finished pictures into textures. Called once per frame while the dialog is open.
    fn collect(&mut self, ctx: &egui::Context) {
        let Some(pool) = self.pool.as_mut() else { return };
        pool.set_monitors_first(self.tab == Tab::Screens);
        for r in pool.poll() {
            let (img, icon, minimized) = match r.face {
                Face::Picture(img) => (Some(img), false, false),
                Face::Icon { icon, minimized } => (icon, true, minimized),
            };
            let Some(img) = img else {
                // No picture and no icon: an empty neutral card with its reason.
                self.cards.insert(r.key.clone(), Card { tex: blank_texture(ctx, &r.key), icon: true, minimized });
                continue;
            };
            let tex = ctx.load_texture(
                format!("golive-thumb-{:?}", r.key),
                egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.data),
                TextureOptions::LINEAR,
            );
            self.cards.insert(r.key, Card { tex, icon, minimized });
        }
        if pool.pending() > 0 {
            // Look again soon for a stuck worker; stops by itself once everything is in.
            ctx.request_repaint_after(WATCHDOG_TICK);
        } else {
            self.pool = None;
        }
    }
}

fn blank_texture(ctx: &egui::Context, key: &Pick) -> TextureHandle {
    ctx.load_texture(format!("golive-thumb-{key:?}"), egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 0]), TextureOptions::NEAREST)
}

/// What the grid tells the dialog.
#[derive(Default)]
pub struct GridOutcome {
    /// A card was clicked.
    pub picked: Option<Pick>,
    /// A card was double-clicked: pick it and go.
    pub go: bool,
}

pub struct GridInput<'a> {
    pub windows: &'a [WindowInfo],
    pub monitors: &'a [MonitorInfo],
    pub loading: bool,
    pub picked: Option<&'a Pick>,
    /// The source the live is on now ("Trocar tela").
    pub current: Option<&'a Pick>,
    pub max_height: f32,
    /// "Só áudio": the apps list (plus "Todo o som do PC") instead of the windows and screens.
    pub audio: bool,
}

/// "Carregando..." / "Nenhuma janela" under the tabs.
fn quiet(ui: &mut egui::Ui, text: &str) {
    ui.add_space(space::L);
    ui.label(egui::RichText::new(text).color(theme::NEVOA));
}

/// The audio-only picker: a card for all the PC's sound, then one per app.
fn show_audio(ui: &mut egui::Ui, picker: &mut Picker, inp: GridInput<'_>) -> GridOutcome {
    let mut out = GridOutcome::default();
    let apps = dedupe_apps(inp.windows);
    ui.horizontal(|ui| {
        ui_kit::section(ui, "Apps com janela aberta");
        ui.label(egui::RichText::new(format!("{}", apps.len())).font(theme::body(size::SMALL)).color(theme::NEVOA));
    });
    ui.add_space(space::S);
    egui::ScrollArea::vertical().max_height(inp.max_height).min_scrolled_height(inp.max_height.min(GRID_MIN_H)).auto_shrink([false, true]).show(ui, |ui| {
        let (cols, card_w) = grid_layout(ui.available_width() - 12.0, AUDIO_CARD_MIN_W, CARD_GAP);
        ui.spacing_mut().item_spacing = Vec2::splat(CARD_GAP);
        let mut items: Vec<(Pick, String, String)> =
            vec![(Pick::AllSound, "Todo o som do PC".to_string(), "menos o concord (a voz da chamada não volta)".to_string())];
        for a in &apps {
            let sub = match (a.windows, truncate_title(&a.title, TITLE_MAX_CHARS)) {
                (1, t) => t,
                (n, t) => format!("{n} janelas · {t}"),
            };
            items.push((Pick::App(a.pid), app_name(&a.exe), sub));
        }
        if inp.loading && apps.is_empty() {
            quiet(ui, "Procurando janelas...");
        }
        for row in items.chunks(cols) {
            ui.horizontal(|ui| {
                for (key, title, sub) in row {
                    let resp = app_card(ui, card_w, title, sub, key, picker.cards.get(key), inp.picked == Some(key), inp.current == Some(key));
                    if resp.clicked() || resp.double_clicked() {
                        out.picked = Some(key.clone());
                    }
                    if resp.double_clicked() {
                        out.go = true;
                    }
                }
            });
        }
    });
    out
}

/// A card's frame: [`theme::MOVEL`] under the pointer, the chosen one outlined in chalk (2 pt) on
/// [`theme::MOVEL`]. The lamp stays out of it.
fn paint_card_frame(painter: &egui::Painter, rect: Rect, hovered: bool, selected: bool) {
    let (fill, line) = if selected {
        (theme::MOVEL, Stroke::new(2.0, theme::GIZ))
    } else if hovered {
        (theme::MOVEL, Stroke::new(stroke::HAIRLINE, theme::RISCO))
    } else {
        (Color32::TRANSPARENT, Stroke::new(stroke::HAIRLINE, theme::RISCO))
    };
    painter.rect(rect, CARD_RADIUS, fill, line, egui::StrokeKind::Inside);
}

/// The "transmitindo agora" mark in a card's picture.
fn paint_current(painter: &egui::Painter, pic: Rect) {
    let galley = painter.layout_no_wrap("transmitindo agora".into(), theme::semibold(10.5), Color32::WHITE);
    let badge = Rect::from_min_size(pic.min + Vec2::new(6.0, 6.0), galley.size() + Vec2::new(10.0, 4.0));
    painter.rect_filled(badge, radius::TAG, RED);
    painter.galley(badge.min + Vec2::new(5.0, 2.0), galley, Color32::WHITE);
}

/// A card's title and its quiet second line, each cut to the card's width.
fn paint_texts(painter: &egui::Painter, rect: Rect, top: f32, title: &str, sub: &str, selected: bool) {
    let w = rect.width() - 16.0;
    for (text, font, color, dy) in [
        (title, theme::semibold(13.0), if selected { theme::GIZ } else { theme::TEXTO }, 0.0),
        (sub, theme::body(11.5), theme::NEVOA, 17.0),
    ] {
        let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(font, color));
        job.wrap.max_width = w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        let g = painter.layout_job(job);
        painter.galley(egui::pos2(rect.min.x + 8.0, top + dy), g, color);
    }
}

/// A card of the audio-only picker: the app's icon (or the speaker for "all sound"), its name and
/// its first window's title.
#[allow(clippy::too_many_arguments)]
fn app_card(ui: &mut egui::Ui, w: f32, title: &str, sub: &str, key: &Pick, face: Option<&Card>, selected: bool, current: bool) -> egui::Response {
    let pic_h = 68.0;
    let text_h = 38.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, pic_h + text_h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint_card_frame(p, rect, resp.hovered(), selected);
        let pic = Rect::from_min_size(rect.min + Vec2::splat(5.0), Vec2::new(w - 10.0, pic_h - 10.0));
        p.rect_filled(pic, PIC_RADIUS, theme::FUNDO);
        match (key, face) {
            (Pick::AllSound, _) => {
                p.text(pic.center(), Align2::CENTER_CENTER, ph::SPEAKER_HIGH, theme::body(30.0), theme::GIZ);
            }
            (_, Some(c)) if c.tex.size_vec2().x > 1.0 => {
                let s = c.tex.size_vec2();
                let (iw, ih) = fit_in_box((s.x, s.y), (40.0, 40.0), true);
                let img = Rect::from_center_size(pic.center(), Vec2::new(iw, ih));
                p.image(c.tex.id(), img, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            (_, Some(_)) => {
                // No icon could be read: a note instead.
                p.text(pic.center(), Align2::CENTER_CENTER, ph::MUSIC_NOTES, theme::body(26.0), theme::NEVOA);
            }
            (_, None) => {
                p.text(pic.center(), Align2::CENTER_CENTER, "...", theme::body(14.0), theme::NEVOA);
            }
        }
        if current {
            paint_current(p, pic);
        }
        paint_texts(p, rect, rect.min.y + pic_h, title, sub, selected);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, title));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{title}\n{sub}"))
}

/// Tabs plus the grid of the active tab.
pub fn show(ui: &mut egui::Ui, picker: &mut Picker, inp: GridInput<'_>) -> GridOutcome {
    picker.collect(ui.ctx());
    if inp.audio {
        return show_audio(ui, picker, inp);
    }
    let apps = format!("Aplicativos ({})", inp.windows.len());
    let screens = format!("Telas ({})", inp.monitors.len());
    forms::segmented(ui, &mut picker.tab, &[(Tab::Apps, apps.as_str()), (Tab::Screens, screens.as_str())]);
    ui.add_space(space::S);

    let mut out = GridOutcome::default();
    let n = match picker.tab {
        Tab::Apps => inp.windows.len(),
        Tab::Screens => inp.monitors.len(),
    };
    if n == 0 {
        quiet(
            ui,
            match (picker.tab, inp.loading) {
                (_, true) => "Procurando janelas...",
                (Tab::Apps, false) => "Nenhuma janela encontrada.",
                (Tab::Screens, false) => "Nenhuma tela encontrada.",
            },
        );
        return out;
    }
    egui::ScrollArea::vertical().max_height(inp.max_height).min_scrolled_height(inp.max_height.min(GRID_MIN_H)).auto_shrink([false, true]).show(ui, |ui| {
        let (cols, card_w) = grid_layout(ui.available_width() - 12.0, CARD_MIN_W, CARD_GAP);
        ui.spacing_mut().item_spacing = Vec2::splat(CARD_GAP);
        let items: Vec<(Pick, String, String)> = match picker.tab {
            Tab::Apps => inp
                .windows
                .iter()
                .map(|w| (Pick::Window(w.hwnd), truncate_title(&w.title, TITLE_MAX_CHARS), if w.exe_name.is_empty() { "—".to_string() } else { w.exe_name.clone() }))
                .collect(),
            Tab::Screens => inp
                .monitors
                .iter()
                .map(|m| {
                    (
                        Pick::Monitor(m.index),
                        format!("Tela {}{}", m.index + 1, if m.primary { " (principal)" } else { "" }),
                        format!("{}x{} · {}", m.width, m.height, m.name),
                    )
                })
                .collect(),
        };
        for row in items.chunks(cols) {
            ui.horizontal(|ui| {
                for (key, title, sub) in row {
                    let resp = card(ui, card_w, title, sub, picker.cards.get(key), inp.picked == Some(key), inp.current == Some(key));
                    if resp.clicked() || resp.double_clicked() {
                        out.picked = Some(key.clone());
                    }
                    if resp.double_clicked() {
                        out.go = true;
                    }
                }
            });
        }
    });
    out
}

/// A thumbnail card (radius 8): the picture of the window or screen (or its icon and why there is
/// none), its title and the app's file or the screen's size.
#[allow(clippy::too_many_arguments)]
fn card(ui: &mut egui::Ui, w: f32, title: &str, sub: &str, face: Option<&Card>, selected: bool, current: bool) -> egui::Response {
    let th = thumb_height(w);
    let text_h = 38.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, th + text_h), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint_card_frame(p, rect, resp.hovered(), selected);
        let pic = Rect::from_min_size(rect.min + Vec2::splat(5.0), Vec2::new(w - 10.0, th - 10.0));
        p.rect_filled(pic, PIC_RADIUS, theme::FUNDO);
        match face {
            None => {
                p.text(pic.center(), Align2::CENTER_CENTER, "Carregando...", theme::body(12.0), theme::NEVOA);
            }
            Some(c) if !c.icon => {
                let s = c.tex.size_vec2();
                let (iw, ih) = fit_in_box((s.x, s.y), (pic.width(), pic.height()), true);
                let img = Rect::from_center_size(pic.center(), Vec2::new(iw, ih));
                // The picture with the card's rounded corners (a mesh: no extra texture or pass).
                p.add(egui::epaint::RectShape::filled(img, PIC_RADIUS, Color32::WHITE).with_texture(c.tex.id(), Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))));
            }
            Some(c) => {
                // A quiet card with the icon in the middle and why there is no picture.
                let s = c.tex.size_vec2();
                if s.x > 1.0 {
                    let (iw, ih) = fit_in_box((s.x, s.y), (44.0, 44.0), true);
                    let img = Rect::from_center_size(pic.center() - Vec2::new(0.0, 8.0), Vec2::new(iw, ih));
                    p.image(c.tex.id(), img, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                let why = if c.minimized { "(minimizada)" } else { "(sem prévia)" };
                p.text(pic.center() + Vec2::new(0.0, 28.0), Align2::CENTER_CENTER, why, theme::body(11.5), theme::NEVOA);
            }
        }
        if current {
            paint_current(p, pic);
        }
        paint_texts(p, rect, rect.min.y + th, title, sub, selected);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, title));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{title}\n{sub}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn grid_fills_the_row() {
        // 740 px: three cards of at least 200 (+10 gaps) fit; they widen to 240.
        let (cols, w) = grid_layout(740.0, 200.0, 10.0);
        assert_eq!(cols, 3);
        assert!((w - 240.0).abs() < 0.01, "{w}");
        // The row never overflows: cols * w + gaps == avail.
        for avail in [150.0, 199.0, 200.0, 421.0, 620.0, 1000.0, 1900.0] {
            let (c, w) = grid_layout(avail, 200.0, 10.0);
            assert!(c >= 1);
            if avail >= 200.0 {
                assert!(w >= 200.0 - 0.01, "avail {avail}: card {w}");
                assert!((c as f32 * w + 10.0 * (c as f32 - 1.0) - avail).abs() < 0.01);
            }
        }
        // Too narrow for one card: still one column, never zero.
        assert_eq!(grid_layout(120.0, 200.0, 10.0).0, 1);
        assert_eq!(grid_layout(0.0, 200.0, 10.0).0, 1);
    }

    #[test]
    fn thumbnails_are_sixteen_by_nine() {
        assert_eq!(thumb_height(240.0), 135.0);
        assert_eq!(thumb_height(320.0), 180.0);
        // A 4:3 window in a 16:9 box is centered with side bars; a tall one gets narrow.
        assert_eq!(fit_in_box((400.0, 300.0), (200.0, 112.0), false), (149.0, 112.0));
        assert_eq!(fit_in_box((100.0, 400.0), (200.0, 112.0), false), (28.0, 112.0));
        // A small picture is not enlarged unless asked.
        assert_eq!(fit_in_box((64.0, 64.0), (200.0, 112.0), false), (64.0, 64.0));
        assert_eq!(fit_in_box((64.0, 64.0), (200.0, 112.0), true), (112.0, 112.0));
        assert_eq!(fit_in_box((0.0, 10.0), (200.0, 112.0), true), (0.0, 0.0));
    }

    #[test]
    fn titles_are_cut_cleanly() {
        assert_eq!(truncate_title("Hello", 10), "Hello");
        assert_eq!(truncate_title("  spaced \t out\r\ntitle ", 50), "spaced out title");
        assert_eq!(truncate_title("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate_title("abcdefghijk", 10), "abcdefghij…");
        // Counts characters, not bytes: never splits a multi-byte one.
        assert_eq!(truncate_title("ação ação ação", 6), "ação a…");
        assert_eq!(truncate_title("日本語のタイトル", 3), "日本語…");
        assert_eq!(truncate_title("", 5), "");
        assert_eq!(truncate_title("   \n ", 5), "");
        // Cutting at a word boundary leaves no dangling space before the ellipsis.
        assert_eq!(truncate_title("ab      cd", 3), "ab…");
        assert_eq!(truncate_title("ab      cd", 2), "ab…");
    }

    /// Counts how many values are still alive.
    struct Probe(Rc<std::cell::Cell<usize>>);
    impl Probe {
        fn new(c: &Rc<std::cell::Cell<usize>>) -> Self {
            c.set(c.get() + 1);
            Probe(c.clone())
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    #[test]
    fn cache_lives_with_the_dialog() {
        let alive = Rc::new(std::cell::Cell::new(0));
        let mut c: ThumbCache<Probe> = ThumbCache::default();
        c.insert(Pick::Window(1), Probe::new(&alive));
        c.insert(Pick::Window(2), Probe::new(&alive));
        c.insert(Pick::Monitor(0), Probe::new(&alive));
        assert_eq!((c.len(), alive.get()), (3, 3));

        // A new picture replaces (and frees) the old one.
        c.insert(Pick::Window(2), Probe::new(&alive));
        assert_eq!((c.len(), alive.get()), (3, 3));

        // Refresh: window 1 closed -> its picture is freed at once, the others stay.
        let listed: HashSet<Pick> = [Pick::Window(2), Pick::Monitor(0)].into_iter().collect();
        c.retain_listed(&listed);
        assert_eq!((c.len(), alive.get()), (2, 2));
        assert!(c.get(&Pick::Window(1)).is_none() && c.get(&Pick::Window(2)).is_some());

        // Dialog closed: everything freed.
        c.clear();
        assert_eq!((c.len(), alive.get()), (0, 0));
    }

    #[test]
    fn queue_serves_the_showing_tab_first() {
        let mut q = Queue { todo: vec![Pick::Window(1), Pick::Window(2), Pick::Monitor(0), Pick::Window(3)], ..Default::default() };
        assert_eq!(take_job(&mut q), Some(Pick::Window(1)));
        q.monitors_first = true;
        assert_eq!(take_job(&mut q), Some(Pick::Monitor(0)));
        // No screens left: windows go on in order.
        assert_eq!(take_job(&mut q), Some(Pick::Window(2)));
        assert_eq!(take_job(&mut q), Some(Pick::Window(3)));
        assert_eq!(take_job(&mut q), None);
    }

    fn win(hwnd: isize, pid: u32, exe: &str, title: &str) -> WindowInfo {
        WindowInfo { hwnd, title: title.into(), process_id: pid, exe_name: exe.into() }
    }

    #[test]
    fn apps_are_one_card_per_process() {
        let list = vec![
            win(1, 10, "chrome.exe", "Mix — YouTube"),
            win(2, 20, "Spotify.exe", "Artista - Música"),
            win(3, 10, "chrome.exe", "Outra aba"),
            win(4, 0, "x.exe", "sem processo"),
            win(5, 30, "", "sem nome"),
        ];
        let apps = dedupe_apps(&list);
        assert_eq!(apps.iter().map(|a| (a.pid, a.hwnd, a.windows)).collect::<Vec<_>>(), vec![(10, 1, 2), (20, 2, 1), (30, 5, 1)]);
        // The first (topmost) window of the process names the card.
        assert_eq!(apps[0].title, "Mix — YouTube");
        assert!(dedupe_apps(&[]).is_empty());
    }

    #[test]
    fn app_names_drop_the_exe_suffix() {
        assert_eq!(app_name("Spotify.exe"), "Spotify");
        assert_eq!(app_name("chrome.EXE"), "chrome");
        assert_eq!(app_name("tool"), "tool");
        assert_eq!(app_name(".exe"), ".exe");
        assert_eq!(app_name(""), "—");
    }

    #[test]
    fn audio_cards_get_icons_only() {
        // The worker never captures for an app card: unknown windows just have no icon.
        let mut g = None;
        let mut hw = HashMap::new();
        assert!(matches!(make_face(&mut g, &Pick::AllSound, &hw), Face::Icon { icon: None, minimized: false }));
        assert!(matches!(make_face(&mut g, &Pick::App(7), &hw), Face::Icon { icon: None, minimized: false }));
        hw.insert(7, 0x7fff_0003);
        assert!(matches!(make_face(&mut g, &Pick::App(7), &hw), Face::Icon { icon: None, minimized: false }));
        assert!(g.is_none(), "no capture device is made for icons");
    }

    #[test]
    fn pool_is_idle_after_release_and_cancel_stops_workers() {
        let mut p = Picker::default();
        assert!(p.is_idle());
        // A round over sources that do not exist ends with an icon for each, with no GPU needed.
        let woke = Arc::new(AtomicU64::new(0));
        let w2 = woke.clone();
        let mut pool = ThumbPool::start(vec![Pick::Window(0x7fff_0001), Pick::Window(0x7fff_0002)], false, HashMap::new(), Arc::new(move || {
            w2.fetch_add(1, Ordering::Relaxed);
        }));
        let t = Instant::now();
        let mut got = 0;
        while got < 2 && t.elapsed() < Duration::from_secs(10) {
            got += pool.poll().len();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got, 2, "both jobs report back");
        assert_eq!(pool.pending(), 0);
        assert!(woke.load(Ordering::Relaxed) >= 2, "the UI was woken for each picture");
        drop(pool);
        p.release();
        assert!(p.is_idle());
    }
}
