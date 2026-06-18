//! Info pane: cover image of the playing track on top, with a metadata block
//! (title / artist / genres) below it. Always reflects the playing track,
//! regardless of what's being browsed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use cursive::event::{Event, EventResult, EventTrigger, Key, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor, Style};
use cursive::view::{Margins, Position};
use cursive::views::{Dialog, OnEventView};
use cursive::{Cursive, Printer, Vec2, View};
use log::debug;
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::{Config, IconKind};
use crate::events::EventManager;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::{ListItem, ViewExt};
use crate::ui::browser::{DropdownBorder, PaneDropdownMenu};
use crate::ui::contextmenu::ContextMenu;
use crate::ui::modal::Modal;

#[cfg(feature = "cover")]
use crate::ui::cover::CoverView;

/// Cached artist genres, keyed by artist id.
pub type GenresCache = Arc<RwLock<HashMap<String, Vec<String>>>>;

/// Number of rows the metadata block occupies (incl. one spacing row).
const METADATA_HEIGHT: usize = 4;

/// Scroll the "Now Playing" title as a marquee once it reaches this many chars.
const TITLE_MARQUEE_MIN: usize = 28;
/// Time per one-character marquee step (also the redraw cadence while scrolling).
const MARQUEE_STEP: Duration = Duration::from_millis(220);

/// Fetch the genres for `playable`'s first artist into `cache`, off-thread.
/// Call this on track changes; cached artists are not re-fetched.
pub fn fetch_genres(cache: &GenresCache, spotify: &Spotify, events: &EventManager, playable: &Playable) {
    let Playable::Track(track) = playable else {
        return;
    };
    let Some(artist_id) = track.artist_ids.first().cloned() else {
        return;
    };
    if cache.read().unwrap().contains_key(&artist_id) {
        return;
    }

    let cache = cache.clone();
    let spotify = spotify.clone();
    let events = events.clone();
    std::thread::spawn(move || {
        if let Ok(artist) = spotify.api.artist(&artist_id) {
            // Spotify has deprecated artist genres; they may come back empty
            // depending on the API client. Treat empty as "no genres".
            #[allow(deprecated)]
            let genres = artist.genres;
            debug!("genres for {}: {genres:?}", artist.name);
            cache.write().unwrap().insert(artist_id, genres);
            events.trigger();
        }
    });
}

#[derive(Clone, Copy)]
enum NowPlayingAction {
    AddToQueue,
    AddToPlaylist,
    RemoveFromPlaylist,
    #[cfg(feature = "share_clipboard")]
    Share,
}


pub struct InfoView {
    queue: Arc<Queue>,
    library: Arc<Library>,
    cfg: Arc<Config>,
    genres: GenresCache,
    last_offset: RwLock<Vec2>,
    last_content_w: RwLock<usize>,
    /// Time origin for the long-title marquee animation.
    marquee_start: Instant,
    #[cfg(feature = "cover")]
    cover_pause: Arc<AtomicBool>,
    #[cfg(feature = "cover")]
    cover: CoverView,
}

impl InfoView {
    pub fn new(
        queue: Arc<Queue>,
        library: Arc<Library>,
        cfg: Arc<Config>,
        genres: GenresCache,
        #[cfg(feature = "cover")] cover: CoverView,
    ) -> Self {
        // Drive the title marquee: while the current title is long enough to
        // scroll, nudge a redraw each step; otherwise poll slowly (near-free).
        {
            let queue = queue.clone();
            let library = library.clone();
            std::thread::spawn(move || {
                loop {
                    let len = queue
                        .get_current()
                        .map(|p| match &p {
                            Playable::Track(t) => t.title.chars().count(),
                            Playable::Episode(e) => e.name.chars().count(),
                        })
                        .unwrap_or(0);
                    if len >= TITLE_MARQUEE_MIN {
                        library.trigger_redraw();
                        std::thread::sleep(MARQUEE_STEP);
                    } else {
                        std::thread::sleep(Duration::from_millis(400));
                    }
                }
            });
        }

        Self {
            queue,
            library,
            cfg,
            genres,
            last_offset: RwLock::new(Vec2::zero()),
            last_content_w: RwLock::new(0),
            marquee_start: Instant::now(),
            #[cfg(feature = "cover")]
            cover_pause: cover.pause_handle(),
            #[cfg(feature = "cover")]
            cover,
        }
    }

    fn genres_line(&self, playable: &Playable) -> String {
        let count = self
            .cfg
            .values()
            .layout
            .as_ref()
            .and_then(|layout| layout.info.as_ref())
            .and_then(|info| info.genres)
            .unwrap_or(2) as usize;
        if count == 0 {
            return String::new();
        }

        let Playable::Track(track) = playable else {
            return String::new();
        };
        let Some(artist_id) = track.artist_ids.first() else {
            return String::new();
        };
        self.genres
            .read()
            .unwrap()
            .get(artist_id)
            .map(|genres| {
                genres
                    .iter()
                    .take(count)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default()
    }

    fn open_dropdown(&self) -> EventResult {
        let offset = *self.last_offset.read().unwrap();
        let content_w = *self.last_content_w.read().unwrap();

        // Mirror the draw_island centering formula used in panes.rs.
        let title_padded_w = "Now Playing".width() + 4;
        let left_fill = content_w.saturating_sub(title_padded_w) / 2;
        let anchor = Vec2::new(offset.x + left_fill, offset.y);

        let queue = self.queue.clone();
        let library = self.library.clone();
        let cfg = self.cfg.clone();

        #[cfg(feature = "cover")]
        self.cover_pause.store(true, Ordering::Relaxed);
        #[cfg(feature = "cover")]
        let cover_pause = self.cover_pause.clone();

        EventResult::with_cb(move |s: &mut Cursive| {
            let Some(playable) = queue.get_current() else {
                #[cfg(feature = "cover")]
                cover_pause.store(false, Ordering::Relaxed);
                return;
            };

            let icon_add_to_queue = cfg.icon(IconKind::MenuAddToQueue);
            let icon_add_to_playlist = cfg.icon(IconKind::MenuAddToPlaylist);
            let icon_remove = cfg.icon(IconKind::MenuRemoveFromPlaylist);
            #[cfg(feature = "share_clipboard")]
            let icon_share = cfg.icon(IconKind::MenuShare);

            let mut items = vec![(format!(" {icon_add_to_queue}Add to queue "), NowPlayingAction::AddToQueue)];
            if playable.track().is_some() {
                items.push((format!(" {icon_add_to_playlist}Add to playlist "), NowPlayingAction::AddToPlaylist));
                items.push((format!(" {icon_remove}Remove from playlist "), NowPlayingAction::RemoveFromPlaylist));
            }
            #[cfg(feature = "share_clipboard")]
            if playable.as_listitem().share_url().is_some() {
                items.push((format!(" {icon_share}Share "), NowPlayingAction::Share));
            }

            let queue_cb = queue.clone();
            let library_cb = library.clone();
            #[cfg(feature = "cover")]
            let cover_pause_cb = cover_pause.clone();

            let menu_view = PaneDropdownMenu::new(items, move |s, action| {
                let Some(playable) = queue_cb.get_current() else {
                    #[cfg(feature = "cover")]
                    cover_pause_cb.store(false, Ordering::Relaxed);
                    s.pop_layer();
                    return;
                };
                #[cfg(feature = "cover")]
                cover_pause_cb.store(false, Ordering::Relaxed);
                s.pop_layer();

                match action {
                    NowPlayingAction::AddToQueue => {
                        queue_cb.insert_after_current(playable);
                    }
                    NowPlayingAction::AddToPlaylist => {
                        if let Some(track) = playable.track() {
                            let dialog = ContextMenu::add_track_dialog(
                                library_cb.clone(),
                                queue_cb.get_spotify(),
                                track,
                            );
                            s.add_layer(dialog);
                        }
                    }
                    NowPlayingAction::RemoveFromPlaylist => {
                        if let Some(track) = playable.track() {
                            let track_id = track.id.as_deref().unwrap_or("");
                            let current_user_id = library_cb.user_id.as_deref().unwrap_or("");
                            let playlists: Vec<Playlist> = library_cb
                                .playlists
                                .read()
                                .unwrap()
                                .iter()
                                .filter(|pl| {
                                    (current_user_id == pl.owner_id || pl.collaborative)
                                        && pl.has_track(track_id)
                                })
                                .cloned()
                                .collect();
                            if playlists.is_empty() {
                                let dialog =
                                    Dialog::text("Track is not in any of your playlists.")
                                        .title("Remove from playlist")
                                        .padding(Margins::lrtb(1, 1, 1, 0))
                                        .dismiss_button("Close");
                                s.add_layer(Modal::new(dialog));
                            } else {
                                let dialog = ContextMenu::remove_track_dialog(
                                    library_cb.clone(),
                                    queue_cb.get_spotify(),
                                    track,
                                    playlists,
                                );
                                s.add_layer(dialog);
                            }
                        }
                    }
                    #[cfg(feature = "share_clipboard")]
                    NowPlayingAction::Share => {
                        if let Some(url) = playable.as_listitem().share_url() {
                            crate::sharing::write_share(url).ok();
                        }
                    }
                }
            });

            #[cfg(feature = "cover")]
            let cover_pause_esc = cover_pause.clone();
            #[cfg(feature = "cover")]
            let cover_pause_q = cover_pause.clone();
            #[cfg(feature = "cover")]
            let cover_pause_mouse = cover_pause.clone();

            let menu = OnEventView::new(DropdownBorder::new(menu_view))
                .on_event(Key::Esc, move |s| {
                    #[cfg(feature = "cover")]
                    cover_pause_esc.store(false, Ordering::Relaxed);
                    s.pop_layer();
                })
                .on_event(Event::Char('q'), move |s| {
                    #[cfg(feature = "cover")]
                    cover_pause_q.store(false, Ordering::Relaxed);
                    s.pop_layer();
                })
                // Close on any unconsumed mouse press (click outside the dropdown).
                .on_event(
                    EventTrigger::from_fn(|e| {
                        matches!(e, Event::Mouse { event: MouseEvent::Press(_), .. })
                    }),
                    move |s| {
                        #[cfg(feature = "cover")]
                        cover_pause_mouse.store(false, Ordering::Relaxed);
                        s.pop_layer();
                    },
                );
            s.screen_mut()
                .add_layer_at(Position::absolute(anchor), menu);
        })
    }
}

impl View for InfoView {
    fn draw(&self, printer: &Printer) {
        *self.last_offset.write().unwrap() = printer.offset;
        *self.last_content_w.write().unwrap() = printer.size.x;

        let cover_height = printer.size.y.saturating_sub(METADATA_HEIGHT);

        #[cfg(feature = "cover")]
        {
            let cover_printer = printer.cropped((printer.size.x, cover_height));
            self.cover.draw(&cover_printer);
        }

        let Some(playable) = self.queue.get_current() else {
            return;
        };

        let (title, artists) = match &playable {
            Playable::Track(track) => (track.title.clone(), track.artists.join(", ")),
            Playable::Episode(episode) => (episode.name.clone(), String::new()),
        };
        let genres = self.genres_line(&playable);

        let center = |text: &str| (printer.size.x.saturating_sub(text.width())) / 2;
        let y = cover_height + 1;

        let title_style = Style::from(ColorStyle::title_primary()).combine(Effect::Bold);
        if title.chars().count() >= TITLE_MARQUEE_MIN && printer.size.x > 0 {
            // Long titles scroll left → right as a seamless loop.
            let scroll: Vec<char> = format!("{title}    ").chars().collect();
            let n = scroll.len();
            let window = printer.size.x;
            let step =
                (self.marquee_start.elapsed().as_millis() / MARQUEE_STEP.as_millis().max(1)) as usize;
            // Increasing start offset moves the text leftward (right → left).
            let off = step % n;
            let visible: String = (0..window).map(|i| scroll[(off + i) % n]).collect();
            printer.with_style(title_style, |printer| printer.print((0, y), &visible));
        } else {
            printer.with_style(title_style, |printer| printer.print((center(&title), y), &title));
        }
        printer.with_color(ColorStyle::primary(), |printer| {
            printer.print((center(&artists), y + 1), &artists);
        });
        printer.with_style(
            Style::from(ColorStyle::new(
                ColorType::Palette(PaletteColor::Background),
                ColorType::Palette(PaletteColor::Background),
            ))
            .combine(Effect::Dim),
            |printer| printer.print((center(&genres), y + 2), &genres),
        );
    }

    fn layout(&mut self, size: Vec2) {
        #[cfg(feature = "cover")]
        self.cover
            .layout(Vec2::new(size.x, size.y.saturating_sub(METADATA_HEIGHT)));
        #[cfg(not(feature = "cover"))]
        let _ = size;
    }

    fn needs_relayout(&self) -> bool {
        true
    }
}

impl ViewExt for InfoView {
    fn title(&self) -> String {
        "Now Playing".to_string()
    }

    fn has_title_action(&self) -> bool {
        true
    }

    fn on_title_action(&mut self) -> EventResult {
        self.open_dropdown()
    }

    fn on_leave(&self) {
        #[cfg(feature = "cover")]
        self.cover.on_leave();
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        #[cfg(feature = "cover")]
        return self.cover.on_command(s, cmd);
        #[cfg(not(feature = "cover"))]
        {
            let _ = (s, cmd);
            Ok(CommandResult::Ignored)
        }
    }
}
