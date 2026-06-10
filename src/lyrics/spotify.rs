//! Fallback lyrics source: Spotify's color-lyrics endpoint, accessed through
//! the existing librespot session.

use librespot_core::SpotifyId;
use librespot_core::session::Session;
use log::{debug, warn};
use serde::Deserialize;

use crate::application::ASYNC_RUNTIME;

use super::{Lyrics, LyricsLine, LyricsSource};

#[derive(Deserialize)]
struct ColorLyricsResponse {
    lyrics: ColorLyrics,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ColorLyrics {
    sync_type: Option<String>,
    lines: Vec<ColorLyricsLine>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ColorLyricsLine {
    start_time_ms: String,
    words: String,
}

/// Fetch lyrics for `track_id` (base62) from Spotify. Returns None when the
/// track has no lyrics or the endpoint rejects the request.
pub fn fetch(session: &Session, track_id: &str) -> Option<Lyrics> {
    let id = SpotifyId::from_base62(track_id)
        .map_err(|e| warn!("invalid track id {track_id}: {e}"))
        .ok()?;

    let response = ASYNC_RUNTIME
        .get()
        .unwrap()
        .block_on(session.spclient().get_lyrics(&id));
    let bytes = match response {
        Ok(bytes) => bytes,
        Err(e) => {
            // 404 here just means "no lyrics for this track".
            debug!("spotify lyrics request for {track_id} failed: {e}");
            return None;
        }
    };

    let parsed: ColorLyricsResponse = serde_json::from_slice(&bytes)
        .map_err(|e| warn!("unexpected spotify lyrics response: {e}"))
        .ok()?;

    let synced = (parsed.lyrics.sync_type.as_deref() == Some("LINE_SYNCED")).then(|| {
        let mut lines: Vec<LyricsLine> = parsed
            .lyrics
            .lines
            .iter()
            .filter_map(|line| {
                Some(LyricsLine {
                    time_ms: line.start_time_ms.parse().ok()?,
                    text: line.words.clone(),
                })
            })
            .collect();
        lines.sort_by_key(|line| line.time_ms);
        lines
    });

    let plain = (!parsed.lyrics.lines.is_empty()).then(|| {
        parsed
            .lyrics
            .lines
            .iter()
            .map(|line| line.words.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    });

    if synced.as_ref().is_none_or(|lines| lines.is_empty()) && plain.is_none() {
        return None;
    }
    debug!("found spotify lyrics for {track_id}");
    Some(Lyrics {
        synced: synced.filter(|lines| !lines.is_empty()),
        plain,
        source: LyricsSource::Spotify,
    })
}
