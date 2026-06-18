//! Client for the lrclib.net lyrics API.

use log::{debug, info, warn};
use serde::Deserialize;

use super::{Lyrics, LyricsSource, lrc};

const API_GET: &str = "https://lrclib.net/api/get";
const API_SEARCH: &str = "https://lrclib.net/api/search";
// lrclib asks clients to identify themselves.
const USER_AGENT: &str = concat!(
    "ncspot-sixel/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/hrkfdn/ncspot)"
);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    instrumental: Option<bool>,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

/// Fetch lyrics from lrclib.net, trying an exact signature lookup first and a
/// fuzzy search as fallback. Returns None when lrclib doesn't know the track.
pub fn fetch(artists: &[String], title: &str, album: Option<&str>, duration_ms: u32) -> Option<Lyrics> {
    let artist = artists.first().map(String::as_str).unwrap_or_default();
    let duration_s = duration_ms.div_ceil(1000).to_string();

    let agent = crate::utils::http_agent();
    let mut request = agent
        .get(API_GET)
        .set("User-Agent", USER_AGENT)
        .query("artist_name", artist)
        .query("track_name", title)
        .query("duration", &duration_s);
    if let Some(album) = album {
        request = request.query("album_name", album);
    }

    match request.call() {
        Ok(response) => {
            if let Ok(found) = response.into_json::<LrclibResponse>() {
                debug!("lrclib match for {artist} - {title}");
                return convert(found);
            }
            None
        }
        Err(ureq::Error::Status(404, _)) => {
            debug!("no exact lrclib match for {artist} - {title}, searching");
            search(agent, artist, title)
        }
        Err(ureq::Error::Status(code, _)) => {
            warn!("lrclib returned {code} for {artist} - {title}");
            None
        }
        Err(e) => {
            warn!("lrclib request failed: {e}");
            None
        }
    }
}

fn search(agent: &ureq::Agent, artist: &str, title: &str) -> Option<Lyrics> {
    let response = agent
        .get(API_SEARCH)
        .set("User-Agent", USER_AGENT)
        .query("artist_name", artist)
        .query("track_name", title)
        .call()
        .map_err(|e| warn!("lrclib search failed: {e}"))
        .ok()?;
    let results: Vec<LrclibResponse> = response.into_json().ok()?;
    info!("lrclib search for {artist} - {title}: {} results", results.len());
    results
        .into_iter()
        // Prefer a synced result over plain text.
        .max_by_key(|r| r.synced_lyrics.is_some())
        .and_then(convert)
}

fn convert(response: LrclibResponse) -> Option<Lyrics> {
    if response.instrumental == Some(true) {
        return Some(Lyrics {
            synced: None,
            plain: Some("♪ instrumental ♪".into()),
            source: LyricsSource::Lrclib,
        });
    }

    let synced = response
        .synced_lyrics
        .as_deref()
        .map(lrc::parse)
        .filter(|lines| !lines.is_empty());
    let plain = response.plain_lyrics.filter(|text| !text.trim().is_empty());

    if synced.is_none() && plain.is_none() {
        return None;
    }
    Some(Lyrics {
        synced,
        plain,
        source: LyricsSource::Lrclib,
    })
}
