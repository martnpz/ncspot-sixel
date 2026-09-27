use std::sync::{Arc, RwLock};
use std::thread;

use cursive::view::ViewWrapper;
use cursive::{Cursive, Printer, View};
use log::warn;
use rspotify::model::AlbumType;

use crate::command::Command;
use crate::commands::CommandResult;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::queue::Queue;
use crate::traits::{ListItem, ViewExt};
use crate::ui::listview::ListView;
use crate::ui::tabbedview::TabbedView;

/// Preserve the list layout while making loading, empty results and failures visible.
struct ArtistList<I: ListItem + Clone> {
    view: ListView<I>,
    status: Arc<RwLock<String>>,
}

impl<I: ListItem + Clone> ArtistList<I> {
    fn new(items: Arc<RwLock<Vec<I>>>, queue: Arc<Queue>, library: Arc<Library>) -> Self {
        Self {
            view: ListView::new(items, queue, library),
            status: Arc::new(RwLock::new("Loading…".to_owned())),
        }
    }
}

impl<I: ListItem + Clone> ViewWrapper for ArtistList<I> {
    wrap_impl!(self.view: ListView<I>);

    fn wrap_draw(&self, printer: &Printer) {
        if self.view.content_len(false) == 0 {
            printer.with_color(cursive::theme::ColorStyle::secondary(), |p| {
                p.print((0, 0), &self.status.read().unwrap());
            });
        } else {
            self.view.draw(printer);
        }
    }
}

impl<I: ListItem + Clone> ViewExt for ArtistList<I> {
    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        self.view.on_command(s, cmd)
    }
}

pub struct ArtistView {
    artist: Artist,
    tabs: TabbedView,
}

impl ArtistView {
    pub fn new(queue: Arc<Queue>, library: Arc<Library>, artist: &Artist) -> Self {
        let spotify = queue.get_spotify();

        let albums_view =
            Self::albums_view(artist, AlbumType::Album, queue.clone(), library.clone());
        let singles_view =
            Self::albums_view(artist, AlbumType::Single, queue.clone(), library.clone());

        let top_tracks = Arc::new(RwLock::new(Vec::new()));
        let related = Arc::new(RwLock::new(Vec::new()));
        let top_view = ArtistList::new(top_tracks.clone(), queue.clone(), library.clone());
        let related_view = ArtistList::new(related.clone(), queue.clone(), library.clone());
        let top_status = top_view.status.clone();
        let related_status = related_view.status.clone();
        let id = artist.id.clone();
        let loader_library = library.clone();
        let top_spotify = spotify.clone();
        thread::spawn(move || {
            let result = if let Some(id) = id {
                if loader_library.cfg.values().client_id.is_some() {
                    match top_spotify.session() {
                        Some(session) => crate::application::ASYNC_RUNTIME
                            .get()
                            .unwrap()
                            .block_on(crate::session_artists::top_tracks(&session, &id)),
                        None => Err("Playback session is not connected".to_owned()),
                    }
                } else {
                    top_spotify
                        .api
                        .artist_top_tracks(&id)
                        .map_err(|_| "Artist request failed".to_owned())
                }
            } else {
                Err("No artist ID available".to_owned())
            };
            match result {
                Ok(tracks) => {
                    *top_tracks.write().unwrap() = tracks;
                    *top_status.write().unwrap() = "No top tracks available.".to_owned();
                }
                Err(e) => {
                    warn!("Could not load artist tracks: {e}");
                    *top_status.write().unwrap() =
                        "Could not load tracks. Reopen the artist to retry.".to_owned();
                }
            }
            loader_library.trigger_redraw();
        });

        let id = artist.id.clone();
        let loader_library = library.clone();
        thread::spawn(move || {
            let result = match (id, spotify.session()) {
                (Some(id), Some(session)) => crate::application::ASYNC_RUNTIME
                    .get()
                    .unwrap()
                    .block_on(crate::session_artists::related_artists(&session, &id)),
                _ => Err("Artist ID or playback session unavailable".to_owned()),
            };
            match result {
                Ok(artists) => {
                    *related.write().unwrap() = artists;
                    *related_status.write().unwrap() = "No related artists available.".to_owned();
                }
                Err(e) => {
                    warn!("Could not load related artists: {e}");
                    *related_status.write().unwrap() =
                        "Could not load related artists. Reopen the artist to retry.".to_owned();
                }
            }
            loader_library.trigger_redraw();
        });

        let mut tabs = TabbedView::new();

        if let Some(tracks) = artist.tracks.as_ref() {
            let tracks = tracks.clone();

            tabs.add_tab(
                "Saved Tracks",
                ListView::new(
                    Arc::new(RwLock::new(tracks)),
                    queue.clone(),
                    library.clone(),
                ),
            );
        }
        tabs.add_tab("Top 10", top_view);
        tabs.add_tab("Albums", albums_view);
        tabs.add_tab("Singles", singles_view);
        tabs.add_tab("Related Artists", related_view);

        Self {
            artist: artist.clone(),
            tabs,
        }
    }

    fn albums_view(
        artist: &Artist,
        album_type: AlbumType,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> ArtistList<Album> {
        let items = Arc::new(RwLock::new(Vec::new()));
        let view = ArtistList::new(items.clone(), queue.clone(), library.clone());
        let pagination = view.view.get_pagination().clone();
        let status = view.status.clone();
        let id = artist.id.clone();
        thread::spawn(move || {
            if let Some(id) = id {
                let page = queue.get_spotify().api.artist_albums(&id, Some(album_type));
                if page.first_page_loaded() {
                    *items.write().unwrap() = page.items.read().unwrap().clone();
                    let loaded = items.read().unwrap().len();
                    pagination.set(
                        loaded,
                        page.total as usize,
                        Box::new(move |items| {
                            if let Some(next) = page.next() {
                                items.write().unwrap().extend(next);
                            }
                        }),
                    );
                    *status.write().unwrap() = "No releases available.".to_owned();
                } else {
                    *status.write().unwrap() =
                        "Could not load releases. Reopen the artist to retry.".to_owned();
                }
            } else {
                *status.write().unwrap() = "No artist ID available.".to_owned();
            }
            library.trigger_redraw();
        });
        view
    }
}

impl ViewWrapper for ArtistView {
    wrap_impl!(self.tabs: TabbedView);
}

impl ViewExt for ArtistView {
    fn title(&self) -> String {
        self.artist.name.clone()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        self.tabs.on_command(s, cmd)
    }
}
