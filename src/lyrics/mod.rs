//! Synced lyrics support.
//!
//! Lyrics are fetched from lrclib.net first and from Spotify's color-lyrics
//! endpoint (through the existing librespot session) as fallback. Results are
//! cached on disk per track, including negative results so missing tracks
//! aren't re-requested on every play.

pub mod lrc;
mod lrclib;
mod spotify;

use std::fs;
use std::sync::{Arc, RwLock};

use log::{debug, error, warn};

use crate::config;
use crate::events::EventManager;
use crate::model::playable::Playable;
use crate::spotify::Spotify;

/// A single time-tagged lyrics line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsLine {
    /// Start time of the line relative to the start of the track.
    pub time_ms: u32,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LyricsSource {
    Lrclib,
    Spotify,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lyrics {
    /// Time-synced lines, sorted by time, if the source provides them.
    pub synced: Option<Vec<LyricsLine>>,
    /// Plain lyrics text as fallback for unsynced sources.
    pub plain: Option<String>,
    pub source: LyricsSource,
}

#[derive(Clone)]
pub enum LyricsStatus {
    Loading,
    Found(Arc<Lyrics>),
    NotFound,
}

struct LyricsState {
    track_id: Option<String>,
    status: LyricsStatus,
}

/// Shared owner of the lyrics for the currently playing track.
pub struct LyricsManager {
    state: RwLock<LyricsState>,
    events: EventManager,
    spotify: Spotify,
}

impl LyricsManager {
    pub fn new(events: EventManager, spotify: Spotify) -> Self {
        Self {
            state: RwLock::new(LyricsState {
                track_id: None,
                status: LyricsStatus::NotFound,
            }),
            events,
            spotify,
        }
    }

    /// The lyrics status for `track_id`, or None if lyrics for a different track are loaded.
    pub fn status(&self, track_id: &str) -> Option<LyricsStatus> {
        let state = self.state.read().unwrap();
        (state.track_id.as_deref() == Some(track_id)).then(|| state.status.clone())
    }

    /// Fetch lyrics for `playable` on a background thread. Fetches are deduplicated by track id,
    /// so calling this on every track change is fine.
    pub fn fetch(self: &Arc<Self>, playable: &Playable) {
        let track = match playable {
            Playable::Track(track) => track.clone(),
            Playable::Episode(episode) => {
                let mut state = self.state.write().unwrap();
                state.track_id = Some(episode.id.clone());
                state.status = LyricsStatus::NotFound;
                return;
            }
        };
        let Some(track_id) = track.id.clone() else {
            let mut state = self.state.write().unwrap();
            state.track_id = None;
            state.status = LyricsStatus::NotFound;
            return;
        };

        {
            let mut state = self.state.write().unwrap();
            if state.track_id.as_deref() == Some(&track_id) {
                return;
            }
            state.track_id = Some(track_id.clone());
            state.status = LyricsStatus::Loading;
        }

        let manager = Arc::clone(self);
        std::thread::spawn(move || {
            let lyrics = manager.resolve(&track_id, &track.artists, &track.title, track.album.as_deref(), track.duration);

            let mut state = manager.state.write().unwrap();
            // Only publish if no newer track took over in the meantime.
            if state.track_id.as_deref() == Some(&track_id) {
                state.status = match lyrics {
                    Some(lyrics) => LyricsStatus::Found(Arc::new(lyrics)),
                    None => LyricsStatus::NotFound,
                };
                drop(state);
                manager.events.trigger();
            }
        });
    }

    /// Cache lookup, then lrclib, then Spotify. Returns None if no source has lyrics.
    fn resolve(
        &self,
        track_id: &str,
        artists: &[String],
        title: &str,
        album: Option<&str>,
        duration_ms: u32,
    ) -> Option<Lyrics> {
        let cache_dir = config::cache_path("lyrics");
        let cache_file = cache_dir.join(format!("{track_id}.json"));

        if let Ok(cached) = fs::read_to_string(&cache_file) {
            match serde_json::from_str::<Option<Lyrics>>(&cached) {
                Ok(lyrics) => {
                    debug!("lyrics for {track_id} loaded from cache");
                    return lyrics;
                }
                Err(e) => warn!("discarding broken lyrics cache {cache_file:?}: {e}"),
            }
        }

        let lyrics = lrclib::fetch(artists, title, album, duration_ms).or_else(|| {
            self.spotify
                .session()
                .and_then(|session| spotify::fetch(&session, track_id))
        });

        if let Err(e) = fs::create_dir_all(&cache_dir)
            .and_then(|()| fs::write(&cache_file, serde_json::to_string(&lyrics).unwrap()))
        {
            error!("failed to write lyrics cache {cache_file:?}: {e}");
        }
        lyrics
    }
}
