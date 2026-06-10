use std::sync::{Arc, RwLock};

use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor, Style};
use cursive::{Cursive, Printer, Vec2, View};
use unicode_width::UnicodeWidthStr;

use crate::command::{Command, MoveAmount, MoveMode};
use crate::commands::CommandResult;
use crate::lyrics::{LyricsManager, LyricsStatus};
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::ViewExt;

/// Screen that shows the lyrics for the currently playing track, following
/// playback when the lyrics are time-synced.
pub struct LyricsView {
    queue: Arc<Queue>,
    spotify: Spotify,
    manager: Arc<LyricsManager>,
    /// Top visible line when scrolling manually.
    scroll: RwLock<usize>,
    /// Whether the view auto-follows the currently sung line.
    follow: RwLock<bool>,
    last_size: RwLock<Vec2>,
}

impl LyricsView {
    pub fn new(queue: Arc<Queue>, spotify: Spotify, manager: Arc<LyricsManager>) -> Self {
        Self {
            queue,
            spotify,
            manager,
            scroll: RwLock::new(0),
            follow: RwLock::new(true),
            last_size: RwLock::new(Vec2::zero()),
        }
    }

    /// All lines to display, plus the index of the currently sung line for
    /// synced lyrics.
    fn lines(&self) -> (Vec<String>, Option<usize>) {
        let Some(playable) = self.queue.get_current() else {
            return (vec!["Nothing is playing".into()], None);
        };
        let Some(track_id) = playable.id() else {
            return (vec!["No lyrics for local tracks".into()], None);
        };

        match self.manager.status(&track_id) {
            None | Some(LyricsStatus::Loading) => (vec!["Fetching lyrics…".into()], None),
            Some(LyricsStatus::NotFound) => (vec!["No lyrics found".into()], None),
            Some(LyricsStatus::Found(lyrics)) => {
                if let Some(synced) = &lyrics.synced {
                    let progress_ms = self.spotify.get_current_progress().as_millis() as u32;
                    let current = synced.partition_point(|line| line.time_ms <= progress_ms);
                    let current = current.checked_sub(1);
                    (synced.iter().map(|line| line.text.clone()).collect(), current)
                } else if let Some(plain) = &lyrics.plain {
                    (plain.lines().map(String::from).collect(), None)
                } else {
                    (vec!["No lyrics found".into()], None)
                }
            }
        }
    }

    fn scroll_by(&self, delta: i32) {
        let (lines, _) = self.lines();
        let height = self.last_size.read().unwrap().y.max(1);
        let max_top = lines.len().saturating_sub(height);
        let mut scroll = self.scroll.write().unwrap();
        *scroll = scroll.saturating_add_signed(delta as isize).min(max_top);
        *self.follow.write().unwrap() = false;
    }
}

impl View for LyricsView {
    fn draw(&self, printer: &Printer) {
        let (lines, current) = self.lines();
        let height = printer.size.y;

        let top = if *self.follow.read().unwrap() {
            // Keep the current line vertically centered.
            current
                .unwrap_or(0)
                .saturating_sub(height / 2)
                .min(lines.len().saturating_sub(height))
        } else {
            *self.scroll.read().unwrap()
        };

        let current_style = Style::from(ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("playing").unwrap()),
            ColorType::Color(*printer.theme.palette.custom("playing_bg").unwrap()),
        ))
        .combine(Effect::Bold);
        let other_style = ColorStyle::new(
            ColorType::Palette(PaletteColor::Primary),
            ColorType::Palette(PaletteColor::Background),
        );

        for (row, line) in lines.iter().skip(top).take(height).enumerate() {
            let index = top + row;
            let x = (printer.size.x.saturating_sub(line.width())) / 2;
            if current == Some(index) {
                printer.with_style(current_style, |printer| {
                    printer.print((x, row), line);
                });
            } else {
                printer.with_color(other_style, |printer| {
                    printer.print((x, row), line);
                });
            }
        }
    }

    fn layout(&mut self, size: Vec2) {
        *self.last_size.write().unwrap() = size;
    }

    fn needs_relayout(&self) -> bool {
        false
    }
}

impl ViewExt for LyricsView {
    fn title(&self) -> String {
        "Lyrics".to_string()
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Move(mode, amount) => {
                let delta = match amount {
                    MoveAmount::Integer(amount) => *amount,
                    MoveAmount::Extreme => i32::MAX / 2,
                    MoveAmount::Float(_) => return Ok(CommandResult::Ignored),
                };
                match mode {
                    MoveMode::Up => {
                        self.scroll_by(-delta);
                        Ok(CommandResult::Consumed(None))
                    }
                    MoveMode::Down => {
                        self.scroll_by(delta);
                        Ok(CommandResult::Consumed(None))
                    }
                    MoveMode::Playing => {
                        *self.follow.write().unwrap() = true;
                        Ok(CommandResult::Consumed(None))
                    }
                    _ => Ok(CommandResult::Ignored),
                }
            }
            Command::Back => {
                let mut follow = self.follow.write().unwrap();
                if *follow {
                    Ok(CommandResult::Ignored)
                } else {
                    // First Back re-enables auto-follow, a second one leaves the view.
                    *follow = true;
                    Ok(CommandResult::Consumed(None))
                }
            }
            _ => Ok(CommandResult::Ignored),
        }
    }
}
