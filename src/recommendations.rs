//! Recommendations through the authenticated playback session, independent of Web API quotas.
use std::collections::HashSet;
use std::time::Duration;

use futures::{StreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::audio::item::{AudioItem, UniqueFields};
use librespot_protocol::autoplay_context_request::AutoplayContextRequest;
use log::{debug, warn};

use crate::model::track::Track;

pub struct Recommendations {
    pub tracks: Vec<Track>,
}

/// Requests are bounded and metadata concurrency is limited. No Web API fallback:
/// development-mode apps cannot use the old Recommendations endpoint.
pub async fn fetch(
    session: &Session,
    contexts: &[String],
    excluded: &HashSet<String>,
    limit: usize,
) -> Result<Vec<Track>, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut seen = excluded.clone();
    let mut tracks = Vec::new();
    let mut last_error = "Spotify returned no playable suggestions".to_owned();
    'contexts: for context in contexts.iter().take(5) {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let request = AutoplayContextRequest {
            context_uri: Some(context.clone()),
            recent_track_uri: contexts.to_vec(),
            ..Default::default()
        };
        let mut candidates = Vec::new();
        let mut next_page = None;
        match tokio::time::timeout(
            Duration::from_secs(12),
            session.spclient().get_autoplay_context(&request),
        )
        .await
        {
            Ok(Ok(ctx)) => {
                for page in ctx.pages {
                    candidates.extend(page.tracks.into_iter().filter_map(|t| t.uri));
                    next_page = page.next_page_url.or(page.page_url).or(next_page);
                }
            }
            result => {
                last_error = format!("Autoplay request failed: {result:?}");
                // Radio is another playback-session service. Its response includes track URIs.
                match tokio::time::timeout(
                    Duration::from_secs(12),
                    session.spclient().get_apollo_station(
                        "tracks",
                        context,
                        Some(limit.min(100)),
                        vec![],
                        true,
                    ),
                )
                .await
                {
                    Ok(Ok(bytes)) => {
                        let page: serde_json::Value =
                            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                        (candidates, next_page) = radio_page(&page);
                    }
                    result => {
                        debug!("Radio fallback failed: {result:?}");
                        continue;
                    }
                }
            }
        }
        let mut visited_pages = HashSet::new();
        for _ in 0..20 {
            let uris: Vec<_> = candidates
                .drain(..)
                .filter(|uri| uri.starts_with("spotify:track:") && seen.insert(uri.clone()))
                .collect();
            let mut metadata = stream::iter(uris)
                .map(|uri| async move {
                    let parsed = SpotifyUri::from_uri(&uri).ok()?;
                    let item = tokio::time::timeout(
                        Duration::from_secs(8),
                        AudioItem::get_file(session, parsed),
                    )
                    .await
                    .ok()?
                    .ok()?;
                    playable_track(item)
                })
                .buffered(4);
            while let Some(track) = metadata.next().await {
                if let Some(track) = track {
                    tracks.push(track);
                }
                if tracks.len() >= limit {
                    return Ok(tracks);
                }
                if std::time::Instant::now() >= deadline {
                    break 'contexts;
                }
            }
            let Some(url) = next_page
                .take()
                .filter(|u| !u.is_empty() && visited_pages.insert(u.clone()))
            else {
                break;
            };
            match tokio::time::timeout(
                Duration::from_secs(12),
                session.spclient().get_next_page(&url),
            )
            .await
            {
                Ok(Ok(bytes)) => {
                    let page: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                    (candidates, next_page) = radio_page(&page);
                }
                result => {
                    warn!("Recommendation page failed: {result:?}");
                    break;
                }
            }
        }
    }
    if tracks.is_empty() {
        Err(last_error)
    } else {
        Ok(tracks)
    }
}

fn radio_page(page: &serde_json::Value) -> (Vec<String>, Option<String>) {
    let tracks = page
        .get("tracks")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("uri").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let next = page
        .get("next_page_url")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    (tracks, next)
}

fn playable_track(item: AudioItem) -> Option<Track> {
    if item.availability.is_err() || item.files.is_empty() {
        return None;
    }
    let id = item.track_id.to_id().ok()?;
    let UniqueFields::Track {
        artists,
        album,
        album_artists,
        number,
        disc_number,
        ..
    } = item.unique_fields
    else {
        return None;
    };
    Some(Track {
        id: Some(id.clone()),
        uri: item.uri,
        title: item.name,
        track_number: number,
        disc_number: disc_number as i32,
        duration: item.duration_ms,
        artists: artists.iter().map(|a| a.name.clone()).collect(),
        artist_ids: artists.iter().filter_map(|a| a.id.to_id().ok()).collect(),
        album: Some(album),
        album_id: None,
        album_artists,
        cover_url: item.covers.first().map(|c| c.url.clone()),
        url: format!("https://open.spotify.com/track/{id}"),
        added_at: None,
        list_index: 0,
        is_local: false,
        is_playable: Some(true),
        is_suggested: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires cached Spotify playback credentials and network access"]
    fn live_session_recommendations() {
        use librespot_core::{cache::Cache, config::SessionConfig};
        let cache = Cache::new(
            Some(crate::config::cache_path("librespot")),
            None,
            None,
            None,
        )
        .unwrap();
        let credentials = cache.credentials().expect("cached playback credentials");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let session = Session::new(
                SessionConfig {
                    client_id: crate::authentication::SPOTIFY_CLIENT_ID.to_owned(),
                    ..Default::default()
                },
                None,
            );
            tokio::time::timeout(Duration::from_secs(20), session.connect(credentials, false))
                .await
                .unwrap()
                .unwrap();
            let contexts = vec!["spotify:track:4cOdK2wGLETKBW3PvgPWqT".to_owned()];
            let excluded = contexts.iter().cloned().collect();
            let started = std::time::Instant::now();
            let tracks = fetch(&session, &contexts, &excluded, 10).await.unwrap();
            assert_eq!(tracks.len(), 10);
            assert_eq!(
                tracks.iter().map(|t| &t.uri).collect::<HashSet<_>>().len(),
                tracks.len()
            );
            assert!(tracks.iter().all(|t| t.is_suggested
                && t.is_playable == Some(true)
                && !t.title.is_empty()
                && !excluded.contains(&t.uri)));
            println!(
                "Resolved {} unique playable suggestions, with metadata, in {:.2}s",
                tracks.len(),
                started.elapsed().as_secs_f64()
            );
            session.shutdown();
        });
    }

    #[test]
    fn radio_page_ignores_entries_without_uris() {
        let page = serde_json::json!({"tracks": [{"uri":"spotify:track:a"}, {}, {"uri":null}], "next_page_url":"hm:/page/2"});
        let (tracks, next) = radio_page(&page);
        assert_eq!(tracks, ["spotify:track:a"]);
        assert_eq!(next.as_deref(), Some("hm:/page/2"));
    }
}
