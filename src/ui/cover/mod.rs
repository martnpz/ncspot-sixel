pub mod sixel;
pub mod ueberzug;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::{Arc, RwLock};

use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::{Cursive, Printer, Vec2, View};
use ioctl_rs::{TIOCGWINSZ, ioctl};
use log::{error, info};

use crate::command::{Command, GotoMode};
use crate::commands::CommandResult;
use crate::config::Config;
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

/// A renderer that can put a cover image onto the terminal.
trait CoverBackend: Send + Sync {
    /// Draw the image at `path` into the cell rectangle described by
    /// `draw_offset`/`draw_size`. Returns whether the image was actually
    /// drawn (false e.g. while an encode is still in flight).
    fn draw(
        &self,
        url: &str,
        path: &Path,
        draw_offset: Vec2,
        draw_size: Vec2,
        font_size: Vec2,
    ) -> bool;

    /// Remove a previously drawn image, where the backend supports it.
    fn clear(&self);
}

pub struct CoverView {
    queue: Arc<Queue>,
    library: Arc<Library>,
    loading: Arc<RwLock<HashSet<String>>>,
    last_size: RwLock<Vec2>,
    drawn_url: RwLock<Option<String>>,
    backend: Box<dyn CoverBackend>,
    /// Maximum HiDPI scale correction from the config.
    scale: f32,
}

impl CoverView {
    pub fn new(
        queue: Arc<Queue>,
        library: Arc<Library>,
        config: &Config,
        images: Arc<sixel::SixelImageCache>,
    ) -> Self {
        let configured = config.values().cover_backend.clone();
        let backend: Box<dyn CoverBackend> = match configured.as_deref() {
            Some("sixel") => Box::new(sixel::SixelBackend::new(images)),
            Some("ueberzug") => Box::new(ueberzug::UeberzugBackend::new()),
            Some(other) => {
                error!(r#"unknown cover_backend "{other}", falling back to ueberzug"#);
                Box::new(ueberzug::UeberzugBackend::new())
            }
            None => {
                if SIXEL_SUPPORT.get().copied().unwrap_or(false) {
                    info!("using sixel cover backend");
                    Box::new(sixel::SixelBackend::new(images))
                } else {
                    info!("terminal doesn't support sixel, using ueberzug cover backend");
                    Box::new(ueberzug::UeberzugBackend::new())
                }
            }
        };

        Self {
            queue,
            library,
            backend,
            loading: Arc::new(RwLock::new(HashSet::new())),
            last_size: RwLock::new(Vec2::new(0, 0)),
            drawn_url: RwLock::new(None),
            scale: config.values().cover_max_scale.unwrap_or(1.0),
        }
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

        let needs_redraw = {
            let last_size = self.last_size.read().unwrap();
            let drawn_url = self.drawn_url.read().unwrap();
            *last_size != draw_size || drawn_url.as_ref() != Some(&url)
        };

        if !needs_redraw {
            return;
        }

        let path = match self.cache_path(url.clone()) {
            Some(p) => p,
            None => return,
        };

        let drawn = self
            .backend
            .draw(&url, &path, draw_offset, draw_size, self.font_size());

        if drawn {
            let mut last_size = self.last_size.write().unwrap();
            *last_size = draw_size;

            let mut drawn_url = self.drawn_url.write().unwrap();
            *drawn_url = Some(url);
        }
    }

    fn clear_cover(&self) {
        let mut drawn_url = self.drawn_url.write().unwrap();
        if drawn_url.is_none() {
            return;
        }
        *drawn_url = None;
        self.backend.clear();
    }

    fn cache_path(&self, url: String) -> Option<PathBuf> {
        let path = crate::utils::cache_path_for_url(url.clone());

        let mut loading = self.loading.write().unwrap();
        if loading.contains(&url) {
            return None;
        }

        if path.exists() {
            return Some(path);
        }

        loading.insert(url.clone());

        let loading_thread = self.loading.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::utils::download(url.clone(), path.clone()) {
                error!("Failed to download cover: {e}");
            }
            let mut loading = loading_thread.write().unwrap();
            loading.remove(&url.clone());
        });

        None
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
