use std::sync::{Arc, RwLock};

use cursive::event::{Event, EventResult, EventTrigger, Key, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor, Style};
use cursive::view::Position;
use cursive::views::OnEventView;
use cursive::{Cursive, Printer, Vec2, View};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::command::{Command, MoveAmount, MoveMode};
use crate::commands::CommandResult;
use crate::config::{Config, IconKind, LyricsAlign};
use crate::lyrics::{LyricsManager, LyricsStatus};
use crate::model::playable::Playable;
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::ViewExt;
use crate::ui::browser::{DropdownBorder, PaneDropdownMenu};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LyricsSection {
    Lyrics,
    Queue,
}

impl LyricsSection {
    fn label(self) -> &'static str {
        match self {
            Self::Lyrics => "Lyrics",
            Self::Queue => "Queue",
        }
    }
}

/// Screen that shows the lyrics for the currently playing track, following
/// playback when the lyrics are time-synced. Its title is a dropdown that
/// switches between the Lyrics and Queue sections.
pub struct LyricsView {
    queue: Arc<Queue>,
    spotify: Spotify,
    manager: Arc<LyricsManager>,
    cfg: Arc<Config>,
    /// Top visible line when scrolling manually.
    scroll: RwLock<usize>,
    /// Whether the view auto-follows the currently sung line.
    follow: RwLock<bool>,
    last_size: RwLock<Vec2>,
    section: LyricsSection,
    /// Absolute screen offset of the content area; captured during draw for dropdown anchoring.
    last_offset: RwLock<Vec2>,
    last_content_w: RwLock<usize>,
}

impl LyricsView {
    pub fn new(
        queue: Arc<Queue>,
        spotify: Spotify,
        manager: Arc<LyricsManager>,
        cfg: Arc<Config>,
    ) -> Self {
        Self {
            queue,
            spotify,
            manager,
            cfg,
            scroll: RwLock::new(0),
            follow: RwLock::new(true),
            last_size: RwLock::new(Vec2::zero()),
            section: LyricsSection::Lyrics,
            last_offset: RwLock::new(Vec2::zero()),
            last_content_w: RwLock::new(0),
        }
    }

    fn set_section(&mut self, section: LyricsSection) {
        self.section = section;
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

    fn align(&self) -> LyricsAlign {
        self.cfg
            .values()
            .lyrics
            .as_ref()
            .and_then(|lyrics| lyrics.align)
            .unwrap_or_default()
    }

    /// The display rows after wrapping all logical lines to `wrap_width`,
    /// plus the row to keep vertically centered (the middle row of the
    /// currently sung line). Each row carries the index of its logical line
    /// and whether it is that line's first row.
    fn display_rows(&self, wrap_width: usize) -> (Vec<DisplayRow>, Option<usize>, Option<usize>) {
        let (lines, current) = self.lines();

        let mut rows = Vec::with_capacity(lines.len());
        let mut current_center_row = None;
        for (logical, line) in lines.iter().enumerate() {
            let segments = wrap_line(line, wrap_width);
            if current == Some(logical) {
                current_center_row = Some(rows.len() + (segments.len() - 1) / 2);
            }
            rows.extend(segments.into_iter().enumerate().map(|(i, text)| DisplayRow {
                logical,
                first: i == 0,
                text,
            }));
        }
        (rows, current, current_center_row)
    }

    fn scroll_by(&self, delta: i32) {
        let size = *self.last_size.read().unwrap();
        let gutter = self.cfg.icon(IconKind::LyricsCurrent).width();
        let (rows, _, _) = self.display_rows(size.x.saturating_sub(gutter).max(1));
        let height = size.y.max(1);
        let max_top = rows.len().saturating_sub(height / 2);
        let mut scroll = self.scroll.write().unwrap();
        *scroll = scroll.saturating_add_signed(delta as isize).min(max_top);
        *self.follow.write().unwrap() = false;
    }

    fn open_dropdown(&self) -> EventResult {
        let offset = *self.last_offset.read().unwrap();
        let content_w = *self.last_content_w.read().unwrap();
        let current = self.section;
        let cfg = self.cfg.clone();

        // Mirror the draw_island centering formula used in panes.rs.
        let title_padded_w = current.label().width() + 4;
        let left_fill = content_w.saturating_sub(title_padded_w) / 2;
        let anchor = Vec2::new(offset.x + left_fill, offset.y);

        EventResult::with_cb(move |s: &mut Cursive| {
            let sections = [LyricsSection::Lyrics, LyricsSection::Queue];
            let items: Vec<(String, LyricsSection)> = sections
                .iter()
                .map(|&sec| {
                    let icon = cfg.icon(match sec {
                        LyricsSection::Lyrics => IconKind::MenuLyrics,
                        LyricsSection::Queue => IconKind::MenuQueue,
                    });
                    (format!(" {icon}{} ", sec.label()), sec)
                })
                .collect();
            let focus = sections.iter().position(|&sec| sec == current).unwrap_or(0);
            let menu_view = PaneDropdownMenu::new(items, |s, section| {
                s.pop_layer();
                s.call_on_name("lyrics_pane", move |view: &mut Self| {
                    view.set_section(section);
                });
            })
            .with_focus(focus);

            let menu = OnEventView::new(DropdownBorder::new(menu_view))
                .on_event(Key::Esc, |s| { s.pop_layer(); })
                .on_event(Event::Char('q'), |s| { s.pop_layer(); })
                .on_event(
                    EventTrigger::from_fn(|e| {
                        matches!(e, Event::Mouse { event: MouseEvent::Press(_), .. })
                    }),
                    |s| { s.pop_layer(); },
                );
            s.screen_mut().add_layer_at(Position::absolute(anchor), menu);
        })
    }

    fn draw_lyrics(&self, printer: &Printer) {
        let height = printer.size.y;
        let align = self.align();

        let icon = self.cfg.icon(IconKind::LyricsCurrent);
        let gutter = icon.width();
        let wrap_width = printer.size.x.saturating_sub(gutter).max(1);

        let (rows, current, current_center_row) = self.display_rows(wrap_width);
        let gutter = if current.is_some() { gutter } else { 0 };

        let top = if *self.follow.read().unwrap() {
            current_center_row.unwrap_or(0).saturating_sub(height / 2)
        } else {
            *self.scroll.read().unwrap()
        };

        let current_style = Style::from(ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("playing").unwrap()),
            ColorType::Color(*printer.theme.palette.custom("playing_bg").unwrap()),
        ))
        .combine(Effect::Bold);
        let past_style = Style::from(ColorStyle::new(
            ColorType::Palette(PaletteColor::Primary),
            ColorType::Palette(PaletteColor::Primary),
        ))
        .combine(Effect::Dim);
        let upcoming_style = Style::from(ColorStyle::new(
            ColorType::Palette(PaletteColor::Background),
            ColorType::Palette(PaletteColor::Background),
        ));

        for (row, display_row) in rows.iter().skip(top).take(height).enumerate() {
            let x = match align {
                LyricsAlign::Left => gutter,
                LyricsAlign::Center => {
                    ((printer.size.x.saturating_sub(display_row.text.width())) / 2).max(gutter)
                }
                LyricsAlign::Right => printer
                    .size
                    .x
                    .saturating_sub(display_row.text.width())
                    .max(gutter),
            };

            let style = match current {
                Some(current) if display_row.logical == current => current_style,
                Some(current) if display_row.logical > current => upcoming_style,
                Some(_) | None => past_style,
            };

            printer.with_style(style, |printer| {
                if display_row.first && current == Some(display_row.logical) {
                    printer.print((0, row), &icon);
                }
                printer.print((x, row), &display_row.text);
            });
        }
    }

    fn draw_queue(&self, printer: &Printer) {
        let queue = self.queue.queue.read().unwrap();
        let current = self.queue.get_current_index();

        let start = current.map(|i| i + 1).unwrap_or(0);
        let items: Vec<&Playable> = queue.iter().skip(start).collect();

        if items.is_empty() {
            printer.with_color(ColorStyle::secondary(), |p| {
                let text = "Nothing coming up";
                let x = p.size.x.saturating_sub(text.width()) / 2;
                p.print((x, p.size.y / 2), text);
            });
            return;
        }

        for (row, item) in items.iter().enumerate() {
            if row >= printer.size.y {
                break;
            }

            let name = match item {
                Playable::Track(t) => {
                    if t.artists.is_empty() {
                        t.title.clone()
                    } else {
                        format!("{} - {}", t.title, t.artists.join(", "))
                    }
                }
                Playable::Episode(e) => e.name.clone(),
            };
            let duration = item.duration_str();
            let dur_w = duration.width();

            // Truncate name to leave room for " duration" on the right.
            let max_name_w = printer.size.x.saturating_sub(dur_w + 1);
            let mut name_w = 0usize;
            let mut truncated = String::new();
            for c in name.chars() {
                let cw = c.width().unwrap_or(0);
                if name_w + cw > max_name_w {
                    break;
                }
                name_w += cw;
                truncated.push(c);
            }

            printer.print((0, row), &truncated);
            let x = printer.size.x.saturating_sub(dur_w);
            printer.with_color(ColorStyle::secondary(), |p| {
                p.print((x, row), &duration);
            });
        }
    }
}

struct DisplayRow {
    /// Index of the logical lyrics line this row belongs to.
    logical: usize,
    /// Whether this is the first row of its logical line.
    first: bool,
    text: String,
}

/// Greedily word-wrap `line` to rows of at most `width` cells, hard-splitting
/// words that are wider than a full row. Empty lines yield one empty row.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut row_width = 0;

    for word in line.split_whitespace() {
        let word_width = word.width();
        if row_width > 0 && row_width + 1 + word_width > width {
            rows.push(String::new());
            row_width = 0;
        }

        if word_width > width {
            for character in word.chars() {
                let character_width = character.width().unwrap_or(0);
                if row_width > 0 && row_width + character_width > width {
                    rows.push(String::new());
                    row_width = 0;
                }
                rows.last_mut().unwrap().push(character);
                row_width += character_width;
            }
        } else {
            if row_width > 0 {
                rows.last_mut().unwrap().push(' ');
                row_width += 1;
            }
            rows.last_mut().unwrap().push_str(word);
            row_width += word_width;
        }
    }

    rows
}

impl View for LyricsView {
    fn draw(&self, printer: &Printer) {
        *self.last_offset.write().unwrap() = printer.offset;
        *self.last_content_w.write().unwrap() = printer.size.x;

        match self.section {
            LyricsSection::Lyrics => self.draw_lyrics(printer),
            LyricsSection::Queue => self.draw_queue(printer),
        }
    }

    fn layout(&mut self, size: Vec2) {
        *self.last_size.write().unwrap() = size;
    }

    fn needs_relayout(&self) -> bool {
        false
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if self.section == LyricsSection::Queue {
            return EventResult::Ignored;
        }
        match event {
            Event::Mouse {
                event: MouseEvent::WheelUp,
                ..
            } => {
                self.scroll_by(-2);
                EventResult::consumed()
            }
            Event::Mouse {
                event: MouseEvent::WheelDown,
                ..
            } => {
                self.scroll_by(2);
                EventResult::consumed()
            }
            _ => EventResult::Ignored,
        }
    }
}

impl ViewExt for LyricsView {
    fn title(&self) -> String {
        self.section.label().to_string()
    }

    fn has_title_action(&self) -> bool {
        true
    }

    fn on_title_action(&mut self) -> EventResult {
        self.open_dropdown()
    }

    /// Right-click re-centres the lyrics on the current line instead of
    /// opening the dropdown.
    fn on_right_click(&mut self) -> EventResult {
        *self.follow.write().unwrap() = true;
        EventResult::consumed()
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        if self.section == LyricsSection::Queue {
            return Ok(CommandResult::Ignored);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_short_line_untouched() {
        assert_eq!(wrap_line("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn wrap_at_word_boundaries() {
        assert_eq!(
            wrap_line("the quick brown fox jumps", 10),
            vec!["the quick", "brown fox", "jumps"]
        );
    }

    #[test]
    fn wrap_hard_splits_overlong_words() {
        assert_eq!(wrap_line("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_keeps_empty_lines() {
        assert_eq!(wrap_line("", 10), vec![""]);
    }

    #[test]
    fn wrap_exact_fit() {
        assert_eq!(wrap_line("ab cd", 5), vec!["ab cd"]);
        assert_eq!(wrap_line("ab cde", 5), vec!["ab", "cde"]);
    }
}
