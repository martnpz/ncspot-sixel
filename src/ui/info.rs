//! Info pane: cover image of the playing track on top, with a metadata block
//! (title / artist / genres) below it. Always reflects the playing track,
//! regardless of what's being browsed.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor, Style};
use cursive::{Cursive, Printer, Vec2, View};
use log::debug;
use unicode_width::UnicodeWidthStr;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::config::Config;
use crate::events::EventManager;
use crate::model::playable::Playable;
use crate::queue::Queue;
use crate::spotify::Spotify;
use crate::traits::ViewExt;

#[cfg(feature = "cover")]
use crate::ui::cover::CoverView;

/// Cached artist genres, keyed by artist id.
pub type GenresCache = Arc<RwLock<HashMap<String, Vec<String>>>>;

/// Number of rows the metadata block occupies (incl. one spacing row).
const METADATA_HEIGHT: usize = 4;

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

pub struct InfoView {
    queue: Arc<Queue>,
    cfg: Arc<Config>,
    genres: GenresCache,
    #[cfg(feature = "cover")]
    cover: CoverView,
}

impl InfoView {
    pub fn new(
        queue: Arc<Queue>,
        cfg: Arc<Config>,
        genres: GenresCache,
        #[cfg(feature = "cover")] cover: CoverView,
    ) -> Self {
        Self {
            queue,
            cfg,
            genres,
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
}

impl View for InfoView {
    fn draw(&self, printer: &Printer) {
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

        printer.with_style(
            Style::from(ColorStyle::title_primary()).combine(Effect::Bold),
            |printer| printer.print((center(&title), y), &title),
        );
        printer.with_color(ColorStyle::primary(), |printer| {
            printer.print((center(&artists), y + 1), &artists);
        });
        printer.with_style(
            Style::from(ColorStyle::new(
                ColorType::Palette(PaletteColor::Secondary),
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
