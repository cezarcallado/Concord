//! Colour emoji (v1.1, card I8), a child module of `autotest`: `emoji-a` (one member, alone) writes
//! a message with emoji in its text, reacts to it with several, types emoji in the text box, then
//! opens the message's menu (the quick reactions) and the reaction picker. Each time it checks
//! that the colour pass ran (`theme::emoji::stats`: the atlas is up, texts on screen have
//! pictures). With `VOX_AUTOTEST_SHOT_DIR`, it saves egui's own picture of its window (its
//! framebuffer, nothing else of the screen) as `emoji-chips.png`, `emoji-menu.png` and
//! `emoji-picker.png`.
//!
//! Silent: `VOX_NO_PLAYBACK=1`, `VOX_NO_MIC=1`.

use std::time::Duration;

use eframe::egui;

use super::Auto;
use crate::app::{chat_extras, App};

/// The message: emoji with a picture, one asked as text (U+FE0E) and one the atlas does not have
/// (both stay the font's).
const TEXT: &str = "emoji no texto: 😂 😮 😢 ❤\u{fe0f} 👍 🔥 🎉 · como texto: ❤\u{fe0e} · sem figura: 🐙 fim";
/// Reactions put on it.
const REACTIONS: [&str; 5] = ["👍", "😂", "❤", "🔥", "😮"];

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST emoji: {what}");
}

/// Ask egui for a picture of the window (only with `VOX_AUTOTEST_SHOT_DIR`).
fn shot(ctx: &egui::Context, name: &str) {
    if std::env::var_os("VOX_AUTOTEST_SHOT_DIR").is_some() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(name.to_string())));
    }
}

/// Write the window pictures egui handed back this pass.
fn save_shots(ctx: &egui::Context) {
    let Some(dir) = std::env::var_os("VOX_AUTOTEST_SHOT_DIR") else { return };
    let shots: Vec<(String, std::sync::Arc<egui::ColorImage>)> = ctx.input(|i| {
        i.raw
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Screenshot { user_data, image, .. } => Some((user_data.data.as_ref()?.downcast_ref::<String>()?.clone(), image.clone())),
                _ => None,
            })
            .collect()
    });
    for (name, img) in shots {
        let path = std::path::Path::new(&dir).join(format!("{name}.png"));
        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        let res = image::RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, rgba).map(|i| i.save(&path));
        log(&format!("window picture {} ({}x{}, saved: {:?})", path.display(), img.size[0], img.size[1], res.map(|r| r.is_ok())));
    }
}

impl Auto {
    fn emoji_waited(&self, secs: f32) -> bool {
        self.step_at.elapsed() >= Duration::from_secs_f32(secs)
    }

    /// The colour pass ran over at least `texts` texts with emoji, with the atlas up.
    fn emoji_check(&self, ctx: &egui::Context, texts: usize, what: &str) -> Result<(), String> {
        match crate::theme::emoji::stats(ctx) {
            Some((true, n)) if n >= texts => {
                log(&format!("{what}: {n} texts drawn with colour emoji"));
                Ok(())
            }
            other => Err(format!("{what}: the colour emoji did not draw (atlas up, texts: {other:?}; wanted {texts})")),
        }
    }

    fn emoji_message(&self, app: &App) -> Option<(i64, i64, usize)> {
        let ch = Self::first_text(app)?;
        let marker = format!("[{}]", self.args.run_id);
        let m = app.logs.get(&ch)?.msgs.iter().find(|m| m.content.starts_with(TEXT) && m.content.ends_with(&marker))?;
        Some((ch, m.id, m.reactions.len()))
    }

    pub(super) fn run_emoji_a(&mut self, app: &mut App) -> Option<i32> {
        let ctx = app.repaint_ctx();
        save_shots(&ctx);
        ctx.request_repaint_after(Duration::from_millis(100));
        match self.step {
            0 => self.connect(app),
            1 => {
                if !self.logged_in(app) {
                    return if self.waited(30) { self.fail("could not log in") } else { None };
                }
                let Some(ch) = Self::first_text(app) else { return self.fail("no text channel") };
                app.select_text(ch);
                let text = format!("{TEXT} [{}]", self.args.run_id);
                Self::say(app, &text);
                self.goto(2, "message with emoji sent");
            }
            2 => {
                if let Some((ch, id, _)) = self.emoji_message(app) {
                    for e in REACTIONS {
                        app.react(chat_extras::Target::Channel(ch), id, e, true);
                    }
                    self.goto(3, "reacting");
                } else if self.waited(20) {
                    return self.fail("the message did not arrive");
                }
            }
            3 => {
                let n = self.emoji_message(app).map_or(0, |m| m.2);
                if n >= REACTIONS.len() {
                    // Emoji typed in the text box too.
                    app.input = "digitando 😂🎉👍".into();
                    self.goto(4, "reactions in: the chips");
                } else if self.waited(20) {
                    return self.fail(&format!("{n} of {} reactions arrived", REACTIONS.len()));
                }
            }
            4 if self.emoji_waited(1.5) => {
                // The message text, its chips and the text box.
                if let Err(e) = self.emoji_check(&ctx, 3, "message, chips and text box") {
                    return self.fail(&e);
                }
                shot(&ctx, "emoji-chips");
                self.goto(5, "the message's menu");
            }
            5 if self.emoji_waited(1.0) => {
                let Some((_, id, _)) = self.emoji_message(app) else { return self.fail("the message is gone") };
                let at = ctx.content_rect().center() - egui::vec2(80.0, 160.0);
                chat_extras::force_menu(&ctx, id, None, at);
                self.goto(6, "menu open");
            }
            6 if self.emoji_waited(1.5) => {
                if !egui::Popup::is_any_open(&ctx) {
                    return self.fail("the message menu did not open");
                }
                let quick = chat_extras::quick_reactions(&app.settings.recent_reactions);
                log(&format!("quick reactions: {quick:?}"));
                if let Some(e) = quick.iter().find(|e| !crate::theme::emoji::has_image(e)) {
                    return self.fail(&format!("the quick reaction {e} has no picture"));
                }
                if let Err(e) = self.emoji_check(&ctx, 4, "the quick reactions") {
                    return self.fail(&e);
                }
                shot(&ctx, "emoji-menu");
                self.goto(7, "closing the menu");
            }
            7 if self.emoji_waited(1.0) => {
                chat_extras::force_menu_off(&ctx);
                let Some((ch, id, _)) = self.emoji_message(app) else { return self.fail("the message is gone") };
                let at = ctx.content_rect().center() - egui::vec2(150.0, 150.0);
                app.open_reaction_picker(chat_extras::Target::Channel(ch), id, at);
                self.goto(8, "the reaction picker");
            }
            8 if self.emoji_waited(1.5) => {
                if let Err(e) = self.emoji_check(&ctx, chat_extras::REACTION_GRID.len(), "the reaction picker") {
                    return self.fail(&e);
                }
                shot(&ctx, "emoji-picker");
                self.goto(9, "saving");
            }
            9 if self.emoji_waited(2.0) => {
                app.input.clear();
                return self.pass();
            }
            _ => {}
        }
        None
    }
}
