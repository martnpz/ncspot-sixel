use std::sync::Arc;
use std::sync::RwLock;
use std::thread;
use std::time::{Duration, Instant};

use cursive::Printer;
use cursive::align::HAlign;
use cursive::event::{Event, EventResult, MouseButton, MouseEvent};
use cursive::theme::{Color, ColorStyle, ColorType, Effect, PaletteColor, Style};
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
/// How long a clicked play/prev/next button flashes the active color.
const BLINK: Duration = Duration::from_millis(140);

/// A clickable control-row button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlButton {
    Play,
    Prev,
    Next,
    Shuffle,
    Repeat,
}

/// A laid-out control button: its icon, active state, and x-range `[start, end)`.
struct CtrlBtn {
    kind: ControlButton,
    icon: String,
    active: bool,
    start: usize,
    end: usize,
}

pub struct StatusBar {
    queue: Arc<Queue>,
    spotify: Spotify,
    library: Arc<Library>,
    last_size: Vec2,
    /// Time origin for the suggested-track title shimmer animation.
    shimmer_start: Instant,
    /// Last clicked play/prev/next button and when, for the press blink.
    blink: Option<(ControlButton, Instant)>,
    /// Last terminal-title string written, to avoid redundant OSC writes.
    last_title: RwLock<Option<String>>,
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
            let spotify = spotify.clone();
            thread::spawn(move || {
                loop {
                    // Only animate while a suggested track is actually playing.
                    // Paused/stopped → nothing changes, so idle-poll cheaply.
                    let playing =
                        matches!(spotify.get_current_status(), PlayerEvent::Playing(_));
                    let animate = playing
                        && queue
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

        Self {
            queue,
            spotify,
            library,
            last_size: Vec2::new(0, 0),
            shimmer_start: Instant::now(),
            blink: None,
            last_title: RwLock::new(None),
        }
    }

    /// Update the terminal window title to reflect the current track, honoring
    /// the `title_format` config. Only writes when the title actually changes.
    fn update_terminal_title(&self) {
        let format = self
            .library
            .cfg
            .values()
            .title_format
            .clone()
            .unwrap_or_else(|| "ncspot - %title".to_string());
        // Empty format disables title management entirely.
        if format.is_empty() {
            return;
        }
        let title = match self.queue.get_current() {
            Some(ref t) => Playable::format(t, &format, &self.library),
            None => "ncspot".to_string(),
        };
        let changed = self.last_title.read().unwrap().as_deref() != Some(title.as_str());
        if changed {
            crate::utils::set_terminal_title(&title);
            *self.last_title.write().unwrap() = Some(title);
        }
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

    fn bar_style(&self) -> String {
        self.library
            .cfg
            .values()
            .statusbar
            .as_ref()
            .and_then(|s| s.style.clone())
            .unwrap_or_else(|| "square".to_string())
    }

    fn bar_chars(&self) -> (&'static str, &'static str) {
        match self.bar_style().as_str() {
            "thin" => ("━", "┉"),
            // "square" (default, █/░); "rounded" adds caps; "thick" legacy alias.
            _ => ("█", "░"),
        }
    }

    /// End caps for the `"rounded"` style: `(left, right)`. None for other styles.
    fn bar_caps(&self) -> Option<(String, String)> {
        if self.bar_style() != "rounded" {
            return None;
        }
        let sb = self.library.cfg.values().statusbar.clone();
        let left = sb
            .as_ref()
            .and_then(|s| s.progress_cap_left.clone())
            .unwrap_or_else(|| "\u{e0b6}".to_string());
        let right = sb
            .as_ref()
            .and_then(|s| s.progress_cap_right.clone())
            .unwrap_or_else(|| "\u{e0b4}".to_string());
        Some((left, right))
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

    fn controls_style(&self) -> String {
        self.library.cfg.values().statusbar.as_ref()
            .and_then(|s| s.controls_style.clone())
            .unwrap_or_else(|| "square".to_string())
    }

    fn controls_cap_left(&self) -> String {
        self.library.cfg.values().statusbar.as_ref()
            .and_then(|s| s.controls_cap_left.clone())
            .unwrap_or_else(|| "\u{e0b6}".to_string())
    }

    fn controls_cap_right(&self) -> String {
        self.library.cfg.values().statusbar.as_ref()
            .and_then(|s| s.controls_cap_right.clone())
            .unwrap_or_else(|| "\u{e0b4}".to_string())
    }

    /// Left/right button decorations for the current controls style.
    fn controls_deco(&self) -> (String, String) {
        if self.controls_style() == "rounded" {
            (self.controls_cap_left(), self.controls_cap_right())
        } else {
            (self.btn_open(), self.btn_close())
        }
    }

    fn repeat_icon_str(&self) -> String {
        match self.queue.get_repeat() {
            RepeatSetting::RepeatTrack => {
                format!("{}1", self.library.cfg.icon(IconKind::RepeatTrack).trim_end())
            }
            _ => self.repeat_icon().to_string(),
        }
    }

    /// Record a press blink for `btn` and schedule a redraw to clear it.
    fn start_blink(&mut self, btn: ControlButton) {
        self.blink = Some((btn, Instant::now()));
        let library = self.library.clone();
        thread::spawn(move || {
            thread::sleep(BLINK);
            library.trigger_redraw();
        });
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
                format!("{} / {}", e, t.duration_str())
            }
            None => String::new(),
        };
        let vol = format!("{}%", self.volume_pct());
        if time.is_empty() {
            format!("|   {}", vol)
        } else {
            format!("󰔚 {} | 󰖀  {}", time, vol)
        }
    }

    /// Lay out the control buttons left to right, computing each one's x-range.
    /// Decoration width (brackets or pill caps) depends on the controls style.
    fn controls_layout(&self) -> Vec<CtrlBtn> {
        let (deco_l, deco_r) = self.controls_deco();
        let deco_w = deco_l.width() + deco_r.width();

        let shuffle_on = self.library.cfg.state().shuffle;
        let repeat_on = self.queue.get_repeat() != RepeatSetting::None;

        let specs: [(ControlButton, String, bool); 5] = [
            (ControlButton::Play, self.playback_indicator().to_string(), false),
            (ControlButton::Prev, self.library.cfg.icon(IconKind::PlayerPrev).to_string(), false),
            (ControlButton::Next, self.library.cfg.icon(IconKind::PlayerNext).to_string(), false),
            (ControlButton::Shuffle, self.shuffle_icon().to_string(), shuffle_on),
            (ControlButton::Repeat, self.repeat_icon_str(), repeat_on),
        ];

        let mut out = Vec::with_capacity(specs.len());
        let mut x: usize = 2; // after leading │ + 1 cell padding
        for (i, (kind, icon, active)) in specs.into_iter().enumerate() {
            let start = x;
            let end = x + deco_w + icon.width();
            out.push(CtrlBtn { kind, icon, active, start, end });
            // Gaps after each button: play␣␣prev␣next␣␣shuffle␣␣repeat
            let gap = match i {
                0 => 2, // after play
                1 => 1, // after prev
                2 => 2, // after next
                3 => 2, // after shuffle
                _ => 1,
            };
            x = end + gap;
        }
        out
    }

    /// Draw the control buttons onto the controls row.
    fn draw_controls(&self, printer: &Printer, row: usize, btns: &[CtrlBtn]) {
        let rounded = self.controls_style() == "rounded";
        let (deco_l, deco_r) = self.controls_deco();

        // Normal icon color follows the pane island style (TitleSecondary).
        let fg = printer.theme.palette[PaletteColor::TitlePrimary];
        let active_fg = *printer.theme.palette.custom("statusbar_controls_active").unwrap();
        let pill_bg = *printer.theme.palette.custom("statusbar_controls_button_bg").unwrap();
        let row_bg = *printer.theme.palette.custom("statusbar_controls_bg").unwrap();

        // Active blink (play/prev/next), still within the blink window.
        let blink = self.blink.filter(|(_, t)| t.elapsed() < BLINK).map(|(b, _)| b);

        for b in btns {
            // When a button is "on" the icon's fg/bg swap (inverted look):
            //   - play/prev/next: briefly, on click, using the active color (blink)
            //   - shuffle/repeat: while enabled, using the normal fg color
            let highlight = match b.kind {
                ControlButton::Shuffle | ControlButton::Repeat => b.active.then_some(fg),
                _ => (blink == Some(b.kind)).then_some(active_fg),
            };

            if rounded {
                // Normally the pill body is pill_bg with the icon in fg. When lit,
                // the body becomes the highlight color and the glyph inverts to it.
                let (body, glyph_fg) = match highlight {
                    Some(hl) => (hl, pill_bg),
                    None => (pill_bg, fg),
                };
                let deco_style =
                    ColorStyle::new(ColorType::Color(body), ColorType::Color(row_bg));
                let icon_style =
                    ColorStyle::new(ColorType::Color(glyph_fg), ColorType::Color(body));
                printer.with_color(deco_style, |p| p.print((b.start, row), &deco_l));
                let ix = b.start + deco_l.width();
                printer.with_color(icon_style, |p| p.print((ix, row), &b.icon));
                printer.with_color(deco_style, |p| {
                    p.print((ix + b.icon.width(), row), &deco_r)
                });
            } else {
                // Square: swap glyph fg with the row background when lit.
                let (glyph_fg, bg) = match highlight {
                    Some(hl) => (row_bg, hl),
                    None => (fg, row_bg),
                };
                let st = ColorStyle::new(ColorType::Color(glyph_fg), ColorType::Color(bg));
                let s = format!("{}{}{}", deco_l, b.icon, deco_r);
                printer.with_color(st, |p| p.print((b.start, row), &s));
            }
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

        log::info!("SS: cycle_shuffle start (new_shuffle={new_shuffle}, new_smart={new_smart})");
        self.queue.set_shuffle(new_shuffle);
        log::info!("SS: set_shuffle done");
        self.library.cfg.with_state_mut(|s| s.smart_shuffle_visual = new_smart);
        log::info!("SS: with_state_mut done");

        // Shuffle operates on the current queue, which holds the playlist you
        // played (playing from a track list replaces the queue with that list).
        // Recommendations belong to smart shuffle only, so always strip them
        // first; plain shuffle/off then show just the playlist.
        self.queue.remove_suggested();
        log::info!("SS: remove_suggested done");

        if new_smart {
            // Smart shuffle: intersperse fresh Spotify recommendations into the
            // queue. Runs off-thread (network + locked queue writes).
            let queue = self.queue.clone();
            let spotify = self.spotify.clone();
            let library = self.library.clone();
            thread::spawn(move || {
                log::info!("SS: bg thread seeding");
                let seed_ids = queue.seed_track_ids(5);
                log::info!("SS: seeds={}", seed_ids.len());
                if seed_ids.is_empty() {
                    return;
                }
                let seed_refs: Vec<&str> = seed_ids.iter().map(|s| s.as_str()).collect();
                log::info!("SS: calling recommendations API");
                let result = spotify.api.recommendations(None, None, Some(seed_refs));
                log::info!("SS: recommendations returned (ok={})", result.is_ok());
                if let Ok(recs) = result {
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
                    log::info!("SS: append_all ({} tracks)", tracks.len());
                    queue.append_all(tracks);
                    log::info!("SS: append_all done, reshuffling");
                    // Reshuffle so recommendations are interspersed (Spotify-style),
                    // not stuck at the end of the play order.
                    queue.reshuffle();
                    log::info!("SS: reshuffle done, triggering redraw");
                    library.trigger_redraw();
                    log::info!("SS: bg thread done");
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

impl View for StatusBar {
    fn draw(&self, printer: &Printer<'_, '_>) {
        if printer.size.x == 0 {
            return;
        }

        self.update_terminal_title();

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
        // Frame + title follow the pane island border color (TitleSecondary),
        // so the player island matches the other panes.
        let style = ColorStyle::new(
            ColorType::Palette(PaletteColor::TitlePrimary),
            ColorType::Color(*printer.theme.palette.custom("statusbar_bg").unwrap()),
        );
        // Controls row text (time/volume) follows the pane island style too.
        let style_controls = ColorStyle::new(
            ColorType::Palette(PaletteColor::TitlePrimary),
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
        if is_suggested && shimmer_enabled && let Some(ref t) = title {
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
                let base_color = printer.theme.palette[PaletteColor::TitleSecondary];
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

        // Row 1: top padding row (│ space │)
        printer.with_color(style, |p| {
            p.print((0, 1), "│");
            p.print((1, 1), &" ".repeat(inner_w));
            p.print((printer.size.x.saturating_sub(1), 1), "│");
        });

        // Rows 2..=bar_height+1: progress bar
        let caps = self.bar_caps();
        let has_current = current.is_some();
        // Number of filled cells across the bar's content region [bar_x, bar_x+bar_w).
        let filled_cells = if has_current {
            (duration_fill + 1).min(bar_w)
        } else {
            0
        };
        // Empty caps use the shadow color dimmed, so a solid cap glyph doesn't
        // read brighter than the partial-fill ░ track next to it.
        let dim_empty = Style::from(style_bar_bg).combine(Effect::Dim);
        for row in 2..=bar_height + 1 {
            printer.with_color(style, |p| {
                p.print((0, row), "│");
                p.print((1, row), " ");
                p.print((bar_x + bar_w, row), " ");
                p.print((printer.size.x.saturating_sub(1), row), "│");
            });

            if let Some((left, right)) = &caps {
                // Rounded: the caps are the FIRST and LAST cells of the bar, so
                // they're part of the seekable region (clicking the left cap
                // seeks to the start) rather than outside decoration.
                for i in 0..bar_w {
                    let col = bar_x + i;
                    let is_filled = i < filled_cells;
                    let glyph: &str = if i == 0 {
                        left.as_str()
                    } else if i + 1 == bar_w {
                        right.as_str()
                    } else if is_filled {
                        filled_char
                    } else {
                        empty_char
                    };
                    if is_filled {
                        printer.with_color(style_bar, |p| p.print((col, row), glyph));
                    } else if i == 0 || i + 1 == bar_w {
                        printer.with_style(dim_empty, |p| p.print((col, row), glyph));
                    } else {
                        printer.with_color(style_bar_bg, |p| p.print((col, row), glyph));
                    }
                }
            } else {
                printer.with_color(style_bar_bg, |p| {
                    p.print((bar_x, row), &empty_char.repeat(bar_w));
                });
                if has_current {
                    printer.with_color(style_bar, |p| {
                        p.print((bar_x, row), &filled_char.repeat(filled_cells));
                    });
                }
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
        self.draw_controls(printer, ctrl_row, &cl);

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
                    let hit = cl.iter().find(|b| x >= b.start && x < b.end).map(|b| b.kind);
                    match hit {
                        Some(ControlButton::Prev) => {
                            self.queue.previous();
                            self.start_blink(ControlButton::Prev);
                        }
                        Some(ControlButton::Next) => {
                            self.queue.next(true);
                            self.start_blink(ControlButton::Next);
                        }
                        Some(ControlButton::Shuffle) => self.cycle_shuffle(),
                        Some(ControlButton::Repeat) => self.cycle_repeat(),
                        // Play button or empty space → toggle playback (play blinks).
                        Some(ControlButton::Play) => {
                            self.queue.toggleplayback();
                            self.start_blink(ControlButton::Play);
                        }
                        None => self.queue.toggleplayback(),
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
