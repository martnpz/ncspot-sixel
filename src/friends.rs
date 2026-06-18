//! Spotify Friend Activity ("buddylist"), fetched through the librespot
//! session's SpClient. This is an internal Spotify endpoint; the response
//! shape is undocumented, so parsing is defensive and the raw body is logged
//! at debug level to ease iteration.

use std::sync::Arc;

use http::Method;
use librespot_core::session::Session;
use log::{debug, warn};
use serde::Deserialize;

use crate::application::ASYNC_RUNTIME;
use crate::library::Library;
use crate::queue::Queue;
use crate::traits::{ListItem, ViewExt};

/// Appended to the SpClient base URL.
const BUDDYLIST_ENDPOINT: &str = "/presence-view/v1/buddylist";

/// A friend whose last activity is within this window is shown as "online".
const ONLINE_WINDOW_MS: i64 = 5 * 60 * 1000;

#[derive(Deserialize, Default)]
struct BuddylistResponse {
    #[serde(default)]
    friends: Vec<BuddylistFriend>,
}

#[derive(Deserialize)]
struct BuddylistFriend {
    #[serde(default)]
    timestamp: i64,
    user: BuddylistUser,
    track: Option<BuddylistTrack>,
}

#[derive(Deserialize)]
struct BuddylistUser {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "imageUrl", default)]
    image_url: Option<String>,
}

#[derive(Deserialize)]
struct BuddylistTrack {
    #[serde(default)]
    name: String,
    context: Option<BuddylistNamed>,
}

#[derive(Deserialize)]
struct BuddylistNamed {
    #[serde(default)]
    uri: String,
    #[serde(default)]
    name: String,
}

/// A friend's current/last listening activity.
#[derive(Clone, Debug)]
pub struct Friend {
    pub user_uri: String,
    pub name: String,
    pub image_url: Option<String>,
    pub track_name: Option<String>,
    /// Listening context (usually the playlist/album): its Spotify URI and name.
    pub context_uri: Option<String>,
    pub context_name: Option<String>,
    /// Unix epoch milliseconds of the friend's last activity.
    pub timestamp_ms: i64,
}

impl Friend {
    /// True when the friend's last activity is recent enough to treat them as
    /// currently online/listening.
    pub fn is_online(&self) -> bool {
        now_ms().saturating_sub(self.timestamp_ms) <= ONLINE_WINDOW_MS
    }

    /// Human-readable time since the friend was last active, e.g. "now",
    /// "3m", "2h", "5d".
    pub fn last_seen(&self) -> String {
        if self.is_online() {
            return "now".to_string();
        }
        let secs = now_ms().saturating_sub(self.timestamp_ms) / 1000;
        let mins = secs / 60;
        let hours = mins / 60;
        let days = hours / 24;
        if days > 0 {
            format!("{days}d")
        } else if hours > 0 {
            format!("{hours}h")
        } else if mins > 0 {
            format!("{mins}m")
        } else {
            "now".to_string()
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl ListItem for Friend {
    fn is_playing(&self, _queue: &Queue) -> bool {
        false
    }

    // Row 1: name (left) and time since last online (right).
    fn display_left(&self, _library: &Library) -> String {
        self.name.clone()
    }

    fn display_right(&self, _library: &Library) -> String {
        self.last_seen()
    }

    // Row 2: song title (left) and listening context / playlist (right).
    fn display_second_left(&self, _library: &Library) -> String {
        self.track_name.clone().unwrap_or_default()
    }

    fn display_second_right(&self, _library: &Library) -> String {
        self.context_name.clone().unwrap_or_default()
    }

    // Online friends' names are drawn in the Secondary palette color.
    fn left_color(&self, _library: &Library) -> Option<cursive::theme::ColorType> {
        use cursive::theme::{ColorType, PaletteColor};
        self.is_online()
            .then_some(ColorType::Palette(PaletteColor::Secondary))
    }

    fn play(&mut self, _queue: &Queue) {}
    fn play_next(&mut self, _queue: &Queue) {}
    fn queue(&mut self, _queue: &Queue) {}
    fn toggle_saved(&mut self, _library: &Library) {}
    fn save(&mut self, _library: &Library) {}
    fn unsave(&mut self, _library: &Library) {}

    fn open(&self, queue: Arc<Queue>, library: Arc<Library>) -> Option<Box<dyn ViewExt>> {
        // Open what the friend is listening to: their listening context
        // (playlist or album).
        let uri = self.context_uri.as_deref()?;
        let id = uri.rsplit(':').next()?;
        let spotify = queue.get_spotify();
        if uri.contains(":playlist:") {
            let full = spotify.api.playlist(id).ok()?;
            crate::model::playlist::Playlist::from(&full).open(queue, library)
        } else if uri.contains(":album:") {
            let full = spotify.api.album(id).ok()?;
            crate::model::album::Album::from(&full).open(queue, library)
        } else {
            None
        }
    }

    fn share_url(&self) -> Option<String> {
        (!self.user_uri.is_empty()).then(|| {
            let id = self.user_uri.rsplit(':').next().unwrap_or(&self.user_uri);
            format!("https://open.spotify.com/user/{id}")
        })
    }

    fn cover_url(&self) -> Option<String> {
        self.image_url.clone()
    }

    fn as_listitem(&self) -> Box<dyn ListItem> {
        Box::new(self.clone())
    }
}

/// Fetch the current user's friend activity. Returns an empty list (and logs)
/// on any error so the UI degrades gracefully.
pub fn fetch(session: &Session) -> Vec<Friend> {
    let result = ASYNC_RUNTIME.get().unwrap().block_on(session.spclient().request_as_json(
        &Method::GET,
        BUDDYLIST_ENDPOINT,
        None,
        None,
    ));

    let bytes = match result {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("buddylist request failed: {e}");
            return Vec::new();
        }
    };

    debug!(
        "buddylist response ({} bytes): {}",
        bytes.len(),
        String::from_utf8_lossy(&bytes)
    );

    let parsed: BuddylistResponse = match serde_json::from_slice(&bytes) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!("unexpected buddylist response: {e}");
            return Vec::new();
        }
    };

    let friends: Vec<Friend> = parsed
        .friends
        .into_iter()
        .map(|f| {
            let track = f.track;
            Friend {
                user_uri: f.user.uri,
                name: f.user.name,
                image_url: f.user.image_url,
                track_name: track.as_ref().map(|t| t.name.clone()),
                context_uri: track
                    .as_ref()
                    .and_then(|t| t.context.as_ref().map(|c| c.uri.clone())),
                context_name: track
                    .as_ref()
                    .and_then(|t| t.context.as_ref().map(|c| c.name.clone())),
                timestamp_ms: f.timestamp,
            }
        })
        .collect();

    debug!("buddylist parsed {} friends", friends.len());
    friends
}
