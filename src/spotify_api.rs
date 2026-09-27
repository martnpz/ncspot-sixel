use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::application::ASYNC_RUNTIME;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use log::{debug, error, info, warn};
use rspotify::http::HttpError;
use rspotify::model::{
    AlbumId, AlbumType, ArtistId, CursorBasedPage, EpisodeId, FullAlbum, FullArtist, FullEpisode,
    FullPlaylist, FullShow, FullTrack, ItemPositions, LibraryId, Market, Page, PlayableId,
    PlaylistId, PlaylistResult, PrivateUser, SavedAlbum, SavedTrack, SearchResult, SearchType,
    Show, ShowId, SimplifiedTrack, TrackId, UserId,
};
use rspotify::{AuthCodeSpotify, ClientError, ClientResult, Config, prelude::*};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::category::Category;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::track::Track;
use crate::spotify_worker::WorkerCommand;
use crate::ui::pagination::{ApiPage, ApiResult};

/// Convenient wrapper around the rspotify web API functionality.
#[derive(Clone)]
pub struct WebApi {
    /// Rspotify web API.
    api: AuthCodeSpotify,
    /// The username of the logged in user.
    user: Option<String>,
    /// Sender of the mpsc channel to the [Spotify](crate::spotify::Spotify) worker thread.
    worker_channel: Arc<RwLock<Option<mpsc::UnboundedSender<WorkerCommand>>>>,
    /// Time at which the token expires.
    token_expiration: Arc<RwLock<DateTime<Utc>>>,
    cfg: Arc<crate::config::Config>,
    request_gate: Arc<Mutex<Option<Instant>>>,
}

impl WebApi {
    pub fn new(cfg: Arc<crate::config::Config>) -> Self {
        let config = Config {
            token_refreshing: false,
            ..Default::default()
        };
        let api = AuthCodeSpotify::with_config(
            rspotify::Credentials::new(&crate::authentication::web_api_client_id(&cfg), ""),
            rspotify::OAuth::default(),
            config,
        );
        Self {
            api,
            user: None,
            worker_channel: Arc::new(RwLock::new(None)),
            token_expiration: Arc::new(RwLock::new(Utc::now())),
            cfg,
            request_gate: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub fn defer_requests_for_test(&self) {
        *self.request_gate.lock().unwrap() = Some(Instant::now() + Duration::from_secs(3600));
    }

    /// Set the username for use with the API.
    pub fn set_user(&mut self, user: Option<String>) {
        self.user = user;
    }

    /// Set the sending end of the channel to the worker thread, managed by
    /// [Spotify](crate::spotify::Spotify).
    pub(crate) fn set_worker_channel(
        &mut self,
        channel: Arc<RwLock<Option<mpsc::UnboundedSender<WorkerCommand>>>>,
    ) {
        self.worker_channel = channel;
    }

    /// Update the authentication token when it expires.
    pub fn update_token(&self) -> Option<JoinHandle<()>> {
        {
            let token_expiration = self.token_expiration.read().unwrap();
            let now = Utc::now();
            let delta = *token_expiration - now;

            // token is valid for 5 more minutes, renewal is not necessary yet
            if delta.num_seconds() > 60 * 5 {
                return None;
            }

            info!("Token will expire in {delta}, renewing");
        }

        let cfg = self.cfg.clone();
        let api_token = self.api.token.clone();
        let api_token_expiration = self.token_expiration.clone();
        Some(ASYNC_RUNTIME.get().unwrap().spawn_blocking(move || {
            match crate::authentication::get_web_token(&cfg, false, false) {
                Ok(token) => {
                    let expires_at = token
                        .expires_at
                        .unwrap_or_else(|| Utc::now() + ChronoDuration::hours(1));
                    *api_token.lock().unwrap() = Some(token);
                    *api_token_expiration.write().unwrap() = expires_at;
                }
                Err(e) => {
                    error!("Failed to update token: {e}");
                }
            }
        }))
    }

    /// Remaining shared cooldown. Only background library jobs wait for it.
    pub fn retry_after(&self) -> Option<Duration> {
        self.request_gate
            .lock()
            .unwrap()
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    /// Serialize requests and respect every 429, including retries. Never sleep
    /// through a server cooldown on the UI thread or retry a mutation blindly.
    fn api_with_retry<F, R>(&self, api_call: F) -> Option<R>
    where
        F: Fn(&AuthCodeSpotify) -> ClientResult<R>,
    {
        let mut gate = self.request_gate.lock().unwrap();
        if gate.is_some_and(|until| until > Instant::now()) {
            return None;
        }
        for attempt in 0..2 {
            match api_call(&self.api) {
                Ok(value) => {
                    *gate = None;
                    return Some(value);
                }
                Err(ClientError::Http(error)) => match *error {
                    HttpError::StatusCode(response) => {
                        let status = response.status();
                        // Log only the endpoint, never authorization headers or token bodies.
                        let endpoint = url::Url::parse(response.get_url())
                            .ok()
                            .map(|u| u.path().to_owned())
                            .unwrap_or_default();
                        if status == 401 && attempt == 0 {
                            match crate::authentication::get_web_token(&self.cfg, true, false) {
                                Ok(token) => {
                                    *self.token_expiration.write().unwrap() =
                                        token.expires_at.unwrap_or(Utc::now());
                                    *self.api.token.lock().unwrap() = Some(token);
                                    continue;
                                }
                                Err(e) => {
                                    error!("Web API token refresh failed: {e}");
                                    return None;
                                }
                            }
                        }
                        let delay = response
                            .header("Retry-After")
                            .and_then(|h| h.parse::<u64>().ok());
                        let body: serde_json::Value = response.into_json().unwrap_or_default();
                        let reason = body
                            .pointer("/error/reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unspecified");
                        if status == 429 {
                            let seconds = delay.unwrap_or(30).max(1);
                            *gate = Instant::now().checked_add(Duration::from_secs(seconds));
                            warn!(
                                "Spotify Web API {endpoint}: HTTP 429, reason={reason}, Retry-After={seconds}s; deferring requests"
                            );
                        } else {
                            error!("Spotify Web API {endpoint}: HTTP {status}, reason={reason}");
                        }
                        return None;
                    }
                    e => {
                        error!("Web API transport error: {e}");
                        return None;
                    }
                },
                Err(e) => {
                    error!("Web API request failed: {e}");
                    return None;
                }
            }
        }
        None
    }

    /// Append `tracks` at `position` in the playlist with `playlist_id`.
    pub fn append_tracks(
        &self,
        playlist_id: &str,
        tracks: &[Playable],
        position: Option<u32>,
    ) -> Result<PlaylistResult, ()> {
        self.api_with_retry(|api| {
            let trackids: Vec<PlayableId> = tracks
                .iter()
                .filter_map(|playable| playable.into())
                .collect();
            api.playlist_add_items(
                PlaylistId::from_id(playlist_id).unwrap(),
                trackids.iter().map(|id| id.as_ref()),
                position,
            )
        })
        .ok_or(())
    }

    pub fn delete_tracks(
        &self,
        playlist_id: &str,
        snapshot_id: &str,
        playables: &[Playable],
    ) -> Result<PlaylistResult, ()> {
        self.api_with_retry(move |api| {
            let playable_ids: Vec<PlayableId> = playables
                .iter()
                .filter_map(|playable| playable.into())
                .collect();
            let positions = playables
                .iter()
                .map(|playable| [playable.list_index() as u32])
                .collect::<Vec<_>>();
            let item_pos: Vec<ItemPositions> = playable_ids
                .iter()
                .zip(positions.iter())
                .map(|(id, positions)| ItemPositions {
                    id: id.as_ref(),
                    positions,
                })
                .collect();
            api.playlist_remove_specific_occurrences_of_items(
                PlaylistId::from_id(playlist_id).unwrap(),
                item_pos,
                Some(snapshot_id),
            )
        })
        .ok_or(())
    }

    /// Set the playlist with `id` to contain only `tracks`. If the playlist already contains
    /// tracks, they will be removed.
    pub fn overwrite_playlist(&self, id: &str, tracks: &[Playable]) {
        // create mutable copy for chunking
        let mut tracks: Vec<Playable> = tracks.to_vec();

        // we can only send 100 tracks per request
        let mut remainder = if tracks.len() > 100 {
            Some(tracks.split_off(100))
        } else {
            None
        };

        let replace_items = self.api_with_retry(|api| {
            let playable_ids: Vec<PlayableId> = tracks
                .iter()
                .filter_map(|playable| playable.into())
                .collect();
            api.playlist_replace_items(
                PlaylistId::from_id(id).unwrap(),
                playable_ids.iter().map(|p| p.as_ref()),
            )
        });

        if replace_items.is_some() {
            debug!("saved {} tracks to playlist {}", tracks.len(), id);
            while let Some(ref mut tracks) = remainder.clone() {
                // grab the next set of 100 tracks
                remainder = if tracks.len() > 100 {
                    Some(tracks.split_off(100))
                } else {
                    None
                };

                debug!("adding another {} tracks to playlist", tracks.len());
                if self.append_tracks(id, tracks, None).is_ok() {
                    debug!("{} tracks successfully added", tracks.len());
                } else {
                    error!("error saving tracks to playlists {id}");
                    return;
                }
            }
        } else {
            error!("error saving tracks to playlist {id}");
        }
    }

    /// Delete the playlist with the given `id`.
    pub fn delete_playlist(&self, id: &str) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_remove([LibraryId::Playlist(PlaylistId::from_id(id).unwrap())])
        })
        .ok_or(())
    }

    /// Create a playlist with the given `name`, `public` visibility and `description`. Returns the
    /// id of the newly created playlist.
    pub fn create_playlist(
        &self,
        name: &str,
        public: Option<bool>,
        description: Option<&str>,
    ) -> Result<String, ()> {
        let result = self.api_with_retry(|api| {
            api.user_playlist_create(
                UserId::from_id(self.user.as_ref().unwrap()).unwrap(),
                name,
                public,
                None,
                description,
            )
        });
        result.map(|r| r.id.id().to_string()).ok_or(())
    }

    /// Fetch the album with the given `album_id`.
    pub fn album(&self, album_id: &str) -> Result<FullAlbum, ()> {
        debug!("fetching album {album_id}");
        let aid = AlbumId::from_id(album_id).map_err(|_| ())?;
        self.api_with_retry(|api| api.album(aid.clone(), Some(Market::FromToken)))
            .ok_or(())
    }

    /// Fetch the artist with the given `artist_id`.
    pub fn artist(&self, artist_id: &str) -> Result<FullArtist, ()> {
        let aid = ArtistId::from_id(artist_id).map_err(|_| ())?;
        self.api_with_retry(|api| api.artist(aid.clone())).ok_or(())
    }

    /// Fetch the playlist with the given `playlist_id`.
    pub fn playlist(&self, playlist_id: &str) -> Result<FullPlaylist, ()> {
        let pid = PlaylistId::from_id(playlist_id).map_err(|_| ())?;
        self.api_with_retry(|api| {
            let body = api.api_get(&format!("playlists/{}", pid.id()), &Default::default())?;
            let mut playlist: serde_json::Value = serde_json::from_str(&body)?;
            // rspotify's shadow model panics when Spotify omits restricted contents.
            if playlist.get("items").is_none_or(|v| v.is_null())
                && playlist.get("tracks").is_none_or(|v| v.is_null())
            {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "Playlist contents unavailable to this app",
                )
                .into());
            }
            if playlist.get("followers").is_none() {
                playlist["followers"] = serde_json::json!({"href":null, "total":0});
            }
            Ok(serde_json::from_value(playlist)?)
        })
        .ok_or(())
    }

    /// Fetch the track with the given `track_id`.
    pub fn track(&self, track_id: &str) -> Result<FullTrack, ()> {
        let tid = TrackId::from_id(track_id).map_err(|_| ())?;
        self.api_with_retry(|api| api.track(tid.clone(), Some(Market::FromToken)))
            .ok_or(())
    }

    /// Fetch the show with the given `show_id`.
    pub fn show(&self, show_id: &str) -> Result<FullShow, ()> {
        let sid = ShowId::from_id(show_id).map_err(|_| ())?;
        self.api_with_retry(|api| api.get_a_show(sid.clone(), Some(Market::FromToken)))
            .ok_or(())
    }

    /// Fetch the episode with the given `episode_id`.
    pub fn episode(&self, episode_id: &str) -> Result<FullEpisode, ()> {
        let eid = EpisodeId::from_id(episode_id).map_err(|_| ())?;
        self.api_with_retry(|api| api.get_an_episode(eid.clone(), Some(Market::FromToken)))
            .ok_or(())
    }

    /// Search for items of `searchtype` using the provided `query`. Limit the results to `limit`
    /// items with the given `offset` from the start.
    pub fn search(
        &self,
        searchtype: SearchType,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<SearchResult, ()> {
        self.api_with_retry(|api| {
            let limit = limit.min(10).to_string();
            let offset = offset.to_string();
            let params = [
                ("q", query),
                ("type", searchtype.into()),
                ("market", "from_token"),
                ("limit", limit.as_str()),
                ("offset", offset.as_str()),
            ]
            .into_iter()
            .collect();
            let body = api.api_get("search", &params)?;
            let mut result: serde_json::Value = serde_json::from_str(&body)?;
            if let Some(items) = result
                .pointer_mut("/playlists/items")
                .and_then(|v| v.as_array_mut())
            {
                items.retain(|p| !p.is_null());
                for playlist in items {
                    normalize_playlist_reference(playlist);
                }
            }
            Ok(serde_json::from_value(result)?)
        })
        .ok_or(())
    }

    /// Fetch all the current user's playlists.
    pub fn current_user_playlist(&self) -> ApiResult<Playlist> {
        const MAX_LIMIT: u32 = 50;
        let spotify = self.clone();
        let fetch_page = move |offset: u32| {
            debug!("fetching user playlists, offset: {offset}");
            spotify.api_with_retry(|api| {
                match api
                    .api_get(
                        "me/playlists",
                        &[("limit", "50"), ("offset", offset.to_string().as_str())]
                            .into_iter()
                            .collect(),
                    )
                    .and_then(|body| {
                        let mut page: serde_json::Value = serde_json::from_str(&body)?;
                        if let Some(items) = page.get_mut("items").and_then(|v| v.as_array_mut()) {
                            // Development-mode apps can read only owned/collaborative contents.
                            if spotify.cfg.values().client_id.is_some() {
                                items.retain(|p| {
                                    p.pointer("/owner/id").and_then(|v| v.as_str())
                                        == spotify.user.as_deref()
                                        || p.get("collaborative").and_then(|v| v.as_bool())
                                            == Some(true)
                                });
                            }
                            for item in items {
                                normalize_playlist_reference(item);
                            }
                        }
                        Ok(serde_json::from_value::<
                            Page<rspotify::model::SimplifiedPlaylist>,
                        >(page)?)
                    }) {
                    Ok(page) => Ok(ApiPage {
                        offset: page.offset,
                        total: page.total,
                        items: page.items.iter().map(|sp| sp.into()).collect(),
                    }),
                    Err(e) => Err(e),
                }
            })
        };
        ApiResult::new(MAX_LIMIT, Arc::new(fetch_page))
    }

    /// Get the tracks in the playlist given by `playlist_id`.
    pub fn user_playlist_tracks(&self, playlist_id: &str) -> ApiResult<Playable> {
        const MAX_LIMIT: u32 = 50;
        let spotify = self.clone();
        let playlist_id = playlist_id.to_string();
        let fetch_page = move |offset: u32| {
            debug!("fetching playlist {playlist_id} tracks, offset: {offset}");
            spotify.api_with_retry(|api| {
                match api.playlist_items_manual(
                    PlaylistId::from_id(&playlist_id).unwrap(),
                    None,
                    Some(Market::FromToken),
                    Some(MAX_LIMIT),
                    Some(offset),
                ) {
                    Ok(page) => Ok(ApiPage {
                        offset: page.offset,
                        total: page.total,
                        items: page
                            .items
                            .iter()
                            .filter(|pt| {
                                if let Some(t) = pt.item.as_ref()
                                    && !t.is_unknown()
                                {
                                    true
                                } else {
                                    error!("Could not process item {pt:?}, ignoring");
                                    false
                                }
                            })
                            .enumerate()
                            .flat_map(|(index, pt)| {
                                pt.item.as_ref().map(|t| {
                                    let mut playable: Playable = t.into();
                                    // TODO: set these
                                    playable.set_added_at(pt.added_at);
                                    playable.set_list_index(page.offset as usize + index);
                                    playable
                                })
                            })
                            .collect(),
                    }),
                    Err(e) => Err(e),
                }
            })
        };
        ApiResult::new(MAX_LIMIT, Arc::new(fetch_page))
    }

    /// Fetch all the tracks in the album with the given `album_id`. Limit the results to `limit`
    /// items, with `offset` from the beginning.
    pub fn album_tracks(
        &self,
        album_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Page<SimplifiedTrack>, ()> {
        debug!("fetching album tracks {album_id}");
        self.api_with_retry(|api| {
            api.album_track_manual(
                AlbumId::from_id(album_id).unwrap(),
                Some(Market::FromToken),
                Some(limit),
                Some(offset),
            )
        })
        .ok_or(())
    }

    /// Fetch all the albums of the given `artist_id`. `album_type` determines which type of albums
    /// to fetch.
    pub fn artist_albums(
        &self,
        artist_id: &str,
        album_type: Option<AlbumType>,
    ) -> ApiResult<Album> {
        const MAX_SIZE: u32 = 50;
        let spotify = self.clone();
        let artist_id = artist_id.to_string();
        let fetch_page = move |offset: u32| {
            debug!("fetching artist {artist_id} albums, offset: {offset}");
            spotify.api_with_retry(|api| {
                match api.artist_albums_manual(
                    ArtistId::from_id(&artist_id).unwrap(),
                    album_type.as_ref().copied(),
                    Some(Market::FromToken),
                    Some(MAX_SIZE),
                    Some(offset),
                ) {
                    Ok(page) => {
                        let mut albums: Vec<Album> =
                            page.items.iter().map(|sa| sa.into()).collect();
                        albums.sort_by(|a, b| b.year.cmp(&a.year));
                        Ok(ApiPage {
                            offset: page.offset,
                            total: page.total,
                            items: albums,
                        })
                    }
                    Err(e) => Err(e),
                }
            })
        };

        ApiResult::new(MAX_SIZE, Arc::new(fetch_page))
    }

    /// Get all the episodes of the show with the given `show_id`.
    pub fn show_episodes(&self, show_id: &str) -> ApiResult<Episode> {
        const MAX_SIZE: u32 = 50;
        let spotify = self.clone();
        let show_id = show_id.to_string();
        let fetch_page = move |offset: u32| {
            debug!("fetching show {} episodes, offset: {}", &show_id, offset);
            spotify.api_with_retry(|api| {
                match api.get_shows_episodes_manual(
                    ShowId::from_id(&show_id).unwrap(),
                    Some(Market::FromToken),
                    Some(50),
                    Some(offset),
                ) {
                    Ok(page) => Ok(ApiPage {
                        offset: page.offset,
                        total: page.total,
                        items: page.items.iter().map(|se| se.into()).collect(),
                    }),
                    Err(e) => Err(e),
                }
            })
        };

        ApiResult::new(MAX_SIZE, Arc::new(fetch_page))
    }

    /// Get the user's saved shows.
    pub fn get_saved_shows(&self, offset: u32) -> Result<Page<Show>, ()> {
        self.api_with_retry(|api| api.get_saved_show_manual(Some(50), Some(offset)))
            .ok_or(())
    }

    /// Add the shows with the given `ids` to the user's library.
    pub fn save_shows(&self, ids: &[&str]) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_add(
                ids.iter()
                    .map(|id| LibraryId::Show(ShowId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Remove the shows with `ids` from the user's library.
    pub fn unsave_shows(&self, ids: &[&str]) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_remove(
                ids.iter()
                    .map(|id| LibraryId::Show(ShowId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Get the user's followed artists. `last` is an artist id. If it is specified, the artists
    /// after the one with this id will be retrieved.
    pub fn current_user_followed_artists(
        &self,
        last: Option<&str>,
    ) -> Result<CursorBasedPage<FullArtist>, ()> {
        self.api_with_retry(|api| api.current_user_followed_artists(last, Some(50)))
            .ok_or(())
    }

    /// Add the logged in user to the followers of the artists with the given `ids`.
    pub fn user_follow_artists(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_add(
                ids.iter()
                    .map(|id| LibraryId::Artist(ArtistId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Remove the logged in user to the followers of the artists with the given `ids`.
    pub fn user_unfollow_artists(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_remove(
                ids.iter()
                    .map(|id| LibraryId::Artist(ArtistId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Get the user's saved albums, starting at the given `offset`. The result is paginated.
    pub fn current_user_saved_albums(&self, offset: u32) -> Result<Page<SavedAlbum>, ()> {
        self.api_with_retry(|api| {
            api.current_user_saved_albums_manual(Some(Market::FromToken), Some(50), Some(offset))
        })
        .ok_or(())
    }

    /// Add the albums with the given `ids` to the user's saved albums.
    pub fn current_user_saved_albums_add(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_add(
                ids.iter()
                    .map(|id| LibraryId::Album(AlbumId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Remove the albums with the given `ids` from the user's saved albums.
    pub fn current_user_saved_albums_delete(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_remove(
                ids.iter()
                    .map(|id| LibraryId::Album(AlbumId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Get the user's saved tracks, starting at the given `offset`. The result is paginated.
    pub fn current_user_saved_tracks(&self, offset: u32) -> Result<Page<SavedTrack>, ()> {
        self.api_with_retry(|api| {
            api.current_user_saved_tracks_manual(Some(Market::FromToken), Some(50), Some(offset))
        })
        .ok_or(())
    }

    /// Add the tracks with the given `ids` to the user's saved tracks.
    pub fn current_user_saved_tracks_add(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_add(
                ids.iter()
                    .map(|id| LibraryId::Track(TrackId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Remove the tracks with the given `ids` from the user's saved tracks.
    pub fn current_user_saved_tracks_delete(&self, ids: Vec<&str>) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_remove(
                ids.iter()
                    .map(|id| LibraryId::Track(TrackId::from_id(*id).unwrap()))
                    .collect::<Vec<LibraryId>>(),
            )
        })
        .ok_or(())
    }

    /// Add the logged in user to the followers of the playlist with the given `id`.
    pub fn user_playlist_follow_playlist(&self, id: &str) -> Result<(), ()> {
        self.api_with_retry(|api| {
            api.library_add([LibraryId::Playlist(PlaylistId::from_id(id).unwrap())])
        })
        .ok_or(())
    }

    /// Get the top tracks of the artist with the given `id`.
    pub fn artist_top_tracks(&self, id: &str) -> Result<Vec<Track>, ()> {
        #[allow(deprecated)]
        self.api_with_retry(|api| {
            api.artist_top_tracks(ArtistId::from_id(id).unwrap(), Some(Market::FromToken))
        })
        .map(|ft| ft.iter().map(|t| t.into()).collect())
        .ok_or(())
    }

    /// Get artists related to the artist with the given `id`.
    pub fn artist_related_artists(&self, id: &str) -> Result<Vec<Artist>, ()> {
        #[allow(deprecated)]
        self.api_with_retry(|api| api.artist_related_artists(ArtistId::from_id(id).unwrap()))
            .map(|fa| fa.iter().map(|a| a.into()).collect())
            .ok_or(())
    }

    /// Get the available categories.
    pub fn categories(&self) -> ApiResult<Category> {
        const MAX_LIMIT: u32 = 50;
        let spotify = self.clone();
        let fetch_page = move |offset: u32| {
            debug!("fetching categories, offset: {offset}");
            spotify.api_with_retry(|api| {
                #[allow(deprecated)]
                match api.categories_manual(
                    None,
                    Some(Market::FromToken),
                    Some(MAX_LIMIT),
                    Some(offset),
                ) {
                    Ok(page) => Ok(ApiPage {
                        offset: page.offset,
                        total: page.total,
                        items: page.items.iter().map(|cat| cat.into()).collect(),
                    }),
                    Err(e) => Err(e),
                }
            })
        };
        ApiResult::new(MAX_LIMIT, Arc::new(fetch_page))
    }

    /// Get the playlists in the category given by `category_id`.
    pub fn category_playlists(&self, category_id: &str) -> ApiResult<Playlist> {
        const MAX_LIMIT: u32 = 50;
        let spotify = self.clone();
        let category_id = category_id.to_string();
        let fetch_page = move |offset: u32| {
            debug!("fetching category playlists, offset: {offset}");
            spotify.api_with_retry(|api| {
                #[allow(deprecated)]
                match api.category_playlists_manual(
                    &category_id,
                    Some(Market::FromToken),
                    Some(MAX_LIMIT),
                    Some(offset),
                ) {
                    Ok(page) => Ok(ApiPage {
                        offset: page.offset,
                        total: page.total,
                        items: page.items.iter().map(|sp| sp.into()).collect(),
                    }),
                    Err(e) => Err(e),
                }
            })
        };
        ApiResult::new(MAX_LIMIT, Arc::new(fetch_page))
    }

    /// Get details about the logged in user.
    pub fn current_user(&self) -> Result<PrivateUser, ()> {
        self.api_with_retry(|api| api.current_user()).ok_or(())
    }
}

// Track counts are optional for restricted playlists in development mode.
fn normalize_playlist_reference(playlist: &mut serde_json::Value) {
    if playlist.get("items").is_none_or(|v| v.is_null())
        && playlist.get("tracks").is_none_or(|v| v.is_null())
    {
        playlist["items"] = serde_json::json!({"href":"", "total":0});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rate_limit_blocks_clones_without_sleeping_or_retrying() {
        let api = WebApi::new(crate::config::Config::new_for_test());
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        let result: Option<()> = api.api_with_retry(|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            let response = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Type: application/json\r\n\r\n{\"error\":{\"status\":429}}".parse::<ureq::Response>().unwrap();
            Err(ClientError::Http(Box::new(HttpError::StatusCode(response))))
        });
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(api.retry_after().unwrap() > Duration::from_secs(3590));
        let result = api.clone().api_with_retry(|_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        assert!(result.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        *api.request_gate.lock().unwrap() = Some(Instant::now());
        assert_eq!(api.api_with_retry(|_| Ok(42)), Some(42));
        assert!(api.retry_after().is_none());
    }

    #[test]
    fn restricted_playlist_reference_deserializes_without_panicking() {
        let mut playlist = serde_json::json!({
            "collaborative":false, "external_urls":{}, "href":"", "id":"abc",
            "images":[], "name":"Restricted", "owner":{"id":"owner", "external_urls":{}, "href":""},
            "public":true, "snapshot_id":"snapshot"
        });
        normalize_playlist_reference(&mut playlist);
        let parsed: rspotify::model::SimplifiedPlaylist = serde_json::from_value(playlist).unwrap();
        assert_eq!(parsed.items.total, 0);
    }

    #[test]
    #[ignore = "requires personal Web API browser authorization and network access"]
    fn live_personal_library_access() {
        let cfg = Arc::new(crate::config::Config::new(None));
        assert!(
            cfg.values().client_id.is_some(),
            "configure a personal client first"
        );
        let token = crate::authentication::get_web_token(&cfg, false, false).unwrap();
        let mut api = WebApi::new(cfg);
        *api.api.token.lock().unwrap() = Some(token);
        let started = Instant::now();
        let user = api.current_user().expect("profile");
        api.set_user(Some(user.id.id().to_owned()));
        let playlists = api.current_user_playlist();
        assert!(playlists.first_page_loaded(), "playlist enumeration");
        let playlists = playlists.items.read().unwrap();
        println!(
            "Profile and first playlist page: {} accessible playlists in {:.2}s",
            playlists.len(),
            started.elapsed().as_secs_f64()
        );
        if let Some(playlist) = playlists.first() {
            let tracks = api.user_playlist_tracks(&playlist.id);
            assert!(tracks.first_page_loaded(), "playlist items");
            println!(
                "First playlist: {} total songs; {} loaded",
                tracks.total,
                tracks.items.read().unwrap().len()
            );
            api.playlist(&playlist.id).expect("full playlist metadata");
        }
        let saved = api.current_user_saved_tracks(0).expect("liked songs");
        println!(
            "Liked songs: {} total; {} loaded",
            saved.total,
            saved.items.len()
        );
        api.search(SearchType::Track, "Radiohead", 50, 0)
            .expect("track search");
        api.search(SearchType::Playlist, "Radiohead", 50, 0)
            .expect("playlist search");
        println!(
            "All live Web API checks passed in {:.2}s",
            started.elapsed().as_secs_f64()
        );
    }
}
