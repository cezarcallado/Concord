//! Mentions, replies and reactions in the chat (v0.8; the wire contract is `vox_proto::chat`):
//!
//! - **Menções**: typing `@` in a channel's input opens a list of the server's members (display
//!   name and username, filtered as you type; arrows / Tab / Enter pick, Esc closes). A message that
//!   mentions me has a highlighted background, the names show as chips, and it notifies with its
//!   own sound and an `@` on the channel's counter (`notify.rs`). `@todos` is offered to
//!   Moderador and above only (the hub enforces it). DMs have no list: a `@name` of one of the two
//!   people just shows as a chip.
//! - **Responder**: on a message's hover bar or right-click menu. The input shows "Respondendo a
//!   <nome>" with "@ ligado" (also mention them; channels) and ✖. The reply shows a one-line quote
//!   above it; a click scrolls to the original and flashes it (older pages are loaded when needed).
//! - **Reações**: "😀" on the hover bar (or "Reagir" in the menu) opens a small picker: the emoji I
//!   used last plus a fixed grid of common ones (every one has a colour Twemoji picture,
//!   `theme::emoji`; the emoji font is the fallback for anything else).
//!   Reactions show as chips under the message, highlighted when I am in them; a click toggles,
//!   hovering lists who reacted.
//!
//! - **Apagar mensagem** (v0.8.2): my own messages (channels and DMs), and anybody's in a channel
//!   for Moderador and above. In the right-click menu and as 🗑 on the hover bar; it asks "Apagar
//!   esta mensagem?" first, Shift+click deletes at once (like Discord).
//!
//! Light on the frame: the chip layout of a message (where its mentions are) is computed once and
//! cached, everything else is a few comparisons per visible message. A child module of `app`.
//!
//! Nothing here trusts a message id alone (v0.8.2): a hub up to v0.8.1 hands a deleted message's
//! id to the next message. Cached chip layouts are checked against the text, history pages replace
//! what they cover ([`merge_page`]), and a deleted message or channel takes its cached state with
//! it ([`App::forget_message`], [`App::forget_channel`]).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use vox_proto::{find_mention, Attachment, ChannelId, ChatFeatures, Reaction, ReplyRef, User, UserId, MENTIONS_MAX, MENTION_EVERYONE};

use super::App;
use crate::backend::Cmd;
use crate::theme::{self, ph};
pub use crate::chat_images::Target;

/// The curated reaction grid (every one of them is in egui's bundled emoji font and has a colour
/// picture in `theme::emoji`'s atlas; a test checks both stay true).
pub const REACTION_GRID: [&str; 40] = [
    "👍", "👎", "❤", "😂", "😮", "😢", "😡", "🎉", "🔥", "👏", //
    "🙏", "😀", "😄", "😁", "😆", "😅", "😊", "😉", "😍", "😘", //
    "😎", "😐", "😏", "😬", "😴", "😭", "😱", "😳", "💪", "👀", //
    "👌", "✌", "👋", "💯", "✅", "❌", "⭐", "🚀", "💀", "🏆",
];
/// How many recently used emoji the picker remembers.
pub const RECENT_MAX: usize = 8;
/// Entries in the mention list.
const LIST_MAX: usize = 8;
/// A clicked quote flashes the original this long.
const FLASH: Duration = Duration::from_millis(1800);
/// At most this many older pages are loaded to find a quoted message.
const SEEK_PAGES: u32 = 10;
/// The chip-layout cache is dropped when it grows past this (it refills from what is on screen).
const SPAN_CACHE_MAX: usize = 3000;
/// Background of a message that mentions me (the lamp's light on the wall), and its left bar.
const MENTION_BG: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x22);
const MENTION_BAR: Color32 = crate::theme::ABAJUR;
/// A chip's colours: a mention of me in the lamp's colours, of somebody else quiet.
const CHIP_BG: Color32 = Color32::from_rgb(0x22, 0x33, 0x48);
const CHIP_TEXT: Color32 = crate::theme::LINK;
const CHIP_ME_BG: Color32 = crate::theme::ABAJUR_FUNDO;
const CHIP_ME_TEXT: Color32 = crate::theme::ABAJUR;

// ---------------------------------------------------------------- pure rules (unit tested)

/// What a chip stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    User(UserId),
    Everyone,
}

/// A mention chip inside a message's text (byte range of `@name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub chip: Chip,
}

/// Where the chips of a text are: every `@name` of `people` (id, username, display name) and, with
/// `everyone`, `@todos`. Overlaps keep the longer one.
pub fn mention_spans(text: &str, people: &[(UserId, &str, Option<&str>)], everyone: bool) -> Vec<Span> {
    let mut all: Vec<Span> = Vec::new();
    for &(id, username, display) in people {
        for name in [Some(username), display].into_iter().flatten() {
            all.extend(find_mention(text, name).into_iter().map(|range| Span { range, chip: Chip::User(id) }));
        }
    }
    if everyone {
        all.extend(find_mention(text, MENTION_EVERYONE).into_iter().map(|range| Span { range, chip: Chip::Everyone }));
    }
    all.sort_by(|a, b| a.range.start.cmp(&b.range.start).then(b.range.end.cmp(&a.range.end)));
    let mut out: Vec<Span> = Vec::new();
    for s in all {
        if out.last().is_none_or(|l| s.range.start >= l.range.end) {
            out.push(s);
        }
    }
    out
}

/// The `@query` being typed at `cursor` (a byte offset): the byte offset of its `@` and the query.
pub fn mention_query(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let ok_start = before[..at].chars().next_back().is_none_or(|c| c.is_whitespace() || "([{\"'".contains(c));
    let query = &before[at + 1..];
    // A query with a space only lives on while a display name could still match (checked by the
    // caller: no candidates, no list); a line break or a long run ends it.
    (ok_start && !query.contains('\n') && query.chars().count() <= vox_proto::DISPLAY_NAME_MAX).then_some((at, query))
}

/// One entry of the mention list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// `None` = `@todos`.
    pub id: Option<UserId>,
    /// What gets inserted after the `@`.
    pub name: String,
    /// The username, shown beside a display name.
    pub username: String,
}

/// The mention list for `query`: people whose display name or username starts with it first,
/// then those containing it; `@todos` first when allowed and it fits.
pub fn candidates(people: &[User], me: Option<UserId>, query: &str, can_everyone: bool) -> Vec<Candidate> {
    let q = query.to_lowercase();
    let mut starts: Vec<&User> = Vec::new();
    let mut contains: Vec<&User> = Vec::new();
    for u in people.iter().filter(|u| Some(u.id) != me) {
        let (shown, user) = (u.shown_name().to_lowercase(), u.username.to_lowercase());
        if shown.starts_with(&q) || user.starts_with(&q) {
            starts.push(u);
        } else if !q.contains(' ') && (shown.contains(&q) || user.contains(&q)) {
            contains.push(u);
        }
    }
    let by_name = |a: &&User, b: &&User| a.shown_name().to_lowercase().cmp(&b.shown_name().to_lowercase());
    starts.sort_by(by_name);
    contains.sort_by(by_name);
    let mut out = Vec::new();
    if can_everyone && MENTION_EVERYONE.starts_with(&q) {
        out.push(Candidate { id: None, name: MENTION_EVERYONE.to_string(), username: String::new() });
    }
    for u in starts.into_iter().chain(contains) {
        if out.len() >= LIST_MAX {
            break;
        }
        out.push(Candidate { id: Some(u.id), name: u.shown_name().to_string(), username: u.username.clone() });
    }
    out
}

/// `text` with the `@query` that starts at `at` and ends at `cursor` replaced by `@name `; the
/// new cursor (byte offset) is after the space.
pub fn complete(text: &str, at: usize, cursor: usize, name: &str) -> (String, usize) {
    let insert = format!("@{name} ");
    let rest = text.get(cursor..).unwrap_or("");
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let out = format!("{}{insert}{rest}", &text[..at]);
    (out, at + insert.len())
}

/// The ids a message being sent mentions: the people picked from the list whose `@name` is still
/// in the text, plus anybody of `people` written as `@username` / `@display name` by hand. Never
/// me; at most [`MENTIONS_MAX`].
pub fn send_mentions(text: &str, people: &[User], picks: &[(UserId, String)], me: Option<UserId>) -> Vec<UserId> {
    let mut out: Vec<UserId> = Vec::new();
    let mut add = |id: UserId| {
        if Some(id) != me && !out.contains(&id) && out.len() < MENTIONS_MAX {
            out.push(id);
        }
    };
    for (id, name) in picks {
        if !find_mention(text, name).is_empty() {
            add(*id);
        }
    }
    if text.contains('@') {
        for u in people {
            if !find_mention(text, &u.username).is_empty() || (u.has_display_name() && !find_mention(text, u.shown_name()).is_empty()) {
                add(u.id);
            }
        }
    }
    out
}

/// Merge a history page into a conversation's messages (ordered by id). The page is the hub's
/// truth for the ids it covers: from its oldest message (or the very beginning when `complete`,
/// the last page) up to `before` (exclusive), or up to its newest message for the newest page
/// (`before` = `None`: what arrived live after the page was read stays). Messages in that range
/// the page does not have were deleted meanwhile and go (`deleted`); a message the page has is
/// replaced by the page's (fresher) copy, and when it is not even the same message (`same`: an
/// older hub reused the id) the local one is reported in `replaced`.
pub fn merge_page<T: Clone>(msgs: &mut Vec<T>, mut page: Vec<T>, before: Option<i64>, complete: bool, id: impl Fn(&T) -> i64, same: impl Fn(&T, &T) -> bool) -> Merged<T> {
    page.sort_by_key(&id);
    page.dedup_by_key(|m| id(m));
    let lo = if complete { i64::MIN } else { page.first().map(&id).unwrap_or(i64::MAX) };
    let hi = match before {
        Some(b) => b,
        // The newest page: up to its newest message (an empty, complete one: everything is gone).
        None => page.last().map(|m| id(m).saturating_add(1)).unwrap_or(if complete { i64::MAX } else { i64::MIN }),
    };
    let mut out = Merged { deleted: Vec::new(), replaced: Vec::new() };
    msgs.retain(|m| {
        let i = id(m);
        if i < lo || i >= hi {
            return true;
        }
        match page.binary_search_by_key(&i, &id) {
            Ok(p) if same(&page[p], m) => true,
            Ok(_) => {
                out.replaced.push(m.clone());
                false
            }
            Err(_) => {
                out.deleted.push(m.clone());
                false
            }
        }
    });
    for m in page {
        match msgs.binary_search_by_key(&id(&m), &id) {
            Ok(i) => msgs[i] = m,
            Err(i) => msgs.insert(i, m),
        }
    }
    out
}

/// What [`merge_page`] took out.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged<T> {
    pub deleted: Vec<T>,
    pub replaced: Vec<T>,
}

/// What a quote line says: who, and the text (or "imagem" / "mensagem apagada").
pub fn quote_parts(r: &ReplyRef) -> (Option<String>, String) {
    if r.deleted || r.author.is_none() {
        return (None, "mensagem apagada".into());
    }
    let who = r.author.as_ref().map(|u| u.shown_name().to_string());
    let body = match (r.excerpt.is_empty(), r.images) {
        (false, _) => r.excerpt.clone(),
        (true, 0) => String::new(),
        (true, 1) => "imagem".into(),
        (true, n) => format!("{n} imagens"),
    };
    (who, body)
}

/// "Tati, Joao e mais 3 reagiram com 👍" (names in order).
pub fn reaction_hover(emoji: &str, names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(6).map(String::as_str).collect();
    let more = names.len().saturating_sub(shown.len());
    let who = match (shown.len(), more) {
        (0, _) => String::new(),
        (1, 0) => shown[0].to_string(),
        (n, 0) => format!("{} e {}", shown[..n - 1].join(", "), shown[n - 1]),
        (_, m) => format!("{} e mais {m}", shown.join(", ")),
    };
    let verb = if names.len() > 1 { "reagiram" } else { "reagiu" };
    format!("{who} {verb} com {emoji}")
}

/// Put `emoji` first in the recent list (no duplicates, at most [`RECENT_MAX`]).
pub fn push_recent(recent: &mut Vec<String>, emoji: &str) {
    recent.retain(|e| e != emoji);
    recent.insert(0, emoji.to_string());
    recent.truncate(RECENT_MAX);
}

/// A failed reaction in words for the person.
pub fn reaction_error_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<vox_client_core::hub::HttpStatusError>() {
        Some(s) if s.status == 409 => format!("Essa mensagem já tem {} reações diferentes.", vox_proto::REACTIONS_MAX_PER_MESSAGE),
        Some(s) if s.status == 429 => "Calma: espere um instante para reagir de novo.".into(),
        Some(s) if s.status == 403 => "Não dá: um de vocês bloqueou o outro.".into(),
        Some(s) if s.status == 404 => "Essa mensagem não existe mais.".into(),
        Some(s) if s.status == 400 => "Só dá para reagir com um emoji.".into(),
        _ => crate::backend::friendly(e),
    }
}

// ---------------------------------------------------------------- state

/// The message being answered in one input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyDraft {
    pub id: i64,
    pub author: User,
    /// "@ ligado": the reply mentions its author too (channels).
    pub ping: bool,
}

/// What a message being sent carries besides its text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendExtras {
    pub mentions: Vec<UserId>,
    pub reply_to: Option<i64>,
    pub reply_mention: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Jump {
    pub target: Target,
    pub id: i64,
    pub at: Instant,
    scrolled: bool,
}

struct Picker {
    target: Target,
    id: i64,
    pos: egui::Pos2,
    /// The frame it opened in (the click that opened it must not close it).
    opened: u64,
}

#[derive(Default)]
pub struct ChatUi {
    /// What the hub knows (none on an older hub: replies, reactions and the list stay hidden).
    pub features: ChatFeatures,
    /// Everybody on the server I know of (the member list, online people, message authors).
    pub people: HashMap<UserId, User>,
    /// The mention list's order (sorted by name), rebuilt when `people` changes.
    people_sorted: Vec<User>,
    people_asked: Option<Instant>,
    /// Bumped when somebody's name changes: cached chip layouts are dropped.
    names_gen: u64,
    spans: SpanCache,
    pub replying: HashMap<Target, ReplyDraft>,
    /// People picked from the mention list per input (id, the name inserted).
    picks: HashMap<Target, Vec<(UserId, String)>>,
    /// The mention list: which entry is selected, and a query it was closed for (Esc).
    ac_selected: usize,
    ac_closed: Option<(Target, usize)>,
    picker: Option<Picker>,
    pub jump: Option<Jump>,
    /// Looking for a quoted message in older pages: (where, which, pages loaded so far).
    seek: Option<(Target, i64, u32)>,
    /// Unread messages that mention me, per channel (the `@` on the sidebar counter).
    pub mention_unread: HashMap<ChannelId, u32>,
    /// The messages behind the sidebar counters, per channel: (id, mentions me). A deleted one
    /// takes its count with it.
    pub unread_ids: HashMap<ChannelId, Vec<(i64, bool)>>,
    /// "Apagar esta mensagem?" (v0.8.2).
    pub confirm_delete: Option<ConfirmDelete>,
    /// For the autotest: deletions asked of the hub, and the messages (target, id) whose menu and
    /// hover bar offered "Apagar mensagem" in the last frame drawn.
    pub deletes_sent: u32,
    pub delete_offers: Vec<(Target, i64)>,
    frame: u64,
    /// For the autotest: reactions asked for, quotes clicked that found their message.
    pub reactions_sent: u32,
    pub jumps_found: u32,
    /// For the autotest's screenshots: draw this message as if the pointer were over it (its
    /// hover bar), since background test mode never moves the real pointer.
    pub force_hover: Option<i64>,
}

impl ChatUi {
    pub fn total_mentions(&self) -> u32 {
        self.mention_unread.values().sum()
    }

    pub fn name(&self, id: UserId) -> String {
        self.people.get(&id).map(|u| u.shown_name().to_string()).unwrap_or_else(|| format!("#{id}"))
    }

    /// Remember a person (and notice a name change).
    pub fn note_user(&mut self, u: &User) {
        match self.people.get(&u.id) {
            Some(old) if old.username == u.username && old.display_name == u.display_name => {
                if old != u {
                    self.people.insert(u.id, u.clone());
                }
            }
            known => {
                let renamed = known.is_some();
                self.people.insert(u.id, u.clone());
                if renamed {
                    self.names_gen += 1;
                }
                self.sort_people();
            }
        }
    }

    fn sort_people(&mut self) {
        let mut v: Vec<User> = self.people.values().cloned().collect();
        v.sort_by_key(|u| u.shown_name().to_lowercase());
        self.people_sorted = v;
    }

    /// The chip layout of a message, computed once (and cached; checked against the text, so a
    /// reused id never gets another message's chips).
    pub fn chips(&mut self, target: Target, id: i64, text: &str, ids: &[UserId], everyone: bool) -> Arc<[Span]> {
        let fp = text_fingerprint(text);
        if let Some((gen, f, s)) = self.spans.get(&(target, id)) {
            if *gen == self.names_gen && *f == fp {
                return s.clone();
            }
        }
        let people: Vec<(UserId, &str, Option<&str>)> =
            ids.iter().filter_map(|i| self.people.get(i)).map(|u| (u.id, u.username.as_str(), u.display_name.as_deref().map(str::trim).filter(|d| !d.is_empty()))).collect();
        let spans: Arc<[Span]> = mention_spans(text, &people, everyone).into();
        if self.spans.len() >= SPAN_CACHE_MAX {
            self.spans.clear();
        }
        self.spans.insert((target, id), (self.names_gen, fp, spans.clone()));
        spans
    }

    /// Drop what is kept about message `id` of `target` (deleted): its chips, a reply being
    /// written to it, a jump / search for it, and the confirmation to delete it.
    fn forget_message(&mut self, target: Target, id: i64) {
        self.spans.remove(&(target, id));
        self.replying.retain(|t, r| !(*t == target && r.id == id));
        if self.jump.is_some_and(|j| j.target == target && j.id == id) {
            self.jump = None;
        }
        if self.seek.is_some_and(|(t, i, _)| t == target && i == id) {
            self.seek = None;
        }
        if self.picker.as_ref().is_some_and(|p| p.target == target && p.id == id) {
            self.picker = None;
        }
        if self.confirm_delete.as_ref().is_some_and(|c| c.target == target && c.id == id) {
            self.confirm_delete = None;
        }
    }

    /// Drop everything kept about a conversation that no longer exists (a deleted channel).
    fn forget_target(&mut self, target: Target) {
        self.spans.retain(|(t, _), _| *t != target);
        self.replying.remove(&target);
        self.picks.remove(&target);
        if self.jump.is_some_and(|j| j.target == target) {
            self.jump = None;
        }
        if self.seek.is_some_and(|(t, _, _)| t == target) {
            self.seek = None;
        }
        if self.picker.as_ref().is_some_and(|p| p.target == target) {
            self.picker = None;
        }
        if self.confirm_delete.as_ref().is_some_and(|c| c.target == target) {
            self.confirm_delete = None;
        }
        if self.ac_closed.is_some_and(|(t, _)| t == target) {
            self.ac_closed = None;
        }
        if let Target::Channel(ch) = target {
            self.mention_unread.remove(&ch);
        }
    }
}

/// A cheap fingerprint of a message's text (the chip cache's check).
fn text_fingerprint(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Chip layouts per message, with the name generation and the text they were made for.
type SpanCache = HashMap<(Target, i64), (u64, u64, Arc<[Span]>)>;

/// "Apagar esta mensagem?" waiting for an answer.
#[derive(Debug, Clone)]
pub struct ConfirmDelete {
    pub target: Target,
    pub id: i64,
    /// Who wrote it and how it starts (shown in the question).
    pub author: String,
    pub excerpt: String,
}

/// What the chat's drawing code asks for (done after the list is drawn, outside its borrows).
#[derive(Debug, Clone)]
pub enum Act {
    Reply(i64),
    React { id: i64, emoji: String, add: bool },
    Picker { id: i64, pos: egui::Pos2 },
    Jump { id: i64, deleted: bool },
    /// "Apagar mensagem"; `now`: Shift was held (no confirmation).
    Delete { id: i64, now: bool },
}

/// What may be done with one message, for its menu and hover bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgActions {
    pub features: ChatFeatures,
    /// Replies and reactions are allowed here (not while a DM is blocked).
    pub social: bool,
    /// "Apagar mensagem" is offered.
    pub delete: bool,
    /// It is my own message (the menu says "Apagar mensagem"; someone else's is "(moderação)").
    pub own: bool,
}

/// May I delete this message? My own: always, wherever I can write (channels on a hub that knows
/// roles, which is where the delete endpoint exists; my DM lines). Somebody else's channel
/// message: Moderador and above (the hub checks it again). Never somebody else's DM line, nor a
/// call line.
pub fn may_delete(target: Target, roles_supported: bool, me: Option<UserId>, author: UserId, my_role_can_moderate: bool, text_line: bool) -> bool {
    let own = me == Some(author);
    match target {
        Target::Channel(_) => roles_supported && (own || my_role_can_moderate),
        Target::Dm(_) => own && text_line,
    }
}

// ---------------------------------------------------------------- drawing pieces (free functions:
// they run inside the message loops, which borrow the logs)

/// Reserve the background slot of a message row (painted once its size is known).
pub fn row_bg_slot(ui: &egui::Ui) -> egui::layers::ShapeIdx {
    ui.painter().add(egui::Shape::Noop)
}

/// Paint a message row's background: the mention highlight, the flash of a clicked quote's
/// original, and a faint hover.
pub fn paint_row_bg(ui: &egui::Ui, slot: egui::layers::ShapeIdx, row: egui::Rect, mentioned: bool, flash: f32, hovered: bool) {
    // (No allocation for a plain row: most rows are neither mentioned, hovered nor flashing.)
    let mut shape = egui::Shape::Noop;
    if mentioned {
        let bar = egui::Rect::from_min_size(row.min, egui::vec2(3.0, row.height()));
        shape = egui::Shape::Vec(vec![egui::Shape::rect_filled(row, 0.0, MENTION_BG), egui::Shape::rect_filled(bar, 0.0, MENTION_BAR)]);
    } else if hovered {
        shape = egui::Shape::rect_filled(row, 0.0, crate::theme::ROW_HOVER.gamma_multiply(0.6));
    }
    if flash > 0.0 {
        shape = egui::Shape::Vec(vec![shape, egui::Shape::rect_filled(row, 0.0, crate::theme::GIZ.gamma_multiply(0.12 * flash))]);
    }
    ui.painter().set(slot, shape);
}

/// The one-line quote above a reply. Returns true when clicked.
pub fn quote_line(ui: &mut egui::Ui, r: &ReplyRef) -> bool {
    let (who, body) = quote_parts(r);
    let mut job = egui::text::LayoutJob::default();
    let small = crate::theme::body(12.0);
    let weak = crate::theme::NEVOA;
    job.append(&format!("{}  ", ph::ARROW_BEND_UP_LEFT), 0.0, egui::TextFormat::simple(small.clone(), weak));
    match who {
        Some(name) => {
            let author = r.author.as_ref().map(|u| u.id).unwrap_or_default();
            job.append(&format!("{name}: "), 0.0, egui::TextFormat::simple(crate::theme::semibold(12.0), super::name_color(author)));
            job.append(&body, 0.0, egui::TextFormat::simple(small, weak));
        }
        None => job.append(&body, 0.0, egui::TextFormat { italics: true, ..egui::TextFormat::simple(small, weak) }),
    }
    let resp = ui.add(egui::Label::new(job).truncate().sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = if r.deleted { resp.on_hover_text("A mensagem original foi apagada") } else { resp.on_hover_text("Ir para a mensagem original") };
    resp.clicked()
}

/// The text of a message with its mention chips (plain `links::chat_message` when it has none).
#[allow(clippy::too_many_arguments)]
pub fn message_text(ui: &mut egui::Ui, chat: &mut ChatUi, target: Target, id: i64, caption: &str, ids: &[UserId], everyone: bool, me: Option<UserId>) {
    if (ids.is_empty() && !everyone) || !caption.contains('@') {
        crate::links::chat_message(ui, caption);
        return;
    }
    let spans = chat.chips(target, id, caption, ids, everyone);
    if spans.is_empty() {
        crate::links::chat_message(ui, caption);
        return;
    }
    let mut offset = 0;
    for line in caption.split('\n') {
        let (start, end) = (offset, offset + line.len());
        offset = end + 1;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut pos = start;
            for s in spans.iter().filter(|s| s.range.start >= start && s.range.end <= end) {
                text_part(ui, &caption[pos..s.range.start]);
                let label = &caption[s.range.clone()];
                let (bg, fg) = match s.chip {
                    Chip::User(u) if Some(u) == me => (CHIP_ME_BG, CHIP_ME_TEXT),
                    Chip::Everyone => (CHIP_ME_BG, CHIP_ME_TEXT),
                    Chip::User(_) => (CHIP_BG, CHIP_TEXT),
                };
                let chip = ui.label(RichText::new(label).font(crate::theme::semibold(14.0)).color(fg).background_color(bg));
                // (The username is only looked up under the pointer.)
                chip.on_hover_ui(|ui| {
                    ui.label(match s.chip {
                        Chip::User(u) => format!("@{}", chat.people.get(&u).map(|p| p.username.as_str()).unwrap_or("")),
                        Chip::Everyone => "Menciona todo mundo".to_string(),
                    });
                });
                pos = s.range.end;
            }
            text_part(ui, &caption[pos..end]);
        });
    }
}

/// Plain text with clickable links, inside a wrapped row.
fn text_part(ui: &mut egui::Ui, text: &str) {
    for seg in crate::links::segments(text) {
        match seg {
            crate::links::Segment::Text(t) => {
                ui.label(t);
            }
            crate::links::Segment::Link { shown, url } => {
                crate::links::link(ui, shown, &url, crate::links::Within::Message);
            }
        }
    }
}

/// The reaction chips under a message ("👍 3", highlighted when I am in it).
pub fn reaction_chips(ui: &mut egui::Ui, chat: &ChatUi, id: i64, reactions: &[Reaction], me: Option<UserId>, acts: &mut Vec<Act>) {
    if reactions.is_empty() {
        return;
    }
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
        for r in reactions {
            let mine = me.is_some_and(|m| r.users.contains(&m));
            let resp = reaction_pill(ui, &r.emoji, r.users.len(), mine);
            // (The names are only looked up under the pointer.)
            let resp = resp.on_hover_ui(|ui| {
                let names: Vec<String> = r.users.iter().map(|u| chat.name(*u)).collect();
                ui.label(reaction_hover(&r.emoji, &names));
            });
            if resp.clicked() && chat.features.reactions {
                acts.push(Act::React { id, emoji: r.emoji.clone(), add: !mine });
            }
        }
    });
}

/// A reaction pill ("👍 2"): round, raised; mine a step lighter with a chalk outline (the lamp is
/// only for speakers, the main action and mentions).
pub fn reaction_pill(ui: &mut egui::Ui, emoji: &str, count: usize, mine: bool) -> egui::Response {
    let g = ui.painter().layout_no_wrap(format!("{emoji} {count}"), theme::body(13.0), theme::GIZ);
    let size = egui::vec2(g.size().x + 18.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, line) = match (mine, resp.hovered()) {
            (true, _) => (theme::MOVEL_HOVER, theme::NEVOA),
            (false, true) => (theme::MOVEL_HOVER, theme::RISCO),
            (false, false) => (theme::MOVEL, theme::RISCO),
        };
        let p = ui.painter();
        p.rect(rect, theme::radius::PILL, fill, egui::Stroke::new(theme::stroke::HAIRLINE, line), egui::StrokeKind::Inside);
        p.galley(rect.center() - g.size() / 2.0, g, theme::GIZ);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The small bar at the top right of a hovered message: reply, react and delete (when allowed),
/// as icon buttons.
pub fn hover_bar(ui: &mut egui::Ui, row: egui::Rect, a: MsgActions, id: i64, acts: &mut Vec<Act>) {
    use crate::ui_kit::{icon_button, IconState};
    let (reply, react) = (a.social && a.features.replies, a.social && a.features.reactions);
    let n = reply as usize + react as usize + a.delete as usize;
    if n == 0 {
        return;
    }
    let w = n as f32 * 32.0 + 8.0;
    let rect = egui::Rect::from_min_size(egui::pos2(row.right() - w - 14.0, row.top() - 16.0), egui::vec2(w, 38.0));
    let mut bar = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::right_to_left(egui::Align::Center)));
    let frame = egui::Frame::new()
        .fill(crate::theme::PAREDE)
        .stroke(egui::Stroke::new(crate::theme::stroke::HAIRLINE, crate::theme::RISCO))
        .corner_radius(crate::theme::radius::BUTTON)
        .inner_margin(egui::Margin::same(3));
    frame.show(&mut bar, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if a.delete {
            let b = icon_button(ui, ph::TRASH, delete_hover(a.own), IconState::Normal);
            if b.hovered() {
                // (Red under the pointer only: deleting is the one destructive action here.)
                ui.painter().text(b.rect.center(), egui::Align2::CENTER_CENTER, ph::TRASH, crate::theme::body(18.0), super::RED);
            }
            if b.clicked() {
                acts.push(Act::Delete { id, now: ui.input(|i| i.modifiers.shift) });
            }
        }
        if react {
            let b = icon_button(ui, ph::SMILEY, "Adicionar reação", IconState::Normal);
            if b.clicked() {
                acts.push(Act::Picker { id, pos: b.rect.left_bottom() + egui::vec2(0.0, 4.0) });
            }
        }
        if reply && icon_button(ui, ph::ARROW_BEND_UP_LEFT, "Responder", IconState::Normal).clicked() {
            acts.push(Act::Reply(id));
        }
    });
}

fn delete_hover(own: bool) -> &'static str {
    if own { "Apagar mensagem (Shift+clique: sem confirmar)" } else { "Apagar mensagem (moderação; Shift+clique: sem confirmar)" }
}

/// The emoji of the quick-reaction row at the top of a message's menu: the recently used ones
/// first, then the usual ones, [`QUICK_REACTIONS`] in all.
pub fn quick_reactions(recent: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::with_capacity(QUICK_REACTIONS);
    for e in recent.iter().map(String::as_str).chain(QUICK_DEFAULTS) {
        if out.len() == QUICK_REACTIONS {
            break;
        }
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// How many emoji the quick-reaction row of a message's menu has.
pub const QUICK_REACTIONS: usize = 5;
/// The usual quick reactions, after the recently used ones.
pub const QUICK_DEFAULTS: [&str; 6] = ["👍", "❤", "😂", "😮", "😢", "🔥"];

/// What a message's menu shows besides the actions (only read while it is open).
#[derive(Clone, Copy)]
pub struct MenuInfo<'a> {
    /// The text of the message (Copiar texto; its links: Copiar link / Abrir link).
    pub caption: &'a str,
    pub reactions: &'a [Reaction],
    pub me: Option<UserId>,
    /// The recently used reactions (the quick row).
    pub recent: &'a [String],
}

/// The rows of a message's menu, in order, as the person reads them (the autotest compares them
/// with what was drawn): `link` is the link it was opened on, if any.
pub fn menu_labels(a: MsgActions, info: MenuInfo<'_>, link: Option<&str>) -> Vec<&'static str> {
    let mut v = Vec::new();
    let has_link = link.is_some() || !crate::links::urls(info.caption, 1).is_empty();
    let (react, reply) = (a.social && a.features.reactions, a.social && a.features.replies);
    if react {
        v.push("reações rápidas");
        v.push("Adicionar reação");
    }
    if reply {
        v.push("Responder");
    }
    if !info.caption.trim().is_empty() {
        v.push("Copiar texto");
    }
    if has_link {
        v.push("Copiar link");
        v.push("Abrir link");
    }
    v.push("Copiar ID da mensagem");
    if a.delete {
        v.push(if a.own { "Apagar mensagem" } else { "Apagar mensagem (moderação)" });
    }
    v
}

/// A message's right-click menu (v1.1, like Discord's): the quick reactions and "Adicionar
/// reação", Responder; Copiar texto, Copiar link and Abrir link (the link right-clicked, else
/// the message's first one), Copiar ID da mensagem; Apagar mensagem (as allowed, with its
/// confirmation). Messages cannot be edited, so there is no "Editar".
pub fn menu_items(ui: &mut egui::Ui, a: MsgActions, id: i64, info: MenuInfo<'_>, link: Option<&str>, acts: &mut Vec<Act>) {
    use crate::ui_kit::{menu_row, menu_separator};
    let (react, reply) = (a.social && a.features.reactions, a.social && a.features.replies);
    if react {
        quick_reaction_row(ui, id, info, acts);
        if menu_row(ui, ph::SMILEY, "Adicionar reação", false).clicked() {
            let pos = ui.ctx().pointer_interact_pos().unwrap_or(ui.min_rect().left_bottom());
            acts.push(Act::Picker { id, pos });
            ui.close();
        }
    }
    if reply && menu_row(ui, ph::ARROW_BEND_UP_LEFT, "Responder", false).clicked() {
        acts.push(Act::Reply(id));
        ui.close();
    }
    if react || reply {
        menu_separator(ui);
    }
    if !info.caption.trim().is_empty() && menu_row(ui, ph::COPY, "Copiar texto", false).clicked() {
        ui.ctx().copy_text(info.caption.to_string());
        tracing::info!(message = id, "message menu: text copied");
        ui.close();
    }
    let first;
    let link = match link {
        Some(l) => Some(l),
        None => {
            first = crate::links::urls(info.caption, 1).pop();
            first.as_deref()
        }
    };
    if let Some(url) = link {
        crate::links::menu_items(ui, url);
    }
    if menu_row(ui, ph::IDENTIFICATION_BADGE, "Copiar ID da mensagem", false).clicked() {
        ui.ctx().copy_text(id.to_string());
        ui.close();
    }
    if a.delete {
        menu_separator(ui);
        let label = if a.own { "Apagar mensagem" } else { "Apagar mensagem (moderação)" };
        let b = menu_row(ui, ph::TRASH, label, true).on_hover_text("Shift+clique: apaga sem confirmar");
        if b.clicked() {
            acts.push(Act::Delete { id, now: ui.input(|i| i.modifiers.shift) });
            ui.close();
        }
    }
}

/// The row of quick reactions at the top of a message's menu (a click puts mine on, or takes it
/// off when it is already there).
fn quick_reaction_row(ui: &mut egui::Ui, id: i64, info: MenuInfo<'_>, acts: &mut Vec<Act>) {
    let quick = quick_reactions(info.recent);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for e in quick {
            let mine = info.me.is_some_and(|me| info.reactions.iter().any(|r| r.emoji == e && r.users.contains(&me)));
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(36.0, 34.0), egui::Sense::click());
            if ui.is_rect_visible(rect) {
                let p = ui.painter();
                if resp.hovered() {
                    p.rect_filled(rect, theme::radius::ROW, theme::MOVEL_HOVER);
                } else if mine {
                    p.rect_filled(rect, theme::radius::ROW, theme::PAREDE);
                }
                p.text(rect.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(19.0), theme::GIZ);
            }
            let hover = if mine { "Tirar a sua reação" } else { "Reagir com" };
            if resp.on_hover_text(format!("{hover} {e}")).clicked() {
                acts.push(Act::React { id, emoji: e.to_string(), add: !mine });
                ui.close();
            }
        }
    });
    ui.add_space(2.0);
}

/// Where a message's menu keeps the link it was opened on.
fn menu_link_id(menu_id: egui::Id) -> egui::Id {
    menu_id.with("link")
}

/// The right-click menu of a message: opens on a right click anywhere on its row (text, images,
/// link cards, reactions, the header's empty space), except where the row has a menu of its own
/// (the author's name and picture: `person_menu`). A right click on a link puts that link in it
/// (`links::take_right_clicked`). `forced`: an autotest holds it open ([`force_menu`]).
#[allow(clippy::too_many_arguments)]
pub fn message_menu(ui: &mut egui::Ui, row: egui::Rect, menu_id: egui::Id, a: MsgActions, id: i64, info: MenuInfo<'_>, person_menu: bool, forced: bool, acts: &mut Vec<Act>) {
    let area = ui.interact(row, menu_id, egui::Sense::hover());
    let right_click = !person_menu && ui.rect_contains_pointer(row) && ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary));
    let forced_at = if forced { forced_menu_at(ui.ctx(), id) } else { None };
    let open = right_click || forced_at.is_some();
    if open {
        // The link under the pointer (if any), for this opening of the menu.
        let link = crate::links::take_right_clicked(ui.ctx());
        ui.data_mut(|d| match link {
            Some(l) => {
                d.insert_temp(menu_link_id(menu_id), l);
            }
            None => d.remove::<String>(menu_link_id(menu_id)),
        });
    }
    let popup = egui::Popup::menu(&area).open_memory(open.then_some(egui::SetOpenCommand::Bool(true)));
    let popup = match forced_at {
        Some(at) => popup.at_position(at),
        None => popup.at_pointer_fixed(),
    };
    popup.frame(crate::ui_kit::popup_frame(ui.style())).style(crate::ui_kit::menu_style).show(|ui| {
        // (The rows take the width they are given: at most this wide.)
        ui.set_min_width(220.0);
        ui.set_max_width(250.0);
        let link: Option<String> = ui.data(|d| d.get_temp(menu_link_id(menu_id)));
        menu_items(ui, a, id, info, link.as_deref(), acts);
    });
}

/// Autotests: hold this message's menu open at `at` (as a right click there, on `link` when given,
/// opens it) until [`force_menu_off`]; the window is never under the real pointer.
pub fn force_menu(ctx: &egui::Context, id: i64, link: Option<String>, at: egui::Pos2) {
    ctx.data_mut(|d| d.insert_temp(forced_menu_key(), (id, link, at)));
}

/// Autotests: let the held menu go (it closes).
pub fn force_menu_off(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<(i64, Option<String>, egui::Pos2)>(forced_menu_key()));
    egui::Popup::close_all(ctx);
}

fn forced_menu_key() -> egui::Id {
    egui::Id::new("force_msg_menu")
}

/// Autotests: the message whose menu [`force_menu`] holds open (read once per list drawn).
pub fn forced_menu(ctx: &egui::Context) -> Option<i64> {
    ctx.data(|d| d.get_temp::<(i64, Option<String>, egui::Pos2)>(forced_menu_key())).map(|(id, _, _)| id)
}

fn forced_menu_at(ctx: &egui::Context, id: i64) -> Option<egui::Pos2> {
    let (want, link, at) = ctx.data(|d| d.get_temp::<(i64, Option<String>, egui::Pos2)>(forced_menu_key()))?;
    if want != id {
        return None;
    }
    if let Some(l) = link {
        crate::links::note_right_clicked(ctx, &l);
    }
    Some(at)
}

/// How strong the flash of a jumped-to message is now (0 = none), and scroll to it once.
pub fn jump_flash(ui: &egui::Ui, chat: &mut ChatUi, target: Target, id: i64, row: egui::Rect) -> f32 {
    let Some(j) = chat.jump.as_mut().filter(|j| j.target == target && j.id == id) else { return 0.0 };
    if !j.scrolled {
        j.scrolled = true;
        ui.scroll_to_rect(row, Some(egui::Align::Center));
    }
    let t = j.at.elapsed();
    if t >= FLASH {
        return 0.0;
    }
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    1.0 - t.as_secs_f32() / FLASH.as_secs_f32()
}

/// One entry of the `@` list: the name, then the username quiet (`@todos` says who it reaches);
/// the selected one raised.
fn mention_row(ui: &mut egui::Ui, c: &Candidate, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width().max(240.0), 28.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected || resp.hovered() {
            p.rect_filled(rect, theme::radius::ROW, if selected { theme::MOVEL_HOVER } else { theme::ROW_HOVER });
        }
        let y = rect.center().y;
        let x = rect.left() + 8.0;
        let (name, sub) = match c.id {
            None => (format!("@{}", c.name), "todo mundo".to_string()),
            Some(_) if !c.username.is_empty() && c.username != c.name => (c.name.clone(), format!("@{}", c.username)),
            Some(_) => (c.name.clone(), String::new()),
        };
        let g = p.layout_no_wrap(name, theme::semibold(13.5), theme::GIZ);
        let w = g.size().x;
        p.galley(egui::pos2(x, y - g.size().y / 2.0), g, theme::GIZ);
        if !sub.is_empty() {
            p.text(egui::pos2(x + w + 8.0, y), egui::Align2::LEFT_CENTER, sub, theme::body(12.0), theme::NEVOA);
        }
    }
    resp
}

// ---------------------------------------------------------------- the app side

impl App {
    /// A hub snapshot: what its chat knows, and the member list for mentions.
    pub(super) fn chat_on_ready(&mut self, chat: Option<ChatFeatures>, online: &[User]) {
        self.chatx.features = chat.unwrap_or_default();
        if let Some(me) = self.me.clone() {
            self.chatx.note_user(&me);
        }
        for u in online {
            self.chatx.note_user(u);
        }
        if self.chatx.features.mentions {
            self.chatx.people_asked = Some(Instant::now());
            self.backend.send(Cmd::Dm(super::dms::DmCmd::People));
        }
        tracing::info!(?self.chatx.features, "chat features");
    }

    pub(super) fn chat_reset(&mut self) {
        self.chatx = ChatUi::default();
    }

    /// The server's member list arrived (`GET /api/dms/people`).
    pub(super) fn chat_people(&mut self, people: &[User]) {
        for u in people {
            self.chatx.note_user(u);
        }
    }

    /// The channel `ch` was read: its counters go.
    pub(super) fn channel_read(&mut self, ch: ChannelId) {
        self.unread.remove(&ch);
        self.chatx.mention_unread.remove(&ch);
        self.chatx.unread_ids.remove(&ch);
    }

    /// A message of `target` was deleted (or replaced by another under the same id): everything
    /// cached for it goes (images, chips, a reply being written to it, its unread count) and
    /// replies to it quote "mensagem apagada".
    pub(crate) fn forget_message(&mut self, target: Target, id: i64, attachments: &[Attachment]) {
        self.forget_images(attachments);
        self.chatx.forget_message(target, id);
        if let Target::Channel(ch) = target {
            if let Some(list) = self.chatx.unread_ids.get_mut(&ch) {
                if let Some(i) = list.iter().position(|(m, _)| *m == id) {
                    let (_, mention) = list.remove(i);
                    if let Some(n) = self.unread.get_mut(&ch) {
                        *n = n.saturating_sub(1);
                        if *n == 0 {
                            self.unread.remove(&ch);
                        }
                    }
                    if mention {
                        if let Some(n) = self.chatx.mention_unread.get_mut(&ch) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                self.chatx.mention_unread.remove(&ch);
                            }
                        }
                    }
                }
            }
        }
        self.quotes_of_deleted(target, id);
    }

    /// The channel `ch` is gone (deleted, or missing from the hub's list): drop every bit of
    /// client state for it, so a channel that might come back with the same id starts empty.
    pub(crate) fn forget_channel(&mut self, ch: ChannelId) {
        let target = Target::Channel(ch);
        if let Some(log) = self.logs.remove(&ch) {
            let atts: Vec<Attachment> = log.msgs.iter().flat_map(|m| m.attachments.iter().cloned()).collect();
            self.forget_images(&atts);
        }
        self.unread.remove(&ch);
        self.chatx.unread_ids.remove(&ch);
        self.chatx.forget_target(target);
        self.images.drop_drafts(target);
        // A fresh scroll position (and anything else egui keeps under the list's id).
        *self.channel_epochs.entry(ch).or_default() += 1;
        if self.selected == Some(ch) {
            self.input.clear();
        }
        tracing::info!(channel = ch, "channel gone: its client state was dropped");
    }

    /// "Apagar mensagem": at once (Shift held), or after "Apagar esta mensagem?".
    pub fn ask_delete(&mut self, target: Target, id: i64, now: bool) {
        if now {
            self.delete_message_now(target, id);
            return;
        }
        let found = match target {
            Target::Channel(ch) => self.logs.get(&ch).and_then(|l| l.msgs.iter().find(|m| m.id == id)).map(|m| (m.author.shown_name().to_string(), m.caption().to_string(), m.attachments.len())),
            Target::Dm(dm) => self.dms.logs.get(&dm).and_then(|l| l.msgs.iter().find(|m| m.id == id)).map(|m| (m.author.shown_name().to_string(), m.caption().to_string(), m.attachments.len())),
        };
        let Some((author, caption, images)) = found else { return };
        let excerpt = match (caption.trim().is_empty(), images) {
            (false, _) => vox_proto::reply_excerpt(&caption),
            (true, 1) => "imagem".into(),
            (true, n) => format!("{n} imagens"),
        };
        self.chatx.confirm_delete = Some(ConfirmDelete { target, id, author, excerpt });
    }

    /// Ask the hub to delete it (the deletion shows when the hub says so, for everybody alike).
    pub fn delete_message_now(&mut self, target: Target, id: i64) {
        self.chatx.confirm_delete = None;
        self.chatx.deletes_sent += 1;
        tracing::info!(?target, id, "delete message");
        match target {
            Target::Channel(_) => self.admin(crate::roles::AdminCmd::DeleteMessage { id }),
            Target::Dm(dm) => self.backend.send(Cmd::Dm(super::dms::DmCmd::Delete { dm, id })),
        }
    }

    /// "Apagar esta mensagem?" (Enter confirms, Esc cancels).
    pub(super) fn delete_message_window(&mut self, ctx: &egui::Context) {
        let Some(c) = self.chatx.confirm_delete.clone() else { return };
        let (mut yes, mut no) = (false, false);
        ctx.input_mut(|i| {
            yes |= i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            no |= i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
        // The kit's dialog: the message quoted, the action on the right.
        let answer = crate::ui_kit::confirm(ctx, "delete_message", "Apagar mensagem", "Apagar", crate::ui_kit::Kind::Danger, true, |ui| {
            crate::ui_kit::body_text(ui, "Apagar esta mensagem?");
            egui::Frame::new().fill(theme::NOITE).corner_radius(theme::radius::ROW).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(RichText::new(&c.author).font(theme::semibold(13.5)).color(theme::GIZ));
                ui.add(egui::Label::new(RichText::new(&c.excerpt).color(theme::TEXTO)).truncate());
            });
            crate::ui_kit::hint(ui, "Ela some para todo mundo, com as imagens. Isso não pode ser desfeito.");
            crate::ui_kit::hint(ui, "Dica: Shift+clique em Apagar mensagem apaga sem perguntar.");
        });
        yes |= answer == Some(true);
        no |= answer == Some(false);
        if yes {
            self.delete_message_now(c.target, c.id);
        } else if no {
            self.chatx.confirm_delete = None;
        }
    }

    fn reply_target_message(&self, target: Target, id: i64) -> Option<User> {
        match target {
            Target::Channel(ch) => self.logs.get(&ch)?.msgs.iter().find(|m| m.id == id).map(|m| m.author.clone()),
            Target::Dm(dm) => self.dms.logs.get(&dm)?.msgs.iter().find(|m| m.id == id).map(|m| m.author.clone()),
        }
    }

    /// "Responder": the input shows who is answered, and gets the focus.
    pub fn start_reply(&mut self, target: Target, id: i64) {
        if !self.chatx.features.replies {
            return;
        }
        let Some(author) = self.reply_target_message(target, id) else { return };
        let ping = Some(author.id) != self.my_id();
        self.chatx.replying.insert(target, ReplyDraft { id, author, ping });
        match target {
            Target::Channel(_) => self.focus_input = true,
            Target::Dm(_) => self.dms.focus_input = true,
        }
    }

    pub fn cancel_reply(&mut self, target: Target) {
        self.chatx.replying.remove(&target);
    }

    /// What a message about to be sent to `target` carries (the reply draft is used up).
    pub fn take_send_extras(&mut self, target: Target, text: &str) -> SendExtras {
        let picks = self.chatx.picks.remove(&target).unwrap_or_default();
        let reply = self.chatx.replying.remove(&target);
        let mentions = match target {
            Target::Channel(_) if self.chatx.features.mentions => send_mentions(text, &self.chatx.people_sorted, &picks, self.my_id()),
            _ => Vec::new(),
        };
        SendExtras {
            mentions,
            reply_to: reply.as_ref().map(|r| r.id),
            reply_mention: matches!(target, Target::Channel(_)) && reply.as_ref().is_some_and(|r| r.ping),
        }
    }

    /// Put my `emoji` on a message (or take it off).
    pub fn react(&mut self, target: Target, id: i64, emoji: &str, add: bool) {
        if !self.chatx.features.reactions || !self.hub_up {
            return;
        }
        if add {
            push_recent(&mut self.settings.recent_reactions, emoji);
            self.save_settings();
        }
        self.chatx.reactions_sent += 1;
        self.backend.send(Cmd::React { target, message: id, emoji: emoji.to_string(), add });
    }

    /// My reactions on a message (to toggle from the picker).
    fn my_reaction(&self, target: Target, id: i64, emoji: &str) -> bool {
        let me = self.my_id();
        let list = match target {
            Target::Channel(ch) => self.logs.get(&ch).and_then(|l| l.msgs.iter().find(|m| m.id == id)).map(|m| m.reactions.as_slice()),
            Target::Dm(dm) => self.dms.logs.get(&dm).and_then(|l| l.msgs.iter().find(|m| m.id == id)).map(|m| m.reactions.as_slice()),
        };
        list.is_some_and(|l| l.iter().any(|r| r.emoji == emoji && me.is_some_and(|m| r.users.contains(&m))))
    }

    /// The hub says a reaction changed.
    pub(super) fn on_reaction(&mut self, target: Target, id: i64, emoji: &str, user: UserId, added: bool) {
        let list = match target {
            Target::Channel(ch) => self.logs.get_mut(&ch).and_then(|l| l.msgs.iter_mut().find(|m| m.id == id)).map(|m| &mut m.reactions),
            Target::Dm(dm) => self.dms.logs.get_mut(&dm).and_then(|l| l.msgs.iter_mut().find(|m| m.id == id)).map(|m| &mut m.reactions),
        };
        if let Some(list) = list {
            vox_proto::apply_reaction(list, emoji, user, added);
        }
    }

    /// A message was deleted: replies to it now quote "mensagem apagada".
    pub(crate) fn quotes_of_deleted(&mut self, target: Target, id: i64) {
        let gone = |r: &mut Option<ReplyRef>| {
            if let Some(q) = r.as_mut().filter(|q| q.id == id) {
                *q = ReplyRef::gone(id);
            }
        };
        match target {
            Target::Channel(ch) => self.logs.get_mut(&ch).into_iter().flat_map(|l| l.msgs.iter_mut()).for_each(|m| gone(&mut m.reply)),
            Target::Dm(dm) => self.dms.logs.get_mut(&dm).into_iter().flat_map(|l| l.msgs.iter_mut()).for_each(|m| gone(&mut m.reply)),
        }
    }

    /// (oldest id loaded, all loaded, a page is loading) of a conversation, and whether `id` is in it.
    fn log_facts(&self, target: Target, id: i64) -> (Option<i64>, bool, bool) {
        match target {
            Target::Channel(ch) => self.logs.get(&ch).map_or((None, false, false), |l| (l.msgs.first().map(|m| m.id), l.no_more, l.msgs.iter().any(|m| m.id == id))),
            Target::Dm(dm) => self.dms.logs.get(&dm).map_or((None, false, false), |l| (l.msgs.first().map(|m| m.id), l.no_more, l.msgs.iter().any(|m| m.id == id))),
        }
    }

    fn load_older(&mut self, target: Target, before: i64) {
        match target {
            Target::Channel(ch) => {
                if let Some(l) = self.logs.get_mut(&ch) {
                    l.loading_older = true;
                }
                self.backend.send(Cmd::LoadHistory { channel: ch, before: Some(before) });
            }
            Target::Dm(dm) => {
                if let Some(l) = self.dms.logs.get_mut(&dm) {
                    l.loading_older = true;
                }
                self.backend.send(Cmd::Dm(super::dms::DmCmd::History { dm, before: Some(before) }));
            }
        }
    }

    /// A clicked quote: scroll to the original and flash it, loading older pages when it is not
    /// loaded yet.
    pub fn jump_to(&mut self, target: Target, id: i64, deleted: bool) {
        if deleted {
            self.toast("A mensagem original foi apagada.");
            return;
        }
        let (oldest, no_more, found) = self.log_facts(target, id);
        if found {
            self.chatx.jump = Some(Jump { target, id, at: Instant::now(), scrolled: false });
            self.chatx.jumps_found += 1;
        } else if oldest.is_some_and(|o| id < o) && !no_more {
            self.chatx.seek = Some((target, id, 1));
            self.load_older(target, oldest.unwrap_or(id));
        } else {
            self.toast("A mensagem original não está mais disponível.");
        }
    }

    /// An older page arrived for `target`: maybe it has the quoted message we look for.
    pub(super) fn chat_on_history(&mut self, target: Target) {
        let Some((t, id, pages)) = self.chatx.seek else { return };
        if t != target {
            return;
        }
        let (oldest, no_more, found) = self.log_facts(target, id);
        if found {
            self.chatx.seek = None;
            self.jump_to(target, id, false);
        } else if !no_more && pages < SEEK_PAGES && oldest.is_some_and(|o| id < o) {
            self.chatx.seek = Some((target, id, pages + 1));
            self.load_older(target, oldest.unwrap_or(id));
        } else {
            self.chatx.seek = None;
            self.toast("A mensagem original não está carregada (é antiga demais).");
        }
    }

    /// Run what the message list asked for.
    pub(super) fn chat_acts(&mut self, target: Target, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Reply(id) => self.start_reply(target, id),
                Act::React { id, emoji, add } => self.react(target, id, &emoji, add),
                Act::Picker { id, pos } => self.open_reaction_picker(target, id, pos),
                Act::Jump { id, deleted } => self.jump_to(target, id, deleted),
                Act::Delete { id, now } => self.ask_delete(target, id, now),
            }
        }
    }

    /// Open the reaction picker for a message at `pos` (the hover bar's smiley, "Adicionar
    /// reação" in its menu; the emoji autotest).
    pub fn open_reaction_picker(&mut self, target: Target, id: i64, pos: egui::Pos2) {
        self.chatx.picker = Some(Picker { target, id, pos, opened: self.chatx.frame });
    }

    /// The reaction picker, when open (drawn over everything).
    pub(super) fn reaction_picker(&mut self, ctx: &egui::Context) {
        self.chatx.frame += 1;
        let Some(p) = self.chatx.picker.as_ref() else { return };
        let (target, id, pos, opened) = (p.target, p.id, p.pos, p.opened);
        let mut chosen: Option<String> = None;
        let recent = self.settings.recent_reactions.clone();
        let area = egui::Area::new(egui::Id::new("reaction_picker")).order(egui::Order::Foreground).fixed_pos(pos).constrain(true).show(ctx, |ui| {
            crate::ui_kit::popup_frame(ui.style()).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(2.0, 2.0);
                let mut cell = |ui: &mut egui::Ui, e: &str| {
                    if ui.add(egui::Button::new(RichText::new(e).size(18.0)).frame(false).min_size(egui::vec2(28.0, 28.0))).clicked() {
                        chosen = Some(e.to_string());
                    }
                };
                if !recent.is_empty() {
                    crate::ui_kit::section(ui, "Recentes");
                    ui.horizontal(|ui| {
                        for e in &recent {
                            cell(ui, e);
                        }
                    });
                    ui.separator();
                }
                egui::Grid::new("reaction_grid").spacing(egui::vec2(2.0, 2.0)).show(ui, |ui| {
                    for (i, e) in REACTION_GRID.iter().enumerate() {
                        cell(ui, e);
                        if i % 10 == 9 {
                            ui.end_row();
                        }
                    }
                });
            });
        });
        if let Some(e) = chosen {
            self.chatx.picker = None;
            let add = !self.my_reaction(target, id, &e);
            self.react(target, id, &e, add);
            return;
        }
        let clicked_outside = ctx.input(|i| i.pointer.any_click()) && !ctx.pointer_interact_pos().is_some_and(|p| area.response.rect.contains(p));
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        if (clicked_outside && self.chatx.frame > opened + 1) || esc || self.chat_target_on_screen() != Some(target) {
            self.chatx.picker = None;
        }
    }

    /// "Respondendo a <nome>" over an input (with "@ ligado" in channels, and ✖).
    pub(super) fn reply_bar(&mut self, ui: &mut egui::Ui, target: Target) {
        let Some(r) = self.chatx.replying.get_mut(&target) else { return };
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{}  Respondendo a", ph::ARROW_BEND_UP_LEFT)).font(crate::theme::body(12.5)).color(crate::theme::NEVOA));
            ui.label(RichText::new(r.author.shown_name()).font(crate::theme::semibold(12.5)).color(super::name_color(r.author.id)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if crate::ui_kit::icon_button(ui, ph::X, "Cancelar a resposta (Esc)", crate::ui_kit::IconState::Normal).clicked() {
                    cancel = true;
                }
                if matches!(target, Target::Channel(_)) && Some(r.author.id) != self.me.as_ref().map(|u| u.id) {
                    let label = if r.ping { RichText::new("@ ligado").small().color(MENTION_BAR) } else { RichText::new("@ desligado").small().weak() };
                    if ui.add(egui::Button::new(label).frame(false)).on_hover_text("Ligado: a pessoa respondida também é mencionada (é avisada)").clicked() {
                        r.ping = !r.ping;
                    }
                }
            });
        });
        if cancel {
            self.cancel_reply(target);
        }
    }

    /// The mention list for the channel input's text and cursor: where the `@` is and the entries.
    pub fn mention_list(&self, target: Target, text: &str, cursor: usize) -> Option<(usize, Vec<Candidate>)> {
        if !self.chatx.features.mentions || !matches!(target, Target::Channel(_)) {
            return None;
        }
        let (at, query) = mention_query(text, cursor)?;
        if self.chatx.ac_closed == Some((target, at)) {
            return None;
        }
        // A private channel (v0.9): only the people who may see it (the hub drops anybody else).
        let private = match target {
            Target::Channel(ch) => self.channels.iter().find(|c| c.id == ch).and_then(|c| c.private.clone()),
            Target::Dm(_) => None,
        };
        let list = match &private {
            Some(p) => {
                let allowed: Vec<User> = self.chatx.people_sorted.iter().filter(|u| p.allows(u.id, u.role)).cloned().collect();
                candidates(&allowed, self.my_id(), query, self.my_role().can_moderate())
            }
            None => candidates(&self.chatx.people_sorted, self.my_id(), query, self.my_role().can_moderate()),
        };
        (!list.is_empty()).then_some((at, list))
    }

    /// Pick `cand` for the `@query` at `at`..`cursor` of the channel input. Returns the new
    /// cursor (byte offset).
    pub fn pick_mention(&mut self, target: Target, at: usize, cursor: usize, cand: &Candidate) -> usize {
        let input = match target {
            Target::Channel(_) => &mut self.input,
            Target::Dm(_) => &mut self.dms.input,
        };
        let (text, new_cursor) = complete(input, at, cursor, &cand.name);
        *input = text;
        if let Some(id) = cand.id {
            self.chatx.picks.entry(target).or_default().push((id, cand.name.clone()));
        }
        self.chatx.ac_selected = 0;
        new_cursor
    }

    /// Keys for the mention list, read before the text box sees them. Returns (the text box keeps
    /// the arrows / Tab / Esc while the list is open).
    fn mention_keys(&mut self, ui: &egui::Ui, list: &Option<(usize, Vec<Candidate>)>) -> Option<usize> {
        let (_, entries) = list.as_ref()?;
        let n = entries.len();
        self.chatx.ac_selected = self.chatx.ac_selected.min(n - 1);
        let key = |k| ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
        if key(egui::Key::ArrowDown) {
            self.chatx.ac_selected = (self.chatx.ac_selected + 1) % n;
        }
        if key(egui::Key::ArrowUp) {
            self.chatx.ac_selected = (self.chatx.ac_selected + n - 1) % n;
        }
        if key(egui::Key::Tab) || key(egui::Key::Enter) {
            return Some(self.chatx.ac_selected);
        }
        None
    }

    /// The channel's text box with the mention list. Returns the text box's response.
    pub(super) fn mention_input(&mut self, ui: &mut egui::Ui, target: Target, hint: String, width: f32) -> egui::Response {
        let id = egui::Id::new("chat_input_box");
        let cursor = egui::text_edit::TextEditState::load(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| self.input.char_indices().nth(r.primary.index.into()).map_or(self.input.len(), |(b, _)| b))
            .unwrap_or(self.input.len());
        let list = self.mention_list(target, &self.input, cursor);
        let mut esc = false;
        let picked = if list.is_some() {
            esc = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            self.mention_keys(ui, &list)
        } else {
            None
        };
        // Esc with no list: drop the reply being written.
        if list.is_none() && self.chatx.replying.contains_key(&target) && ui.memory(|m| m.has_focus(id)) && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_reply(target);
        }
        let open = list.is_some();
        let filter = egui::EventFilter { tab: open, vertical_arrows: open, horizontal_arrows: false, escape: open };
        let kept = super::chat_view::box_menu_before(ui, id, &self.input);
        let resp = ui.add(super::chat_view::field_edit(&mut self.input, hint).desired_width(width).event_filter(filter).id(id));
        // The right-click menu: Recortar, Copiar, Colar (a picture goes to the attachments, as
        // Ctrl+V does), Selecionar tudo.
        if super::chat_view::box_menu(ui, &resp, id, &self.input, kept, self.video.parent.unwrap_or(0)) == super::chat_view::BoxOut::PasteImage {
            let ctx = ui.ctx().clone();
            self.paste_image_from_menu(target, &ctx);
        }
        if let Some((at, entries)) = &list {
            if esc {
                self.chatx.ac_closed = Some((target, *at));
            }
            let mut choice = picked;
            if !esc {
                let sel = self.chatx.ac_selected;
                let area = egui::Area::new(egui::Id::new("mention_list")).order(egui::Order::Foreground).fixed_pos(resp.rect.left_top()).pivot(egui::Align2::LEFT_BOTTOM);
                area.show(ui.ctx(), |ui| {
                    crate::ui_kit::popup_frame(ui.style()).show(ui, |ui| {
                        ui.set_min_width(260.0);
                        crate::ui_kit::section(ui, "Membros");
                        ui.add_space(2.0);
                        for (i, c) in entries.iter().enumerate() {
                            if mention_row(ui, c, i == sel).clicked() {
                                choice = Some(i);
                            }
                        }
                        ui.add_space(2.0);
                        ui.label(RichText::new("Setas: escolher · Tab ou Enter: inserir · Esc: fechar").font(crate::theme::body(11.5)).color(crate::theme::NEVOA));
                    });
                });
            }
            if let Some(i) = choice {
                if let Some(c) = entries.get(i).cloned() {
                    let new_cursor = self.pick_mention(target, *at, cursor, &c);
                    let chars = self.input[..new_cursor].chars().count();
                    if let Some(mut s) = egui::text_edit::TextEditState::load(ui.ctx(), id) {
                        s.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(chars))));
                        s.store(ui.ctx(), id);
                    }
                    resp.request_focus();
                }
            }
        } else if self.chatx.ac_closed.is_some_and(|(t, at)| t == target && mention_query(&self.input, cursor).is_none_or(|(a, _)| a != at)) {
            self.chatx.ac_closed = None;
        }
        // The member list can be stale (somebody new joined the server): refresh it now and then.
        if open && self.chatx.people_asked.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            self.chatx.people_asked = Some(Instant::now());
            self.backend.send(Cmd::Dm(super::dms::DmCmd::People));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people() -> Vec<User> {
        let mut tati = User::new(2, "tati");
        tati.display_name = Some("Tati Silva".into());
        vec![User::new(1, "dono"), tati, User::new(3, "joao"), User::new(4, "Joana")]
    }

    #[derive(Debug, Clone, PartialEq)]
    struct M(i64, &'static str);

    fn ids(v: &[M]) -> Vec<i64> {
        v.iter().map(|m| m.0).collect()
    }

    fn merge(msgs: &mut Vec<M>, page: Vec<M>, before: Option<i64>, complete: bool) -> Merged<M> {
        merge_page(msgs, page, before, complete, |m| m.0, |a, b| a.1.chars().next() == b.1.chars().next())
    }

    #[test]
    fn a_history_page_is_the_truth_for_the_ids_it_covers() {
        // Newest page after a reconnect: 3 was deleted meanwhile, 5 came back as another message
        // (an older hub reused the id), 2 got a reaction ("b" -> "b+"), 9 arrived live after the
        // page was read.
        let mut msgs = vec![M(1, "a"), M(2, "b"), M(3, "c"), M(5, "old"), M(9, "live")];
        let out = merge(&mut msgs, vec![M(6, "f"), M(5, "new"), M(2, "b+")], None, false);
        assert_eq!(ids(&msgs), [1, 2, 5, 6, 9], "1 is older than the page: untouched; 9 is newer: kept");
        assert_eq!((msgs[1].1, msgs[2].1), ("b+", "new"), "the page's copies win");
        assert_eq!(out, Merged { deleted: vec![M(3, "c")], replaced: vec![M(5, "old")] });
        // An older page (before 2), the last one: everything below 2 that it lacks is gone.
        let out = merge(&mut msgs, vec![], Some(2), true);
        assert_eq!(ids(&msgs), [2, 5, 6, 9]);
        assert_eq!(out.deleted, vec![M(1, "a")]);
        // The newest page of a channel with nothing left in it.
        let out = merge(&mut msgs, vec![], None, true);
        assert!(msgs.is_empty() && out.deleted.len() == 4);
        // An empty newest page that is not marked complete changes nothing it cannot vouch for.
        let mut msgs = vec![M(4, "x")];
        assert!(merge(&mut msgs, vec![], None, false).deleted.is_empty());
        assert_eq!(ids(&msgs), [4]);
    }

    #[test]
    fn who_may_delete_what() {
        let (ch, dm) = (Target::Channel(1), Target::Dm(1));
        // My own channel message: any role, with or without images (the author is what counts).
        assert!(may_delete(ch, true, Some(5), 5, false, true));
        assert!(may_delete(ch, true, Some(5), 5, true, true), "an admin / the owner deletes their own too");
        // Somebody else's: moderators and up only.
        assert!(!may_delete(ch, true, Some(5), 6, false, true));
        assert!(may_delete(ch, true, Some(5), 6, true, true));
        // A hub without roles has no delete endpoint.
        assert!(!may_delete(ch, false, Some(5), 5, true, true));
        // DMs: only my own text lines, whatever my role.
        assert!(may_delete(dm, true, Some(5), 5, false, true));
        assert!(!may_delete(dm, true, Some(5), 6, true, true));
        assert!(!may_delete(dm, true, Some(5), 5, false, false), "call lines stay");
        assert!(!may_delete(ch, true, None, 5, false, true));
    }

    #[test]
    fn the_query_is_the_word_after_an_at() {
        assert_eq!(mention_query("oi @ma", 6), Some((3, "ma")));
        assert_eq!(mention_query("@", 1), Some((0, "")));
        assert_eq!(mention_query("oi @Tati S", 10), Some((3, "Tati S")));
        assert_eq!(mention_query("email@x", 7), None, "inside a word");
        assert_eq!(mention_query("oi @ma tudo", 2), None, "the cursor is before it");
        assert_eq!(mention_query("(@jo", 4), Some((1, "jo")));
        assert_eq!(mention_query("oi", 99), None);
    }

    #[test]
    fn candidates_prefix_first_never_me_and_todos_for_staff() {
        let p = people();
        let names = |v: Vec<Candidate>| v.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(candidates(&p, Some(1), "jo", false)), ["Joana", "joao"]);
        assert_eq!(names(candidates(&p, Some(1), "silva", false)), ["Tati Silva"], "contains, in the display name");
        assert_eq!(names(candidates(&p, Some(1), "tati s", false)), ["Tati Silva"]);
        assert!(candidates(&p, Some(1), "do", false).is_empty(), "not me");
        assert!(candidates(&p, Some(1), "x y", false).is_empty());
        assert_eq!(names(candidates(&p, Some(3), "to", true)), ["todos"]);
        assert!(candidates(&p, Some(3), "to", false).is_empty(), "a member is not offered @todos");
        assert_eq!(candidates(&p, Some(1), "", true).len(), 4, "@todos and the three others");
        let c = &candidates(&p, Some(1), "tat", false)[0];
        assert_eq!((c.id, c.username.as_str()), (Some(2), "tati"));
    }

    #[test]
    fn completing_replaces_the_query() {
        assert_eq!(complete("oi @ta", 3, 6, "Tati Silva"), ("oi @Tati Silva ".to_string(), 15));
        assert_eq!(complete("oi @ta tudo", 3, 6, "Tati"), ("oi @Tati tudo".to_string(), 9));
        assert_eq!(complete("@", 0, 1, "todos"), ("@todos ".to_string(), 7));
    }

    #[test]
    fn sent_mentions_follow_the_text() {
        let p = people();
        let picks = vec![(2, "Tati Silva".to_string()), (3, "joao".to_string())];
        assert_eq!(send_mentions("oi @Tati Silva", &p, &picks, Some(1)), vec![2], "joao was deleted from the text");
        assert_eq!(send_mentions("@joana e @TATI", &p, &[], Some(1)), vec![2, 4], "typed by hand");
        assert!(send_mentions("@dono", &p, &[(1, "dono".into())], Some(1)).is_empty(), "never me");
        assert!(send_mentions("sem arroba", &p, &[], Some(1)).is_empty());
    }

    #[test]
    fn chips_cover_names_and_todos_without_overlaps() {
        let t = "@Tati Silva e @tati e @todos e @Joana";
        let people = [(2, "tati", Some("Tati Silva")), (4, "Joana", None)];
        let s = mention_spans(t, &people, true);
        let shown: Vec<(&str, Chip)> = s.iter().map(|s| (&t[s.range.clone()], s.chip)).collect();
        assert_eq!(shown, [("@Tati Silva", Chip::User(2)), ("@tati", Chip::User(2)), ("@todos", Chip::Everyone), ("@Joana", Chip::User(4))]);
        assert!(mention_spans(t, &people, false).iter().all(|s| s.chip != Chip::Everyone), "a member's @todos is plain text");
        // "@Tati" inside "@Tati Silva" is not a second chip.
        let s = mention_spans("@Tati Silva", &[(2, "Tati", Some("Tati Silva"))], false);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].range, 0..11);
    }

    #[test]
    fn quotes_and_hover_texts() {
        let r = ReplyRef { id: 1, author: Some(User::new(2, "Tati")), excerpt: "oi".into(), images: 0, deleted: false, sealed: false };
        assert_eq!(quote_parts(&r), (Some("Tati".into()), "oi".into()));
        assert_eq!(quote_parts(&ReplyRef { excerpt: String::new(), images: 1, ..r.clone() }).1, "imagem");
        assert_eq!(quote_parts(&ReplyRef { excerpt: String::new(), images: 3, ..r.clone() }).1, "3 imagens");
        assert_eq!(quote_parts(&ReplyRef::gone(1)), (None, "mensagem apagada".into()));
        assert_eq!(reaction_hover("👍", &["Tati".into()]), "Tati reagiu com 👍");
        assert_eq!(reaction_hover("👍", &["Tati".into(), "Joao".into()]), "Tati e Joao reagiram com 👍");
        let many: Vec<String> = (0..8).map(|i| format!("p{i}")).collect();
        assert_eq!(reaction_hover("🎉", &many), "p0, p1, p2, p3, p4, p5 e mais 2 reagiram com 🎉");
    }

    #[test]
    fn recent_reactions_stay_short_and_unique() {
        let mut r = Vec::new();
        for e in ["👍", "🎉", "👍", "😂"] {
            push_recent(&mut r, e);
        }
        assert_eq!(r, ["😂", "👍", "🎉"]);
        for e in REACTION_GRID {
            push_recent(&mut r, e);
        }
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], REACTION_GRID[39]);
    }

    #[test]
    fn the_quick_row_puts_the_recent_reactions_first() {
        assert_eq!(quick_reactions(&[]), ["👍", "❤", "😂", "😮", "😢"]);
        let recent: Vec<String> = ["🔥", "👍", "🎉"].iter().map(|s| s.to_string()).collect();
        assert_eq!(quick_reactions(&recent), ["🔥", "👍", "🎉", "❤", "😂"]);
        let many: Vec<String> = REACTION_GRID.iter().take(8).map(|s| s.to_string()).collect();
        assert_eq!(quick_reactions(&many).len(), QUICK_REACTIONS);
    }

    #[test]
    fn the_message_menu_has_what_applies() {
        let a = MsgActions { features: ChatFeatures::ALL, social: true, delete: true, own: true };
        let info = MenuInfo { caption: "olha https://example.com/x", reactions: &[], me: Some(1), recent: &[] };
        let rows = menu_labels(a, info, None);
        assert_eq!(rows, ["reações rápidas", "Adicionar reação", "Responder", "Copiar texto", "Copiar link", "Abrir link", "Copiar ID da mensagem", "Apagar mensagem"]);
        // No link, an image without a caption, someone else's message in a blocked DM.
        let a = MsgActions { features: ChatFeatures::ALL, social: false, delete: false, own: false };
        let info = MenuInfo { caption: "", reactions: &[], me: Some(1), recent: &[] };
        assert_eq!(menu_labels(a, info, None), ["Copiar ID da mensagem"]);
        // Opened on a link of a message without one in its text (a preview card): the link rows.
        let info = MenuInfo { caption: "sem link", reactions: &[], me: Some(1), recent: &[] };
        assert!(menu_labels(a, info, Some("https://example.com")).contains(&"Abrir link"));
        // A moderator's delete of someone else's message says so; there is never "Editar".
        let a = MsgActions { features: ChatFeatures::ALL, social: true, delete: true, own: false };
        let rows = menu_labels(a, info, None);
        assert!(rows.contains(&"Apagar mensagem (moderação)") && !rows.contains(&"Editar"));
    }

    #[test]
    fn the_grid_is_forty_distinct_single_emoji() {
        let mut seen = std::collections::HashSet::new();
        for e in REACTION_GRID {
            assert!(vox_proto::clean_reaction_emoji(e).is_some(), "{e}");
            assert!(seen.insert(e), "{e} twice");
        }
    }
}
