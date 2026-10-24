//! Calladex (v1.0.1): the owner's easter egg, a playful "Pokédex" of the server's members, signed.
//!
//! - In any chat input (a channel or a DM), the exact message `/calladex` is not sent: it opens the
//!   card with one entry per member the app knows, in join order (the user ids grow as people first
//!   log in): "#001", the avatar in its colour ring, the name, a "tipo" from the role, "Membro desde"
//!   (when the app already has it, else "???") and a short line picked from [`LINES`] by the id.
//!   A message that merely contains the word is sent as usual ([`is_command`]).
//! - On the home screen, typing "calladex" with no field focused opens the credits flavour: just
//!   the signature, since there are no members there.
//!
//! Purely local: it reads what the app already holds (no request to the hub, not even for a
//! picture: only the ones already loaded are drawn), logs one debug line, and while closed it is
//! `None` (nothing drawn, nothing kept).
//! A child module of `app`.

use eframe::egui::{self, Align, Align2, Color32, Layout, Margin, Pos2, RichText, Sense, Stroke, Vec2};
use vox_proto::{Role, User, UserId};

use super::{local_offset_min, name_color, App};
use crate::brand;
use crate::chat_images::Target;
use crate::theme::{self, ph, space, stroke};
use crate::ui_kit::{self, IconState};

/// What opens it in a chat input (the whole message, nothing else).
pub const COMMAND: &str = "/calladex";
/// What opens it on the home screen (typed with no field focused).
pub const SEQUENCE: &str = "calladex";
/// The card's last line.
pub const FOOTER: &str = "Calladex v1 · por Cezar Callado";

/// The short lines of the entries (one per person, always the same for the same person).
pub const LINES: [&str; 12] = [
    "Aparece quando menos se espera.",
    "Fala pouco, mas quando fala a sala para.",
    "Mestre do \"já volto\".",
    "Nunca recusa uma partida.",
    "Ri das próprias piadas, e com razão.",
    "Habitat natural: o canal de memes.",
    "Visto pela última vez às 3 da manhã.",
    "Entra na call só pra ouvir. Dizem.",
    "Tem sempre um áudio de dois minutos.",
    "Rara aparição em dia de semana.",
    "Domina a arte do microfone mudo.",
    "Chega atrasado, sai por último.",
];

/// The exact command, and nothing else, opens the card (spaces around it are forgiven, the way
/// the message itself would be trimmed before sending).
pub fn is_command(input: &str) -> bool {
    input.trim() == COMMAND
}

/// Feed what was typed on the home screen into the last letters; true when they now spell
/// [`SEQUENCE`] (then they start over).
pub fn push_keys(buf: &mut String, typed: &str) -> bool {
    let mut hit = false;
    for c in typed.chars() {
        buf.push(c.to_ascii_lowercase());
        let n = buf.chars().count();
        if n > SEQUENCE.len() {
            *buf = buf.chars().skip(n - SEQUENCE.len()).collect();
        }
        if buf == SEQUENCE {
            buf.clear();
            hit = true;
        }
    }
    hit
}

/// The entry's "tipo", from the server role.
pub fn tipo(role: Role) -> &'static str {
    match role {
        Role::Owner => "Dono 👑",
        Role::Admin => "Admin",
        Role::Moderator => "Moderador",
        Role::Member => "Membro",
    }
}

/// The person's own line: the same one every time (a hash of the id, not random); a card may
/// give the next free one instead (see [`entries`]).
#[cfg(test)]
pub fn fun_line(id: UserId) -> &'static str {
    LINES[line_index(id)]
}

fn line_index(id: UserId) -> usize {
    // splitmix64's finaliser: neighbouring ids get unrelated lines.
    let mut x = id as u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x % LINES.len() as u64) as usize
}

/// "#001".
pub fn number(n: usize) -> String {
    format!("#{n:03}")
}

/// Which card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// From a chat: the server's members.
    Members,
    /// From the home screen: the signature only.
    Credits,
}

/// One member's entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub number: usize,
    pub user: User,
    pub tipo: &'static str,
    /// When they first logged in to this server, if the app already knows it.
    pub since_ms: Option<i64>,
    pub line: &'static str,
    pub ring: Color32,
}

/// The entries, in join order (by user id), numbered from 1; each person once. Each gets their
/// own line ([`line_index`]), unless an earlier entry has it already: then the next free line of
/// the list (a card shows no line twice until the list runs out; the same people, the same card).
pub fn entries(people: impl IntoIterator<Item = User>, since: impl Fn(UserId) -> Option<i64>, ring: impl Fn(UserId) -> Color32) -> Vec<Entry> {
    let mut users: Vec<User> = people.into_iter().collect();
    users.sort_by_key(|u| u.id);
    users.dedup_by_key(|u| u.id);
    let mut used = [false; LINES.len()];
    users
        .into_iter()
        .enumerate()
        .map(|(i, user)| {
            if used.iter().all(|u| *u) {
                used = [false; LINES.len()];
            }
            let mut k = line_index(user.id);
            while used[k] {
                k = (k + 1) % LINES.len();
            }
            used[k] = true;
            Entry { number: i + 1, tipo: tipo(user.role), since_ms: since(user.id), line: LINES[k], ring: ring(user.id), user }
        })
        .collect()
}

/// The open card.
pub struct Calladex {
    pub flavour: Flavour,
    pub entries: Vec<Entry>,
    server: String,
}

/// Width of the card's content.
const CARD_W: f32 = 420.0;
/// The avatar's diameter in an entry.
const AVATAR: f32 = 40.0;

impl App {
    /// Enter in the chat input of `target`: when it holds exactly `/calladex`, the input is cleared,
    /// the card opens and nothing is sent (true).
    pub(super) fn calladex_command(&mut self, target: Target) -> bool {
        let input = match target {
            Target::Channel(_) => &mut self.input,
            Target::Dm(_) => &mut self.dms.input,
        };
        if !is_command(input) {
            return false;
        }
        input.clear();
        self.open_calladex(Flavour::Members);
        true
    }

    /// Open the card (a snapshot of what the app knows now).
    pub fn open_calladex(&mut self, flavour: Flavour) {
        let entries = match flavour {
            Flavour::Credits => Vec::new(),
            Flavour::Members => {
                let mut people: Vec<User> = self.chatx.people.values().cloned().collect();
                people.extend(self.online.iter().cloned());
                people.extend(self.me.iter().cloned());
                let admin_since: std::collections::HashMap<UserId, i64> = self.roles.members.iter().map(|m| (m.user.id, m.created_at_ms)).collect();
                entries(
                    people,
                    |id| {
                        self.cached_profile(id)
                            .map(|p| p.member_since_ms)
                            .filter(|ms| *ms > 0)
                            .or_else(|| admin_since.get(&id).copied().filter(|ms| *ms > 0))
                    },
                    |id| self.cached_profile(id).and_then(|p| crate::profile::accent_color(p.accent.as_deref())).unwrap_or_else(|| name_color(id)),
                )
            }
        };
        tracing::debug!(?flavour, members = entries.len(), "calladex opened");
        self.calladex = Some(Calladex { flavour, entries, server: self.server_name.clone() });
    }

    /// The home screen: "calladex" typed with no field focused opens the credits.
    pub(super) fn calladex_home_keys(&mut self, ctx: &egui::Context) {
        if self.calladex.is_some() || ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let typed: String = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect()
        });
        if !typed.is_empty() {
            self.calladex_home_typed(&typed);
        }
    }

    /// Text typed on the home screen with no field focused (also the autotest's way in).
    pub fn calladex_home_typed(&mut self, typed: &str) {
        if self.calladex.is_none() && push_keys(&mut self.calladex_keys, typed) {
            self.open_calladex(Flavour::Credits);
        }
    }

    /// The card, in the middle of the window. Esc or ✕ closes it.
    pub(super) fn calladex_window(&mut self, ctx: &egui::Context) {
        let Some(dex) = self.calladex.as_ref() else { return };
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        // (I6: the list leaves the card's header and footer room in a short window.)
        let h = ctx.content_rect().height();
        let max_list = (h * 0.55).min(h - 230.0).clamp(60.0, 440.0);
        let store = &self.avatars.store;
        egui::Window::new("Calladex")
            .id(egui::Id::new("calladex_card"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(ui_kit::dialog_frame(&ctx.global_style()))
            .show(ctx, |ui| {
                ui.set_width(ui_kit::dialog_w(ctx, CARD_W));
                ui.spacing_mut().item_spacing.y = 6.0;
                close |= header(ui, dex);
                ui.add_space(space::M);
                match dex.flavour {
                    Flavour::Credits => credits(ui),
                    Flavour::Members if dex.entries.is_empty() => {
                        ui_kit::body_text(ui, "Nenhum membro registrado ainda. Volte quando a turma chegar!");
                    }
                    Flavour::Members => {
                        egui::ScrollArea::vertical().max_height(max_list).auto_shrink([false, true]).show(ui, |ui| {
                            for e in &dex.entries {
                                entry(ui, e, store.loaded(&e.user));
                            }
                        });
                    }
                }
                ui.add_space(space::M);
                let w = ui.available_width();
                let (r, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
                ui.painter().hline(r.x_range(), r.center().y, Stroke::new(stroke::HAIRLINE, theme::RISCO));
                ui.add_space(space::XS);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(FOOTER).font(theme::body(12.0)).color(theme::NEVOA));
                });
            });
        if close {
            self.calladex = None;
        }
    }
}

/// The lens and the three lights, the title, the count and ✕. True when ✕ was clicked.
fn header(ui: &mut egui::Ui, dex: &Calladex) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(64.0, 30.0), Sense::hover());
        let p = ui.painter();
        let lens = Pos2::new(rect.left() + 15.0, rect.center().y);
        p.circle_filled(lens, 14.0, theme::GIZ);
        p.circle_filled(lens, 11.5, theme::LINK);
        p.circle_filled(lens + Vec2::new(-4.0, -4.0), 3.0, Color32::from_white_alpha(150));
        for (i, c) in [theme::AO_VIVO, theme::ABAJUR, theme::ONLINE].into_iter().enumerate() {
            p.circle_filled(Pos2::new(rect.left() + 38.0 + i as f32 * 9.0, rect.top() + 6.0), 3.0, c);
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new("Calladex").font(theme::display(20.0)).color(theme::GIZ));
            let sub = match dex.flavour {
                Flavour::Credits => "Edição especial: os créditos".to_string(),
                Flavour::Members => {
                    let n = dex.entries.len();
                    let who = if n == 1 { "1 membro registrado".to_string() } else { format!("{n} membros registrados") };
                    if dex.server.is_empty() {
                        who
                    } else {
                        format!("{who} em {}", dex.server)
                    }
                }
            };
            ui.label(RichText::new(sub).font(theme::body(12.5)).color(theme::NEVOA));
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            close = ui_kit::icon_button(ui, ph::X, "Fechar (Esc)", IconState::Normal).clicked();
        });
    });
    close
}

/// One member: number, avatar in its ring, name and tipo, "Membro desde" and their line.
fn entry(ui: &mut egui::Ui, e: &Entry, picture: Option<egui::TextureId>) {
    egui::Frame::new().fill(theme::NOITE).corner_radius(theme::radius::BUTTON).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.add_sized(Vec2::new(38.0, AVATAR), egui::Label::new(RichText::new(number(e.number)).font(theme::semibold(13.0)).color(theme::NEVOA)));
            let outer = AVATAR + 2.0 * (stroke::RING_GAP + stroke::SPEAKING);
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(outer), Sense::hover());
            let (c, r) = (rect.center(), AVATAR / 2.0);
            match picture {
                Some(tex) => crate::avatars::paint_circle_image(ui.painter(), c, r, tex),
                None => crate::avatars::paint_initial(ui.painter(), c, r, &e.user),
            }
            ui_kit::paint_ring(ui.painter(), c, r, theme::NOITE, e.ring);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(e.user.shown_name()).font(theme::semibold(14.5)).color(theme::GIZ)).truncate());
                    // (The lamp stays for whoever speaks: the owner's pill is only a little brighter.)
                    let (fill, fg) = if e.user.role == Role::Owner { (theme::CAIXA, theme::GIZ) } else { (theme::MOVEL, theme::TEXTO) };
                    ui_kit::pill(ui, e.tipo, fill, fg);
                });
                match e.since_ms {
                    Some(ms) => {
                        ui.label(RichText::new(format!("Membro desde {}", crate::profile::format_date_pt(ms, local_offset_min()))).font(theme::body(12.5)).color(theme::NEVOA));
                    }
                    None => {
                        ui.label(RichText::new("Membro desde ???").font(theme::body(12.5)).color(theme::NEVOA))
                            .on_hover_text("Dado ainda não registrado: abra o perfil da pessoa uma vez e o Calladex fica sabendo.");
                    }
                }
                ui.label(RichText::new(e.line).font(theme::body(13.0)).italics().color(theme::TEXTO));
            });
        });
    });
    ui.add_space(space::S);
}

/// The credits flavour: the logo, the wordmark and the signature.
fn credits(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(space::M);
        brand::logo(ui, 64.0);
        ui.add_space(space::S);
        ui.label(RichText::new(brand::APP_NAME).font(theme::display(26.0)).color(theme::GIZ));
        ui.add_space(space::S);
        ui.label(RichText::new("Nº 000 · tipo: Criador").font(theme::semibold(13.0)).color(theme::GIZ));
        ui.add_space(space::S);
        ui.label(RichText::new(format!("Feito por {}", brand::OWNER_NAME)).font(theme::body(14.0)).color(theme::TEXTO));
        brand::email_link(ui, 14.0);
        ui.add_space(space::M);
        ui.add(egui::Label::new(RichText::new("Ainda não há membros por aqui. Entre num servidor e digite /calladex no chat.").font(theme::body(12.5)).color(theme::NEVOA)).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_command_opens_the_card() {
        assert!(is_command("/calladex"));
        assert!(is_command("  /calladex \n"), "trimmed like a sent message");
        for sent in [
            "calladex",
            "/calladex agora",
            "olha o /calladex",
            "o calladex é legal",
            "/Calladex",
            "/CALLADEX",
            "/calladexx",
            "//calladex",
            "/calla dex",
            "",
            "/",
        ] {
            assert!(!is_command(sent), "{sent:?} is an ordinary message and is sent");
        }
    }

    #[test]
    fn the_home_sequence_needs_all_the_letters_in_a_row() {
        let mut buf = String::new();
        assert!(!push_keys(&mut buf, "callade"));
        assert!(push_keys(&mut buf, "x"), "typed one key at a time");
        assert!(buf.is_empty(), "starts over after a hit");
        assert!(push_keys(&mut buf, "xyzCALLADEX"), "Caps Lock is fine, earlier letters too");
        assert!(!push_keys(&mut buf, "calla dex"));
        assert!(!push_keys(&mut buf, "callad"));
        assert!(!push_keys(&mut buf, "e"));
        assert!(buf.chars().count() <= SEQUENCE.len(), "the buffer never grows");
        let mut long = String::new();
        assert!(!push_keys(&mut long, &"a".repeat(10_000)));
        assert_eq!(long.chars().count(), SEQUENCE.len());
    }

    #[test]
    fn entries_are_numbered_in_join_order_with_their_tipo() {
        let u = |id, name: &str, role| User { id, username: name.into(), role, avatar: None, display_name: None };
        let people = vec![u(9, "lua", Role::Member), u(2, "dono", Role::Owner), u(5, "sol", Role::Moderator), u(2, "dono", Role::Owner), u(7, "rio", Role::Admin)];
        let list = entries(people, |id| (id == 5).then_some(1_700_000_000_000), |_| Color32::WHITE);
        let shown: Vec<(String, &str, &str)> = list.iter().map(|e| (number(e.number), e.user.username.as_str(), e.tipo)).collect();
        assert_eq!(
            shown,
            [("#001".into(), "dono", "Dono 👑"), ("#002".into(), "sol", "Moderador"), ("#003".into(), "rio", "Admin"), ("#004".into(), "lua", "Membro")]
        );
        assert_eq!(list[1].since_ms, Some(1_700_000_000_000));
        assert_eq!(list[0].since_ms, None);
        assert_eq!(number(42), "#042");
        assert_eq!(number(1234), "#1234");
    }

    #[test]
    fn the_line_is_deterministic_and_varied() {
        for id in [1, 2, 3, 99, -4, i64::MAX] {
            assert_eq!(fun_line(id), fun_line(id));
            assert!(LINES.contains(&fun_line(id)));
        }
        let distinct: std::collections::HashSet<_> = (1..=40).map(fun_line).collect();
        assert!(distinct.len() >= 8, "only {} different lines for 40 people", distinct.len());
        // On one card no line repeats while the list lasts, and the card is the same every time.
        let people = |n: i64| (1..=n).map(|id| User::new(id, format!("u{id}"))).collect::<Vec<_>>();
        let card = entries(people(12), |_| None, |_| Color32::WHITE);
        let lines: std::collections::HashSet<_> = card.iter().map(|e| e.line).collect();
        assert_eq!(lines.len(), 12);
        assert_eq!(card, entries(people(12), |_| None, |_| Color32::WHITE));
        assert_eq!(card[0].line, fun_line(1), "the first one always gets their own line");
        assert_eq!(entries(people(30), |_| None, |_| Color32::WHITE).len(), 30, "more people than lines");
    }

    #[test]
    fn the_footer_and_the_signature() {
        assert_eq!(FOOTER, "Calladex v1 · por Cezar Callado");
        assert_eq!(brand::OWNER_NAME, "Cezar Callado");
    }
}
