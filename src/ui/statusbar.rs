use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cursive::Printer;
use cursive::align::HAlign;
use cursive::event::{Event, EventResult, MouseButton, MouseEvent};
use cursive::theme::{Color, ColorStyle, ColorType, PaletteColor};
use cursive::traits::View;
use cursive::vec::Vec2;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::IconKind;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::model::track::Track;
use crate::queue::{Queue, RepeatSetting};
use crate::spotify::{PlayerEvent, Spotify};
use crate::utils::ms_to_hms;

/// One full left-to-right shimmer sweep over a suggested-track title.
const SHIMMER_CYCLE: Duration = Duration::from_millis(1800);
/// Redraw interval while a suggested track is playing (~6 fps).
const SHIMMER_FRAME: Duration = Duration::from_millis(160);

pub struct StatusBar {
    queue: Arc<Queue>,
    spotify: Spotify,
    library: Arc<Library>,
    last_size: Vec2,
    /// Time origin for the suggested-track title shimmer animation.
    shimmer_start: Instant,
}

impl StatusBar {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let spotify = queue.get_spotify();

        // Drive the title shimmer: while the current track is a recommendation,
        // nudge the UI to redraw at the animation frame rate. Idle (non-suggested)
        // it polls slowly so it costs almost nothing.
        {
            let queue = queue.clone();
            let library = library.clone();
            thread::spawn(move || {
                loop {
                    let animate = queue
                        .get_current()
                        .map(|t| t.is_suggested())
                        .unwrap_or(false);
                    if animate {
                        library.trigger_redraw();
                        thread::sleep(SHIMMER_FRAME);
                    } else {
                        thread::sleep(Duration::from_millis(300));
                    }
                }
            });
        }

        Self { queue, spotify, library, last_size: Vec2::new(0, 0), shimmer_start: Instant::now() }
    }

    fn bar_height(&self) -> usize {
        self.library
            .cfg
            .values()
            .statusbar
            .as_ref()
            .and_then(|s| s.height)
            .unwrap_or(1) as usize
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
        let indicators = if flipped { (pause.clone(), play.clone(), play) } else { (play, pause, stop) };
        match status {
            PlayerEvent::Playing(_) => indicators.0,
            PlayerEvent::Paused(_) => indicators.1,
            PlayerEvent::Stopped | PlayerEvent::FinishedTrack => indicators.2,
        }
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

    fn bar_chars(&self) -> (&'static str, &'static str) {
        let style = self
            .library
            .cfg
            .values()
            .statusbar
            .as_ref()
            .and_then(|s| s.style.clone())
            .unwrap_or_else(|| "thin".to_string());
        match style.as_str() {
            "thick" | "rounded" => ("█", "░"),
            _ => ("━", "┉"),
        }
    }

    fn shuffle_icon(&self) -> String {
        let state = self.library.cfg.state();
        if !state.shuffle {
            self.library.cfg.icon(IconKind::ShuffleOff)
        } else if state.smart_shuffle_visual {
            self.library.cfg.icon(IconKind::SmartShuffle)
        } else {
            self.library.cfg.icon(IconKind::Shuffle)
        }
    }

    fn repeat_icon(&self) -> String {
        match self.queue.get_repeat() {
            RepeatSetting::None => self.library.cfg.icon(IconKind::RepeatOff),
            RepeatSetting::RepeatPlaylist => self.library.cfg.icon(IconKind::Repeat),
            RepeatSetting::RepeatTrack => self.library.cfg.icon(IconKind::RepeatTrack),
        }
    }

    fn btn_open(&self) -> String {
        self.library.cfg.values().statusbar.as_ref()
            .and_then(|s| s.button_open.clone())
            .unwrap_or_else(|| "[ ".to_string())
    }

    fn btn_close(&self) -> String {
        self.library.cfg.values().statusbar.as_ref()
            .and_then(|s| s.button_close.clone())
            .unwrap_or_else(|| " ]".to_string())
    }

    fn btn_wrap(&self, icon: &str) -> String {
        format!("{}{}{}", self.btn_open(), icon, self.btn_close())
    }

    fn repeat_btn_str(&self) -> String {
        let open = self.btn_open();
        let close = self.btn_close();
        match self.queue.get_repeat() {
            RepeatSetting::RepeatTrack => {
                let icon = self.library.cfg.icon(IconKind::RepeatTrack);
                format!("{}{}1{}", open, icon.trim_end(), close)
            }
            _ => {
                let icon = self.repeat_icon();
                format!("{}{}{}", open, icon.trim_end(), close)
            }
        }
    }

    fn volume_pct(&self) -> u16 {
        (self.spotify.volume() as f64 / 65535_f64 * 100.0).round() as u16
    }

    /// Build the right-side status string: `{time} | {vol}%`
    fn right_str(&self) -> String {
        let elapsed = self.spotify.get_current_progress();
        let time = match self.queue.get_current() {
            Some(ref t) => {
                let e = ms_to_hms(elapsed.as_millis().try_into().unwrap_or(0));
                format!("{}/{}", e, t.duration_str())
            }
            None => String::new(),
        };
        let vol = format!("{}%", self.volume_pct());
        if time.is_empty() {
            format!("| {}", vol)
        } else {
            format!("{} | {}", time, vol)
        }
    }

    /// Build the controls string (left side of controls row) and return it
    /// along with the x-ranges for each clickable element.
    /// All ranges are relative to the inner content (after the leading │).
    fn controls_layout(&self) -> ControlsLayout {
        let play = self.playback_indicator();
        let prev = self.btn_wrap(self.library.cfg.icon(IconKind::PlayerPrev).trim_end());
        let next = self.btn_wrap(self.library.cfg.icon(IconKind::PlayerNext).trim_end());
        let shuffle = self.btn_wrap(self.shuffle_icon().trim_end());
        let repeat = self.repeat_btn_str();

        let mut x: usize = 2; // after leading │ + 1 cell padding

        let play_start = x;
        x += play.width();
        let play_end = x;

        x += 1; // space

        let prev_start = x;
        x += prev.width();
        let prev_end = x;

        x += 1; // space

        let next_start = x;
        x += next.width();
        let next_end = x;

        x += 1; // space

        let shuffle_start = x;
        x += shuffle.width();
        let shuffle_end = x;

        x += 2; // double space

        let repeat_start = x;
        x += repeat.width();
        let repeat_end = x;

        let left_str = format!(
            "{} {} {} {}  {}",
            play, prev, next, shuffle, repeat
        );

        let _ = (play_start, play_end);
        ControlsLayout {
            left_str,
            prev: (prev_start, prev_end),
            next: (next_start, next_end),
            shuffle: (shuffle_start, shuffle_end),
            repeat: (repeat_start, repeat_end),
        }
    }

    fn cycle_shuffle(&self) {
        let (shuffle, smart) = {
            let state = self.library.cfg.state();
            (state.shuffle, state.smart_shuffle_visual)
        };
        // Off → On → Smart → Off
        let (new_shuffle, new_smart) = if !shuffle {
            (true, false)
        } else if !smart {
            (true, true)
        } else {
            (false, false)
        };

        self.queue.set_shuffle(new_shuffle);
        self.library.cfg.with_state_mut(|s| s.smart_shuffle_visual = new_smart);

        // Shuffle operates on the current queue, which holds the playlist you
        // played (playing from a track list replaces the queue with that list).
        // Recommendations belong to smart shuffle only, so always strip them
        // first; plain shuffle/off then show just the playlist.
        self.queue.remove_suggested();

        if new_smart {
            // Smart shuffle: intersperse fresh Spotify recommendations into the
            // queue. Runs off-thread (network + locked queue writes).
            let queue = self.queue.clone();
            let spotify = self.spotify.clone();
            let library = self.library.clone();
            thread::spawn(move || {
                let seed_ids = queue.seed_track_ids(5);
                if seed_ids.is_empty() {
                    return;
                }
                let seed_refs: Vec<&str> = seed_ids.iter().map(|s| s.as_str()).collect();
                if let Ok(recs) = spotify.api.recommendations(None, None, Some(seed_refs)) {
                    let tracks: Vec<Playable> = recs
                        .tracks
                        .iter()
                        .map(|t| {
                            let mut track = Track::from(t);
                            track.is_suggested = true;
                            Playable::Track(track)
                        })
                        .collect();
                    // Single locked append minimizes contention with the playback thread.
                    queue.append_all(tracks);
                    // Reshuffle so recommendations are interspersed (Spotify-style),
                    // not stuck at the end of the play order.
                    queue.reshuffle();
                    library.trigger_redraw();
                }
            });
        }
    }

    fn cycle_repeat(&self) {
        let next = match self.queue.get_repeat() {
            RepeatSetting::None => RepeatSetting::RepeatPlaylist,
            RepeatSetting::RepeatPlaylist => RepeatSetting::RepeatTrack,
            RepeatSetting::RepeatTrack => RepeatSetting::None,
        };
        self.queue.set_repeat(next);
    }
}

struct ControlsLayout {
    left_str: String,
    prev: (usize, usize),
    next: (usize, usize),
    shuffle: (usize, usize),
    repeat: (usize, usize),
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
                *printer.theme.palette.custom("statusbar_progress_bg").unwrap(),
            ),
            ColorType::Palette(PaletteColor::Background),
        );
        let style = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("statusbar").unwrap()),
            ColorType::Color(*printer.theme.palette.custom("statusbar_bg").unwrap()),
        );
        let style_controls = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("statusbar_controls").unwrap()),
            ColorType::Color(*printer.theme.palette.custom("statusbar_controls_bg").unwrap()),
        );

        let bar_height = self.bar_height();
        let (filled_char, empty_char) = self.bar_chars();

        // inner_w: space between ╭/╰ and ╮/╯
        let inner_w = printer.size.x.saturating_sub(2);
        // bar_w: progress bar fill width (inner_w minus 1-cell padding each side)
        let bar_w = inner_w.saturating_sub(2);
        let bar_x: usize = 2; // │ + 1 padding

        // Elapsed progress fill
        let elapsed = self.spotify.get_current_progress();
        let elapsed_ms = elapsed.as_millis() as u32;
        let duration_fill: usize = if let Some(t) = self.queue.get_current() {
            elapsed_ms
                .checked_mul(bar_w as u32)
                .and_then(|v| v.checked_div(t.duration()))
                .unwrap_or(0) as usize
        } else {
            0
        };

        // Row 0: top border with centered title (with optional [N] prefix for suggested tracks)
        let current = self.queue.get_current();
        let is_suggested = current.as_ref().map(|t| t.is_suggested()).unwrap_or(false);
        let title = current.as_ref().map(|t| {
            let formatted = self.format_track(t);
            if t.is_suggested() {
                let tag = self.library.cfg.values().statusbar.as_ref()
                    .and_then(|s| s.suggested_tag.clone())
                    .unwrap_or_else(|| "[N] ".to_string());
                if !tag.is_empty() {
                    return format!("{}{}", tag, formatted);
                }
            }
            formatted
        });
        let top_border = if let Some(ref t) = title {
            let title_with_spaces = format!(" {} ", t);
            let tw = title_with_spaces.width();
            if tw + 2 <= inner_w {
                let left_dashes = (inner_w.saturating_sub(tw)) / 2;
                let right_dashes = inner_w.saturating_sub(tw).saturating_sub(left_dashes);
                format!(
                    "╭{}{}{}╮",
                    "─".repeat(left_dashes),
                    title_with_spaces,
                    "─".repeat(right_dashes)
                )
            } else {
                let max_title = inner_w.saturating_sub(4);
                let truncated: String = t.chars().take(max_title).collect();
                let t2 = format!(" {} ", truncated);
                let tw2 = t2.width();
                let left_dashes = (inner_w.saturating_sub(tw2)) / 2;
                let right_dashes = inner_w.saturating_sub(tw2).saturating_sub(left_dashes);
                format!(
                    "╭{}{}{}╮",
                    "─".repeat(left_dashes),
                    t2,
                    "─".repeat(right_dashes)
                )
            }
        } else {
            format!("╭{}╮", "─".repeat(inner_w))
        };
        printer.with_color(style, |p| {
            p.print((0, 0), &top_border);
        });

        // Shimmer: a soft bright band easing left→right across a recommended
        // track's title (Claude-CLI style). Overdraws the title glyphs already
        // painted above; only active for suggested tracks and when enabled.
        let shimmer_enabled = self
            .library
            .cfg
            .values()
            .statusbar
            .as_ref()
            .and_then(|s| s.suggested_shimmer)
            .unwrap_or(true);
        if is_suggested && shimmer_enabled {
            if let Some(ref t) = title {
                // Mirror the top_border layout to find the title's start column
                // and the actually displayed (possibly truncated) text.
                let full = format!(" {t} ");
                let (disp, tw) = if full.width() + 2 <= inner_w {
                    (t.clone(), full.width())
                } else {
                    let max_title = inner_w.saturating_sub(4);
                    let truncated: String = t.chars().take(max_title).collect();
                    let tw = format!(" {truncated} ").width();
                    (truncated, tw)
                };
                let left_dashes = inner_w.saturating_sub(tw) / 2;
                let title_start_x = 1 + left_dashes + 1; // ╭ + dashes + leading space

                // Glyphs render in the exact normal title color, except a soft
                // moving band that brightens toward white. The bright band is
                // grayscale (white shine) so no themed hue is ever interpolated —
                // avoids the brown/oversaturated tints a colored blend produced.
                let base_color = *printer.theme.palette.custom("statusbar").unwrap();
                let bg = *printer.theme.palette.custom("statusbar_bg").unwrap();

                let chars: Vec<char> = disp.chars().collect();
                let n = chars.len().max(1) as f32;
                let phase = (self.shimmer_start.elapsed().as_millis() as f32
                    % SHIMMER_CYCLE.as_millis() as f32)
                    / SHIMMER_CYCLE.as_millis() as f32;
                // Band center sweeps from before the first glyph to past the last.
                let sigma = (n * 0.18).clamp(1.5, 6.0);
                let center = phase * (n - 1.0 + 4.0 * sigma) - 2.0 * sigma;

                let mut x = title_start_x;
                for ch in chars {
                    let d = x as f32 - title_start_x as f32 - center;
                    let intensity = (-(d * d) / (2.0 * sigma * sigma)).exp();
                    let color = if intensity < 0.08 {
                        base_color
                    } else {
                        // 220..=255 grayscale: a subtle white shine at the band.
                        let v = (220.0 + 35.0 * intensity).round() as u8;
                        Color::Rgb(v, v, v)
                    };
                    let st = ColorStyle::new(ColorType::Color(color), ColorType::Color(bg));
                    let s = ch.to_string();
                    printer.with_color(st, |p| p.print((x, 0), &s));
                    x += ch.width().unwrap_or(1);
                }
            }
        }

        // Row 1: top padding row (│ space │)
        printer.with_color(style, |p| {
            p.print((0, 1), "│");
            p.print((1, 1), &" ".repeat(inner_w));
            p.print((printer.size.x.saturating_sub(1), 1), "│");
        });

        // Rows 2..=bar_height+1: progress bar
        for row in 2..=bar_height + 1 {
            printer.with_color(style, |p| {
                p.print((0, row), "│");
                p.print((1, row), " ");
                p.print((bar_x + bar_w, row), " ");
                p.print((printer.size.x.saturating_sub(1), row), "│");
            });
            printer.with_color(style_bar_bg, |p| {
                p.print((bar_x, row), &empty_char.repeat(bar_w));
            });
            if self.queue.get_current().is_some() {
                printer.with_color(style_bar, |p| {
                    p.print(
                        (bar_x, row),
                        &filled_char.repeat((duration_fill + 1).min(bar_w)),
                    );
                });
            }
        }

        // Row bar_height+2: padding row (between progress bar and controls)
        let pre_ctrl_pad = bar_height + 2;
        printer.with_color(style, |p| {
            p.print((0, pre_ctrl_pad), "│");
            p.print((1, pre_ctrl_pad), &" ".repeat(inner_w));
            p.print((printer.size.x.saturating_sub(1), pre_ctrl_pad), "│");
        });

        // Row bar_height+3: controls row
        let ctrl_row = bar_height + 3;
        // Border glyphs in border style, content in controls style.
        printer.with_color(style, |p| {
            p.print((0, ctrl_row), "│");
            p.print((printer.size.x.saturating_sub(1), ctrl_row), "│");
        });
        printer.with_color(style_controls, |p| {
            p.print((1, ctrl_row), &" ".repeat(inner_w));
        });

        let cl = self.controls_layout();
        printer.with_color(style_controls, |p| {
            p.print((2, ctrl_row), &cl.left_str);
        });

        // Right-align time/volume with 1-cell right padding before │
        let right = self.right_str();
        let right_area_w = printer.size.x.saturating_sub(2); // 1 for │, 1 for padding
        let offset = HAlign::Right.get_offset(right.width(), right_area_w);
        printer.with_color(style_controls, |p| {
            p.print((offset, ctrl_row), &right);
        });

        // Row bar_height+4: padding row (between controls and bottom border)
        let post_ctrl_pad = bar_height + 4;
        printer.with_color(style, |p| {
            p.print((0, post_ctrl_pad), "│");
            p.print((1, post_ctrl_pad), &" ".repeat(inner_w));
            p.print((printer.size.x.saturating_sub(1), post_ctrl_pad), "│");
        });

        // Bottom border
        let bottom_row = bar_height + 5;
        printer.with_color(style, |p| {
            p.print((0, bottom_row), &format!("╰{}╯", "─".repeat(inner_w)));
        });
    }

    fn layout(&mut self, size: Vec2) {
        self.last_size = size;
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        let bar_height = self.bar_height();
        // top border + top-pad + progress rows + mid-pad + controls row + post-pad + bottom border
        Vec2::new(constraint.x, bar_height + 6)
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if let Event::Mouse { offset, position, event } = event {
            let position = position - offset;
            let bar_height = self.bar_height();
            let inner_w = self.last_size.x.saturating_sub(2);

            // All scroll events change volume
            if event == MouseEvent::WheelUp || event == MouseEvent::WheelDown {
                let new_vol = if event == MouseEvent::WheelUp {
                    self.spotify.volume().saturating_add(crate::spotify::VOLUME_PERCENT)
                } else {
                    self.spotify.volume().saturating_sub(crate::spotify::VOLUME_PERCENT)
                };
                self.spotify.set_volume(new_vol, true);
                return EventResult::Consumed(None);
            }

            if event == MouseEvent::Press(MouseButton::Left) {
                let row = position.y;
                // bar_w: progress bar content width (inner_w - 2 padding cells)
                let bar_w = inner_w.saturating_sub(2);

                // Progress bar rows (2..=bar_height+1): click to seek
                if row >= 2 && row <= bar_height + 1 {
                    if let Some(playable) = self.queue.get_current() {
                        let click_x = position.x.saturating_sub(2); // skip │ + padding
                        let f: f32 = click_x as f32 / bar_w.max(1) as f32;
                        let new = playable.duration() as f32 * f;
                        self.spotify.seek(new as u32);
                    }
                    return EventResult::Consumed(None);
                }

                // Controls row (bar_height + 3): button hit testing
                if row == bar_height + 3 {
                    let cl = self.controls_layout();
                    let x = position.x;

                    if x >= cl.prev.0 && x < cl.prev.1 {
                        self.queue.previous();
                    } else if x >= cl.next.0 && x < cl.next.1 {
                        self.queue.next(true);
                    } else if x >= cl.shuffle.0 && x < cl.shuffle.1 {
                        self.cycle_shuffle();
                    } else if x >= cl.repeat.0 && x < cl.repeat.1 {
                        self.cycle_repeat();
                    } else {
                        // Click on play area or empty space → toggle pause
                        self.queue.toggleplayback();
                    }
                    return EventResult::Consumed(None);
                }

                // Top border or bottom border → toggle pause
                self.queue.toggleplayback();
                return EventResult::Consumed(None);
            }

            EventResult::Consumed(None)
        } else {
            EventResult::Ignored
        }
    }
}
