pub mod sixel;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::{Cursive, Printer, Vec2, View};
use ioctl_rs::{TIOCGWINSZ, ioctl};

use crate::command::{Command, GotoMode};
use crate::commands::CommandResult;
use crate::config::Config;
use crate::events::EventManager;
use crate::library::Library;
use crate::queue::Queue;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::album::AlbumView;
use crate::ui::artist::ArtistView;

/// Whether the terminal reported sixel support. Probed once before cursive
/// takes over the terminal.
static SIXEL_SUPPORT: OnceLock<bool> = OnceLock::new();

/// Probe terminal capabilities used for cover rendering. Must be called
/// before `create_cursive()`.
pub fn detect_terminal_capabilities() {
    let _ = SIXEL_SUPPORT.set(sixel::probe_support());
}

/// Whether the terminal reported sixel support (false before probing).
pub fn sixel_supported() -> bool {
    SIXEL_SUPPORT.get().copied().unwrap_or(false)
}

/// The size of a terminal cell in pixels (queried via TIOCGWINSZ), scaled
/// down by `scale`. Falls back to a common cell size when the terminal
/// doesn't report dimensions.
pub fn cell_size_px(scale: f32) -> Vec2 {
    let (rows, cols, mut xpixels, mut ypixels) = unsafe {
        let mut query: (u16, u16, u16, u16) = (0, 0, 0, 0);
        ioctl(1, TIOCGWINSZ, &mut query);
        query
    };

    xpixels = ((xpixels as f32) / scale) as u16;
    ypixels = ((ypixels as f32) / scale) as u16;

    if cols > 0 && rows > 0 && xpixels > 0 && ypixels > 0 {
        Vec2::new((xpixels / cols) as usize, (ypixels / rows) as usize)
    } else {
        Vec2::new(8, 16)
    }
}

pub struct CoverView {
    queue: Arc<Queue>,
    library: Arc<Library>,
    last_size: RwLock<Vec2>,
    last_offset: RwLock<Vec2>,
    drawn_url: RwLock<Option<String>>,
    /// True when the sixel was emitted on the first frame after a content
    /// change and a second frame is pending to let the crossterm diff settle.
    draw_pending: RwLock<bool>,
    events: EventManager,
    backend: sixel::SixelBackend,
    /// Maximum HiDPI scale correction from the config.
    scale: f32,
    /// When true, sixel emission is suppressed (e.g. while a modal overlay is
    /// shown). Setting to false and resetting drawn_url triggers a fresh emit.
    paused: Arc<AtomicBool>,
}

impl CoverView {
    pub fn new(
        queue: Arc<Queue>,
        library: Arc<Library>,
        config: &Config,
        images: Arc<sixel::SixelImageCache>,
        events: EventManager,
    ) -> Self {
        Self {
            queue,
            library,
            backend: sixel::SixelBackend::new(images),
            events,
            last_size: RwLock::new(Vec2::new(0, 0)),
            last_offset: RwLock::new(Vec2::zero()),
            drawn_url: RwLock::new(None),
            draw_pending: RwLock::new(false),
            scale: config.values().cover_max_scale.unwrap_or(1.0),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns the shared pause flag. Set to `true` to suppress sixel
    /// emission (e.g. while a dropdown overlay is shown) and back to `false`
    /// to resume; a fresh emit is triggered automatically on the next draw.
    pub fn pause_handle(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }

    /// The size of a terminal cell in pixels, scaled by `cover_max_scale`.
    /// Queried per draw so it stays correct across window resizes.
    fn font_size(&self) -> Vec2 {
        cell_size_px(self.scale)
    }

    fn draw_cover(&self, url: String, draw_offset: Vec2, draw_size: Vec2) {
        if draw_size.x <= 1 || draw_size.y <= 1 {
            return;
        }

        // While paused (e.g. a dropdown overlay is shown), suppress sixel
        // emission and keep drawn_url cleared so a fresh emit fires on resume.
        if self.paused.load(Ordering::Relaxed) {
            *self.drawn_url.write().unwrap() = None;
            return;
        }

        let content_changed = {
            let last_size = self.last_size.read().unwrap();
            let last_off = self.last_offset.read().unwrap();
            let drawn_url = self.drawn_url.read().unwrap();
            *last_size != draw_size
                || *last_off != draw_offset
                || drawn_url.as_ref() != Some(&url)
        };
        let is_pending = *self.draw_pending.read().unwrap();

        if !content_changed && !is_pending {
            return;
        }

        // Content changed → reset Pending so the cycle restarts cleanly.
        if content_changed && is_pending {
            *self.draw_pending.write().unwrap() = false;
        }

        let drawn = self
            .backend
            .draw(&url, draw_offset, draw_size, self.font_size());

        if drawn {
            *self.last_size.write().unwrap() = draw_size;
            *self.last_offset.write().unwrap() = draw_offset;
            *self.drawn_url.write().unwrap() = Some(url);

            let mut pending = self.draw_pending.write().unwrap();
            if *pending {
                // Second frame: crossterm diff for this area is now empty,
                // the sixel sticks.
                *pending = false;
            } else {
                // First emission. The crossterm diff for this frame may carry
                // stale cells (full redraw on resize, push/pop, section switch)
                // and will arrive after our /dev/tty write, overwriting the
                // sixel. Schedule a second frame; that frame's diff will be
                // empty and the sixel will persist.
                *pending = true;
                self.events.trigger();
            }
        }
    }

    fn clear_cover(&self) {
        {
            let mut drawn_url = self.drawn_url.write().unwrap();
            if drawn_url.is_none() {
                return;
            }
            *drawn_url = None;
        }
        *self.draw_pending.write().unwrap() = false;
        let offset = *self.last_offset.read().unwrap();
        let size = *self.last_size.read().unwrap();
        self.backend.clear(offset, size);
    }
}

impl View for CoverView {
    fn draw(&self, printer: &Printer<'_, '_>) {
        // Completely blank out screen
        let style = ColorStyle::new(
            ColorType::Palette(PaletteColor::Background),
            ColorType::Palette(PaletteColor::Background),
        );
        printer.with_color(style, |printer| {
            for i in 0..printer.size.y {
                printer.print_hline((0, i), printer.size.x, " ");
            }
        });

        let cover_url = self.queue.get_current().and_then(|t| t.cover_url());

        if let Some(url) = cover_url {
            self.draw_cover(url, printer.offset, printer.size);
        } else {
            self.clear_cover();
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        Vec2::new(constraint.x, 2)
    }
}

impl ViewExt for CoverView {
    fn title(&self) -> String {
        "Cover".to_string()
    }

    fn on_leave(&self) {
        self.clear_cover();
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Save => {
                if let Some(mut track) = self.queue.get_current() {
                    track.save(&self.library);
                }
            }
            Command::Delete => {
                if let Some(mut track) = self.queue.get_current() {
                    track.unsave(&self.library);
                }
            }
            #[cfg(feature = "share_clipboard")]
            Command::Share(_mode) => {
                let url = self
                    .queue
                    .get_current()
                    .and_then(|t| t.as_listitem().share_url());

                if let Some(url) = url {
                    crate::sharing::write_share(url).ok();
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Goto(mode) => {
                if let Some(track) = self.queue.get_current() {
                    let queue = self.queue.clone();
                    let library = self.library.clone();

                    match mode {
                        GotoMode::Album => {
                            if let Some(album) = track.album(&queue) {
                                let view =
                                    AlbumView::new(queue, library, &album).into_boxed_view_ext();
                                return Ok(CommandResult::View(view));
                            }
                        }
                        GotoMode::Artist => {
                            if let Some(artists) = track.artists() {
                                return match artists.len() {
                                    0 => Ok(CommandResult::Consumed(None)),
                                    // Always choose the first artist even with more because
                                    // the cover image really doesn't play nice with the menu
                                    _ => {
                                        let view = ArtistView::new(queue, library, &artists[0])
                                            .into_boxed_view_ext();
                                        Ok(CommandResult::View(view))
                                    }
                                };
                            }
                        }
                    }
                }
            }
            _ => {}
        };

        Ok(CommandResult::Ignored)
    }
}
