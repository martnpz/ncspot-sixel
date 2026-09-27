//! Artist details unavailable through development-mode Web API endpoints.
use std::time::Duration;

use futures::{StreamExt, TryStreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::{Metadata, artist::Artist as SessionArtist, audio::item::AudioItem};

use crate::model::{artist::Artist, track::Track};

pub async fn top_tracks(session: &Session, id: &str) -> Result<Vec<Track>, String> {
    tokio::time::timeout(Duration::from_secs(45), async {
        let uri =
            SpotifyUri::from_uri(&format!("spotify:artist:{id}")).map_err(|e| e.to_string())?;
        let artist = SessionArtist::get(session, &uri)
            .await
            .map_err(|e| e.to_string())?;
        let uris = artist.top_tracks.for_country(&session.country());
        let tracks = stream::iter(uris.iter().take(10).cloned().enumerate())
            .map(|(index, uri)| async move {
                let audio = tokio::time::timeout(
                    Duration::from_secs(15),
                    AudioItem::get_file(session, uri),
                )
                .await
                .map_err(|_| "Artist track metadata timed out".to_owned())?
                .map_err(|e| e.to_string())?;
                let mut track =
                    Track::from_audio_item(audio).ok_or("Invalid artist track metadata")?;
                track.list_index = index;
                Ok::<_, String>(track)
            })
            .buffered(4)
            .try_collect()
            .await?;
        Ok(tracks)
    })
    .await
    .map_err(|_| "Artist details timed out".to_owned())?
}

// Public persisted-query identifier, not a credential. Spotify may rotate it.
// Protocol reference: https://github.com/Spotui/Spotui/blob/main/spotify/src/main/kotlin/com/metrolist/spotify/SpotifyHashProvider.kt
const ARTIST_OVERVIEW_QUERY: &str =
    "5b9e64f43843fa3a9b6a98543600299b0a2cbbbccfdcdcef2402eb9c1017ca4c";

pub async fn related_artists(session: &Session, id: &str) -> Result<Vec<Artist>, String> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let token = session
            .login5()
            .auth_token()
            .await
            .map_err(|e| e.to_string())?;
        let client_token = session
            .spclient()
            .client_token()
            .await
            .map_err(|e| e.to_string())?;
        let body = serde_json::to_vec(&serde_json::json!({
            "operationName": "queryArtistOverview",
            "variables": {"uri": format!("spotify:artist:{id}"), "locale": ""},
            "extensions": {"persistedQuery": {"version": 1, "sha256Hash": ARTIST_OVERVIEW_QUERY}}
        }))
        .map_err(|e| e.to_string())?;
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("https://api-partner.spotify.com/pathfinder/v2/query")
            .header(
                http::header::AUTHORIZATION,
                format!("{} {}", token.token_type, token.access_token),
            )
            .header("client-token", client_token)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .map_err(|e| e.to_string())?;
        let bytes = session
            .http_client()
            .request_body(request)
            .await
            .map_err(|e| e.to_string())?;
        let response: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        parse_related_artists(&response, id)
    })
    .await
    .map_err(|_| "Related artists request timed out".to_owned())?
}

fn parse_related_artists(
    response: &serde_json::Value,
    source_id: &str,
) -> Result<Vec<Artist>, String> {
    if response
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err("Spotify rejected the artist-overview query".to_owned());
    }
    let items = response
        .pointer("/data/artistUnion/relatedContent/relatedArtists/items")
        .and_then(serde_json::Value::as_array)
        .ok_or("Missing related artists in Spotify's response")?;
    let mut seen = std::collections::HashSet::from([source_id.to_owned()]);
    let mut artists = Vec::new();
    for item in items {
        let uri = item
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .ok_or("Missing artist URI")?;
        let SpotifyUri::Artist { id } = SpotifyUri::from_uri(uri).map_err(|e| e.to_string())?
        else {
            return Err("Invalid related artist URI".to_owned());
        };
        let id = id.to_base62().map_err(|e| e.to_string())?;
        let name = item
            .pointer("/profile/name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or("Missing artist name")?;
        if seen.insert(id.clone()) {
            artists.push(Artist::new(id, name.to_owned()));
        }
    }
    Ok(artists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_core::{cache::Cache, config::SessionConfig};

    fn response(items: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"data":{"artistUnion":{"relatedContent":{"relatedArtists":{"items":items}}}}})
    }

    #[test]
    fn related_artist_parser_preserves_order_and_excludes_duplicates_and_source() {
        let first = "1".repeat(22);
        let second = "2".repeat(22);
        let source = "3".repeat(22);
        let item = |id: &str, name: &str| serde_json::json!({"uri":format!("spotify:artist:{id}"),"profile":{"name":name}});
        let data = response(serde_json::json!([
            item(&first, "First"),
            item(&source, "Source"),
            item(&first, "First"),
            item(&second, "Second")
        ]));
        let artists = parse_related_artists(&data, &source).unwrap();
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].id.as_deref(), Some(first.as_str()));
        assert_eq!(artists[1].name, "Second");
    }

    #[test]
    fn related_artist_parser_distinguishes_empty_results_from_errors() {
        assert!(
            parse_related_artists(&response(serde_json::json!([])), "source")
                .unwrap()
                .is_empty()
        );
        assert!(parse_related_artists(&serde_json::json!({}), "source").is_err());
        assert!(
            parse_related_artists(
                &serde_json::json!({"errors":[{"message":"PersistedQueryNotFound"}]}),
                "source"
            )
            .is_err()
        );
        assert!(parse_related_artists(&response(serde_json::json!([{"uri":format!("spotify:track:{}", "1".repeat(22)),"profile":{"name":"Wrong type"}}])), "source").is_err());
        assert!(parse_related_artists(&response(serde_json::json!([{"uri":format!("spotify:artist:{}", "1".repeat(22)),"profile":{"name":""}}])), "source").is_err());
    }

    #[test]
    #[ignore = "requires cached playback credentials and network; no audio is started"]
    fn live_artist_details() {
        let cache = Cache::new(
            Some(crate::config::cache_path("librespot")),
            None,
            None,
            None,
        )
        .unwrap();
        let credentials = cache.credentials().expect("cached playback login");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
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
            let tracks = top_tracks(&session, "43ZHCT0cAZBISjO8DG9PnE")
                .await
                .unwrap();
            assert_eq!(tracks.len(), 10);
            assert!(
                tracks
                    .iter()
                    .all(|t| !t.title.is_empty() && !t.artists.is_empty() && !t.is_suggested)
            );
            println!("Artist details: {} top tracks with metadata", tracks.len());
            let related = related_artists(&session, "43ZHCT0cAZBISjO8DG9PnE")
                .await
                .unwrap();
            assert!(!related.is_empty());
            println!("Artist overview supplied {} related artists", related.len());
            let second = related_artists(&session, "4Z8W4fKeB5YxbusRsdQVPb")
                .await
                .unwrap();
            assert!(!second.is_empty());
            assert_ne!(related[0].id, second[0].id);
            println!(
                "Second artist overview supplied {} related artists",
                second.len()
            );
            session.shutdown();
        });
    }
}
