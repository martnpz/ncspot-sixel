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
    let duration_s = duration_ms.div_ceil(1000);

    let client = match reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!("failed to create lrclib HTTP client: {e}");
            return None;
        }
    };

    let mut query = vec![
        ("artist_name", artist.to_string()),
        ("track_name", title.to_string()),
        ("duration", duration_s.to_string()),
    ];
    if let Some(album) = album {
        query.push(("album_name", album.to_string()));
    }

    match client.get(API_GET).query(&query).send() {
        Ok(response) if response.status().is_success() => {
            if let Ok(found) = response.json::<LrclibResponse>() {
                debug!("lrclib match for {artist} - {title}");
                return convert(found);
            }
            None
        }
        Ok(response) if response.status().as_u16() == 404 => {
            debug!("no exact lrclib match for {artist} - {title}, searching");
            search(&client, artist, title)
        }
        Ok(response) => {
            warn!("lrclib returned {} for {artist} - {title}", response.status());
            None
        }
        Err(e) => {
            warn!("lrclib request failed: {e}");
            None
        }
    }
}

fn search(client: &reqwest::blocking::Client, artist: &str, title: &str) -> Option<Lyrics> {
    let response = client
        .get(API_SEARCH)
        .query(&[("artist_name", artist), ("track_name", title)])
        .send()
        .map_err(|e| warn!("lrclib search failed: {e}"))
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let results: Vec<LrclibResponse> = response.json().ok()?;
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
