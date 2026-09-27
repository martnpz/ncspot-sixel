use crate::application::ASYNC_RUNTIME;
use crate::command::Command;
use crate::commands::CommandResult;
use crate::events::{Event, EventManager};
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::Queue;
use crate::spotify::{Spotify, UriType};
use crate::spotify_url::SpotifyUrl;
use crate::traits::{ListItem, ViewExt};
use crate::ui::listview::ListView;
use crate::ui::pagination::Pagination;
use crate::ui::tabbedview::TabbedView;
use cursive::Cursive;
use cursive::view::ViewWrapper;
use rspotify::model::search::SearchResult;
use rspotify::model::{Page, SearchType};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

pub struct SearchResultsView {
    search_term: String,
    results_tracks: Arc<RwLock<Vec<Track>>>,
    pagination_tracks: Pagination<Track>,
    results_albums: Arc<RwLock<Vec<Album>>>,
    pagination_albums: Pagination<Album>,
    results_artists: Arc<RwLock<Vec<Artist>>>,
    pagination_artists: Pagination<Artist>,
    results_playlists: Arc<RwLock<Vec<Playlist>>>,
    pagination_playlists: Pagination<Playlist>,
    results_shows: Arc<RwLock<Vec<Show>>>,
    pagination_shows: Pagination<Show>,
    results_episodes: Arc<RwLock<Vec<Episode>>>,
    pagination_episodes: Pagination<Episode>,
    tabs: TabbedView,
    spotify: Spotify,
    events: EventManager,
}

type SearchHandler<I> = Box<
    dyn Fn(&Spotify, &Arc<RwLock<Vec<I>>>, &str, usize, bool) -> Result<Option<usize>, ()>
        + Send
        + Sync,
>;

// Spotify's search totals and `next` links can underreport available results.
// Probe one page at a time until it adds nothing, within the API's offset limit.
pub(crate) fn apply_search_page<T: serde::de::DeserializeOwned, I: ListItem>(
    page: Page<T>,
    results: &Arc<RwLock<Vec<I>>>,
    append: bool,
    convert: impl Fn(&T) -> I,
) -> Option<usize> {
    let mut results = results.write().unwrap();
    if !append {
        results.clear();
    }
    let before = results.len();
    let mut seen: HashSet<_> = results.iter().filter_map(ListItem::share_url).collect();
    for item in &page.items {
        let item = convert(item);
        if item.share_url().is_none_or(|url| seen.insert(url)) {
            results.push(item);
        }
    }
    let next = page.offset.checked_add(page.limit)?;
    (results.len() > before && page.limit > 0 && next <= 1000).then_some(next as usize)
}

impl SearchResultsView {
    pub fn new(
        search_term: String,
        events: EventManager,
        queue: Arc<Queue>,
        library: Arc<Library>,
    ) -> Self {
        let results_tracks = Arc::new(RwLock::new(Vec::new()));
        let results_albums = Arc::new(RwLock::new(Vec::new()));
        let results_artists = Arc::new(RwLock::new(Vec::new()));
        let results_playlists = Arc::new(RwLock::new(Vec::new()));
        let results_shows = Arc::new(RwLock::new(Vec::new()));
        let results_episodes = Arc::new(RwLock::new(Vec::new()));

        let list_tracks = ListView::new(results_tracks.clone(), queue.clone(), library.clone());
        let pagination_tracks = list_tracks.get_pagination().clone();
        let list_albums = ListView::new(results_albums.clone(), queue.clone(), library.clone());
        let pagination_albums = list_albums.get_pagination().clone();
        let list_artists = ListView::new(results_artists.clone(), queue.clone(), library.clone());
        let pagination_artists = list_artists.get_pagination().clone();
        let list_playlists =
            ListView::new(results_playlists.clone(), queue.clone(), library.clone());
        let pagination_playlists = list_playlists.get_pagination().clone();
        let list_shows = ListView::new(results_shows.clone(), queue.clone(), library.clone());
        let pagination_shows = list_shows.get_pagination().clone();
        let list_episodes = ListView::new(results_episodes.clone(), queue.clone(), library);
        let pagination_episodes = list_episodes.get_pagination().clone();

        let mut tabs = TabbedView::new();
        tabs.add_tab("Tracks", list_tracks);
        tabs.add_tab("Albums", list_albums);
        tabs.add_tab("Artists", list_artists);
        tabs.add_tab("Playlists", list_playlists);
        tabs.add_tab("Shows", list_shows);
        tabs.add_tab("Episodes", list_episodes);

        let mut view = Self {
            search_term,
            results_tracks,
            pagination_tracks,
            results_albums,
            pagination_albums,
            results_artists,
            pagination_artists,
            results_playlists,
            pagination_playlists,
            results_shows,
            pagination_shows,
            results_episodes,
            pagination_episodes,
            tabs,
            spotify: queue.get_spotify(),
            events,
        };

        view.run_search();
        view
    }

    fn get_track(
        spotify: &Spotify,
        tracks: &Arc<RwLock<Vec<Track>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(results) = spotify.api.track(query) {
            let t = vec![(&results).into()];
            let mut r = tracks.write().unwrap();
            *r = t;
            return Ok(None);
        }
        Err(())
    }

    fn search_track(
        spotify: &Spotify,
        tracks: &Arc<RwLock<Vec<Track>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Tracks(results)) =
            spotify
                .api
                .search(SearchType::Track, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, tracks, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn get_album(
        spotify: &Spotify,
        albums: &Arc<RwLock<Vec<Album>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(results) = spotify.api.album(query) {
            let a = vec![(&results).into()];
            let mut r = albums.write().unwrap();
            *r = a;
            return Ok(None);
        }
        Err(())
    }

    fn search_album(
        spotify: &Spotify,
        albums: &Arc<RwLock<Vec<Album>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Albums(results)) =
            spotify
                .api
                .search(SearchType::Album, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, albums, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn get_artist(
        spotify: &Spotify,
        artists: &Arc<RwLock<Vec<Artist>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(results) = spotify.api.artist(query) {
            let a = vec![(&results).into()];
            let mut r = artists.write().unwrap();
            *r = a;
            return Ok(None);
        }
        Err(())
    }

    fn search_artist(
        spotify: &Spotify,
        artists: &Arc<RwLock<Vec<Artist>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Artists(results)) =
            spotify
                .api
                .search(SearchType::Artist, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, artists, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn get_playlist(
        spotify: &Spotify,
        playlists: &Arc<RwLock<Vec<Playlist>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(result) = spotify.api.playlist(query).as_ref() {
            let pls = vec![result.into()];
            let mut r = playlists.write().unwrap();
            *r = pls;
            return Ok(None);
        }
        Err(())
    }

    fn search_playlist(
        spotify: &Spotify,
        playlists: &Arc<RwLock<Vec<Playlist>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Playlists(results)) =
            spotify
                .api
                .search(SearchType::Playlist, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, playlists, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn get_show(
        spotify: &Spotify,
        shows: &Arc<RwLock<Vec<Show>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(result) = spotify.api.show(query).as_ref() {
            let pls = vec![result.into()];
            let mut r = shows.write().unwrap();
            *r = pls;
            return Ok(None);
        }
        Err(())
    }

    fn search_show(
        spotify: &Spotify,
        shows: &Arc<RwLock<Vec<Show>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Shows(results)) =
            spotify
                .api
                .search(SearchType::Show, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, shows, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn get_episode(
        spotify: &Spotify,
        episodes: &Arc<RwLock<Vec<Episode>>>,
        query: &str,
        _offset: usize,
        _append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(result) = spotify.api.episode(query).as_ref() {
            let e = vec![result.into()];
            let mut r = episodes.write().unwrap();
            *r = e;
            return Ok(None);
        }
        Err(())
    }

    fn search_episode(
        spotify: &Spotify,
        episodes: &Arc<RwLock<Vec<Episode>>>,
        query: &str,
        offset: usize,
        append: bool,
    ) -> Result<Option<usize>, ()> {
        if let Ok(SearchResult::Episodes(results)) =
            spotify
                .api
                .search(SearchType::Episode, query, 10, offset as u32)
        {
            return Ok(apply_search_page(results, episodes, append, |item| {
                item.into()
            }));
        }
        Err(())
    }

    fn perform_search<I: ListItem + Clone>(
        &self,
        handler: SearchHandler<I>,
        results: &Arc<RwLock<Vec<I>>>,
        query: &str,
        paginator: Option<&Pagination<I>>,
    ) {
        let spotify = self.spotify.clone();
        let query = query.to_owned();
        let results = results.clone();
        let ev = self.events.clone();
        let paginator = paginator.cloned();

        std::thread::spawn(move || {
            let next_offset = match handler(&spotify, &results, &query, 0, false) {
                Ok(next) => next,
                Err(()) => {
                    ev.send(Event::Message(Err(
                        "Search failed; please retry the query.".to_owned(),
                    )));
                    ev.trigger();
                    return;
                }
            };

            if let Some(paginator) = paginator {
                let loaded = results.read().unwrap().len();
                let has_more = next_offset.is_some();
                let next_offset = Mutex::new(next_offset);
                let update_progress = paginator.search_progress_callback();
                let callback_events = ev.clone();
                let cb = move |items: Arc<RwLock<Vec<I>>>| {
                    let mut next = next_offset.lock().unwrap();
                    let Some(offset) = *next else { return };
                    match handler(&spotify, &results, &query, offset, true) {
                        Ok(new_offset) => {
                            *next = new_offset;
                            update_progress(items.read().unwrap().len(), next.is_some());
                        }
                        Err(()) => {
                            // Keep both existing results and the failed offset for a later retry.
                            callback_events.send(Event::Message(Err(
                                "Could not load more search results; scroll down to retry."
                                    .to_owned(),
                            )));
                        }
                    }
                    callback_events.trigger();
                };
                paginator.set(loaded, loaded, Box::new(cb));
                paginator.search_progress_callback()(loaded, has_more);
            }
            ev.trigger();
        });
    }

    pub fn run_search(&mut self) {
        let query = self.search_term.clone();

        // check if API token refresh is necessary before commencing multiple
        // requests to avoid deadlock, as the parallel requests might
        // simultaneously try to refresh the token
        self.spotify
            .api
            .update_token()
            .map(move |h| ASYNC_RUNTIME.get().unwrap().block_on(h).ok());

        // is the query a Spotify URI?
        if let Ok(uritype) = query.parse() {
            match uritype {
                UriType::Track => {
                    self.perform_search(
                        Box::new(Self::get_track),
                        &self.results_tracks,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(0);
                }
                UriType::Album => {
                    self.perform_search(
                        Box::new(Self::get_album),
                        &self.results_albums,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(1);
                }
                UriType::Artist => {
                    self.perform_search(
                        Box::new(Self::get_artist),
                        &self.results_artists,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(2);
                }
                UriType::Playlist => {
                    self.perform_search(
                        Box::new(Self::get_playlist),
                        &self.results_playlists,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(3);
                }
                UriType::Show => {
                    self.perform_search(
                        Box::new(Self::get_show),
                        &self.results_shows,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(4);
                }
                UriType::Episode => {
                    self.perform_search(
                        Box::new(Self::get_episode),
                        &self.results_episodes,
                        &query,
                        None,
                    );
                    self.tabs.set_selected(5);
                }
            }
        // Is the query a spotify URL?
        // https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC
        } else if let Some(url) = SpotifyUrl::from_url(&query) {
            match url.uri_type {
                UriType::Track => {
                    self.perform_search(
                        Box::new(Self::get_track),
                        &self.results_tracks,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(0);
                }
                UriType::Album => {
                    self.perform_search(
                        Box::new(Self::get_album),
                        &self.results_albums,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(1);
                }
                UriType::Artist => {
                    self.perform_search(
                        Box::new(Self::get_artist),
                        &self.results_artists,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(2);
                }
                UriType::Playlist => {
                    self.perform_search(
                        Box::new(Self::get_playlist),
                        &self.results_playlists,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(3);
                }
                UriType::Show => {
                    self.perform_search(
                        Box::new(Self::get_show),
                        &self.results_shows,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(4);
                }
                UriType::Episode => {
                    self.perform_search(
                        Box::new(Self::get_episode),
                        &self.results_episodes,
                        &url.id,
                        None,
                    );
                    self.tabs.set_selected(5);
                }
            }
        } else {
            self.perform_search(
                Box::new(Self::search_track),
                &self.results_tracks,
                &query,
                Some(&self.pagination_tracks),
            );
            self.perform_search(
                Box::new(Self::search_album),
                &self.results_albums,
                &query,
                Some(&self.pagination_albums),
            );
            self.perform_search(
                Box::new(Self::search_artist),
                &self.results_artists,
                &query,
                Some(&self.pagination_artists),
            );
            self.perform_search(
                Box::new(Self::search_playlist),
                &self.results_playlists,
                &query,
                Some(&self.pagination_playlists),
            );
            self.perform_search(
                Box::new(Self::search_show),
                &self.results_shows,
                &query,
                Some(&self.pagination_shows),
            );
            self.perform_search(
                Box::new(Self::search_episode),
                &self.results_episodes,
                &query,
                Some(&self.pagination_episodes),
            );
        }
    }
}

impl ViewWrapper for SearchResultsView {
    wrap_impl!(self.tabs: TabbedView);
}

impl ViewExt for SearchResultsView {
    fn title(&self) -> String {
        format!("Search: {}", self.search_term)
    }
    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        self.tabs.on_command(s, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::category::Category;

    fn page(offset: u32, ids: std::ops::Range<u32>) -> Page<Category> {
        Page {
            href: String::new(),
            limit: 10,
            next: None,
            offset,
            previous: None,
            total: 9,
            items: ids
                .map(|id| Category {
                    id: id.to_string(),
                    name: id.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn search_continues_past_underreported_total_and_missing_next() {
        let results = Arc::new(RwLock::new(Vec::new()));
        assert_eq!(
            apply_search_page(page(0, 0..9), &results, false, Clone::clone),
            Some(10)
        );
        assert_eq!(results.read().unwrap().len(), 9);
        assert_eq!(
            apply_search_page(page(10, 10..20), &results, true, Clone::clone),
            Some(20)
        );
        assert_eq!(
            apply_search_page(page(20, 20..30), &results, true, Clone::clone),
            Some(30)
        );
        assert_eq!(results.read().unwrap().len(), 29);
        assert_eq!(
            apply_search_page(page(30, 0..0), &results, true, Clone::clone),
            None
        );
        assert_eq!(results.read().unwrap().len(), 29);
    }

    #[test]
    fn overlapping_pages_are_deduplicated_and_repeated_pages_stop() {
        let results = Arc::new(RwLock::new(Vec::new()));
        apply_search_page(page(0, 0..10), &results, false, Clone::clone);
        assert_eq!(
            apply_search_page(page(10, 5..15), &results, true, Clone::clone),
            Some(20)
        );
        assert_eq!(results.read().unwrap().len(), 15);
        assert_eq!(
            apply_search_page(page(20, 5..15), &results, true, Clone::clone),
            None
        );
        assert_eq!(results.read().unwrap().len(), 15);
    }

    #[test]
    fn search_respects_offset_ceiling_and_replaces_old_query() {
        let results = Arc::new(RwLock::new(Vec::new()));
        assert_eq!(
            apply_search_page(page(990, 0..10), &results, false, Clone::clone),
            Some(1000)
        );
        assert_eq!(
            apply_search_page(page(1000, 10..20), &results, true, Clone::clone),
            None
        );
        apply_search_page(page(0, 30..33), &results, false, Clone::clone);
        assert_eq!(results.read().unwrap().len(), 3);
        assert_eq!(results.read().unwrap()[0].id, "30");
    }
}
