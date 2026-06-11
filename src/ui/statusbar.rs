use std::sync::Arc;

use cursive::Printer;
use cursive::align::HAlign;
use cursive::event::{Event, EventResult, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::traits::View;
use cursive::vec::Vec2;
use unicode_width::UnicodeWidthStr;

use crate::config::IconKind;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::queue::{Queue, RepeatSetting};
use crate::spotify::{PlayerEvent, Spotify};
use crate::utils::ms_to_hms;

pub struct StatusBar {
    queue: Arc<Queue>,
    spotify: Spotify,
    library: Arc<Library>,
    last_size: Vec2,
}

impl StatusBar {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let spotify = queue.get_spotify();

        Self {
            queue,
            spotify,
            library,
            last_size: Vec2::new(0, 0),
        }
    }

    fn playback_indicator(&self) -> String {
        let status = self.spotify.get_current_status();
        let cfg = &self.library.cfg;
        let flipped = cfg.values().flip_status_indicators.unwrap_or(false);

        let (play, pause, stop) = (
            cfg.icon(IconKind::Playing),
            cfg.icon(IconKind::Paused),
            cfg.icon(IconKind::Stopped),
        );
        // When flipped, show the icon of the action that pressing play/pause
        // would trigger instead of the current state.
        let indicators = if flipped {
            (pause.clone(), play.clone(), play)
        } else {
            (play, pause, stop)
        };

        match status {
            PlayerEvent::Playing(_) => indicators.0,
            PlayerEvent::Paused(_) => indicators.1,
            PlayerEvent::Stopped | PlayerEvent::FinishedTrack => indicators.2,
        }
    }

    fn volume_display(&self) -> String {
        format!(
            " [{}%]",
            (self.spotify.volume() as f64 / 65535_f64 * 100.0).round() as u16
        )
    }

    fn format_track(&self, t: &Playable) -> String {
        let format = self
            .library
            .cfg
            .values()
            .statusbar_format
            .clone()
            .unwrap_or_else(|| "%artists - %title".to_string());
        Playable::format(t, &format, &self.library)
    }
}

impl StatusBar {
    /// Returns `(bar_height, bar_row_start, info_row, has_rounded, has_caps)`.
    fn sb_layout(&self) -> (usize, usize, usize, bool, bool) {
        let (bar_height, border_str) = {
            let vals = self.library.cfg.values();
            let sb = vals.statusbar.as_ref();
            (
                sb.and_then(|s| s.height).unwrap_or(1) as usize,
                sb.and_then(|s| s.border.clone())
                    .unwrap_or_else(|| "none".to_string()),
            )
        };
        let has_rounded = border_str == "rounded";
        let has_caps = has_rounded || border_str == "icon";
        let bar_row_start: usize = if has_rounded { 1 } else { 0 };
        // +1 gap row between the progress bar and the info line
        (bar_height, bar_row_start, bar_row_start + bar_height + 1, has_rounded, has_caps)
    }
}

impl View for StatusBar {
    fn draw(&self, printer: &Printer<'_, '_>) {
        if printer.size.x == 0 {
            return;
        }

        let style_bar = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("statusbar_progress").unwrap()),
            ColorType::Palette(PaletteColor::Background),
        );
        let style_bar_bg = ColorStyle::new(
            ColorType::Color(
                *printer
                    .theme
                    .palette
                    .custom("statusbar_progress_bg")
                    .unwrap(),
            ),
            ColorType::Palette(PaletteColor::Background),
        );
        let style = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("statusbar").unwrap()),
            ColorType::Color(*printer.theme.palette.custom("statusbar_bg").unwrap()),
        );

        let bar_style_str = {
            let vals = self.library.cfg.values();
            vals.statusbar
                .as_ref()
                .and_then(|s| s.style.clone())
                .unwrap_or_else(|| "thin".to_string())
        };

        let (filled_char, empty_char): (&str, &str) =
            if bar_style_str == "thick" { ("█", "░") } else { ("━", "┉") };

        let (bar_height, bar_row_start, info_row, has_rounded, has_caps) = self.sb_layout();

        // Inner bar width (leaves room for │ end caps when present)
        let inner_w = printer.size.x.saturating_sub(if has_caps { 2 } else { 0 });
        let bar_left: usize = if has_caps { 1 } else { 0 };

        // Elapsed progress fill width
        let elapsed = self.spotify.get_current_progress();
        let elapsed_ms = elapsed.as_millis() as u32;
        let duration_fill: usize = if let Some(t) = self.queue.get_current() {
            elapsed_ms
                .checked_mul(inner_w as u32)
                .and_then(|v| v.checked_div(t.duration()))
                .unwrap_or(0) as usize
        } else {
            0
        };

        // Top border
        if has_rounded {
            printer.print((0, 0), &format!("╭{}╮", "─".repeat(inner_w)));
        }

        // Progress bar row(s)
        for row in bar_row_start..(bar_row_start + bar_height) {
            printer.with_color(style_bar_bg, |p| {
                p.print((bar_left, row), &empty_char.repeat(inner_w));
            });
            if self.queue.get_current().is_some() {
                printer.with_color(style_bar, |p| {
                    p.print(
                        (bar_left, row),
                        &filled_char.repeat((duration_fill + 1).min(inner_w)),
                    );
                });
            }
            if has_caps {
                printer.print((0, row), "│");
                printer.print((printer.size.x.saturating_sub(1), row), "│");
            }
        }

        // Gap row (space between progress bar and info line)
        let gap_row = info_row - 1;
        if has_rounded {
            printer.print((0, gap_row), "│");
            printer.with_color(style, |p| {
                p.print((1, gap_row), &" ".repeat(inner_w));
            });
            printer.print((printer.size.x.saturating_sub(1), gap_row), "│");
        } else {
            printer.with_color(style, |p| {
                p.print((0, gap_row), &" ".repeat(printer.size.x));
            });
        }

        // Info row background
        if has_rounded {
            printer.print((0, info_row), "│");
            printer.with_color(style, |p| {
                p.print((1, info_row), &" ".repeat(inner_w));
            });
            printer.print((printer.size.x.saturating_sub(1), info_row), "│");
        } else {
            printer.with_color(style, |p| {
                p.print((0, info_row), &" ".repeat(printer.size.x));
            });
        }

        // Playback indicator + track name
        printer.with_color(style, |printer| {
            printer.print((1, info_row), &self.playback_indicator());
        });

        // Right-side status text
        let updating = if !*self.library.is_done.read().unwrap() {
            self.library.cfg.icon(IconKind::Updating)
        } else {
            String::new()
        };
        let repeat = match self.queue.get_repeat() {
            RepeatSetting::None => String::new(),
            RepeatSetting::RepeatPlaylist => self.library.cfg.icon(IconKind::Repeat),
            RepeatSetting::RepeatTrack => self.library.cfg.icon(IconKind::RepeatTrack),
        };
        let shuffle = if self.queue.get_shuffle() {
            self.library.cfg.icon(IconKind::Shuffle)
        } else {
            String::new()
        };
        let volume = self.volume_display();
        let formatted_elapsed = ms_to_hms(elapsed.as_millis().try_into().unwrap_or(0));
        let playback_duration_status = match self.queue.get_current() {
            Some(ref t) => format!("{} / {}", formatted_elapsed, t.duration_str()),
            None => String::new(),
        };
        let right = updating + &repeat + &shuffle + &playback_duration_status + &volume;

        // Align right before the closing │ when using rounded border
        let right_area_w = if has_rounded {
            printer.size.x.saturating_sub(1)
        } else {
            printer.size.x
        };
        let offset = HAlign::Right.get_offset(right.width(), right_area_w);

        printer.with_color(style, |printer| {
            if let Some(ref t) = self.queue.get_current() {
                printer.print((4, info_row), &self.format_track(t));
            }
            printer.print((offset, info_row), &right);
        });

        // Bottom border
        if has_rounded {
            printer.print((0, info_row + 1), &format!("╰{}╯", "─".repeat(inner_w)));
        }
    }

    fn layout(&mut self, size: Vec2) {
        self.last_size = size;
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        let (bar_height, _bar_row_start, _info_row, has_rounded, _has_caps) = self.sb_layout();
        let border_rows: usize = if has_rounded { 2 } else { 0 };
        Vec2::new(constraint.x, bar_height + 2 + border_rows)
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if let Event::Mouse {
            offset,
            position,
            event,
        } = event
        {
            let position = position - offset;
            let (bar_height, bar_row_start, info_row, _has_rounded, has_caps) = self.sb_layout();
            let inner_w = self.last_size.x.saturating_sub(if has_caps { 2 } else { 0 });
            let bar_left: usize = if has_caps { 1 } else { 0 };
            let volume_len = self.volume_display().len();

            if position.y >= bar_row_start && position.y < bar_row_start + bar_height {
                // Progress bar rows — seek
                if event == MouseEvent::WheelUp {
                    self.spotify.seek_relative(-500);
                }
                if event == MouseEvent::WheelDown {
                    self.spotify.seek_relative(500);
                }
                if event == MouseEvent::Press(MouseButton::Left) {
                    if let Some(playable) = self.queue.get_current() {
                        let click_x = position.x.saturating_sub(bar_left);
                        let f: f32 = click_x as f32 / inner_w.max(1) as f32;
                        let new = playable.duration() as f32 * f;
                        self.spotify.seek(new as u32);
                    }
                }
            } else if position.y == info_row {
                // Info row — volume scroll on right edge, playback toggle elsewhere
                if self.last_size.x.saturating_sub(position.x) < volume_len {
                    if event == MouseEvent::WheelUp {
                        let volume = self
                            .spotify
                            .volume()
                            .saturating_add(crate::spotify::VOLUME_PERCENT);
                        self.spotify.set_volume(volume, true);
                    }
                    if event == MouseEvent::WheelDown {
                        let volume = self
                            .spotify
                            .volume()
                            .saturating_sub(crate::spotify::VOLUME_PERCENT);
                        self.spotify.set_volume(volume, true);
                    }
                } else if event == MouseEvent::Press(MouseButton::Left) {
                    self.queue.toggleplayback();
                }
            }

            EventResult::Consumed(None)
        } else {
            EventResult::Ignored
        }
    }
}
