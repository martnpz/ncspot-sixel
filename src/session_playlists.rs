//! Read-only access to saved Spotify mixes and Blend via the playback session.
use crate::model::{playable::Playable, playlist::Playlist, track::Track};
use futures::{StreamExt, TryStreamExt, stream};
use librespot_core::{Session, SpotifyUri};
use librespot_metadata::audio::item::AudioItem;
use librespot_protocol::playlist4_external::{ListItems, SelectedListContent};
use protobuf::Message;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

fn revision(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetch saved entries only; unsaved mixes can be added in the official app.
pub async fn saved_mixes(session: &Session) -> Result<Vec<Playlist>, String> {
    tokio::time::timeout(Duration::from_secs(60), saved_mixes_inner(session))
        .await
        .map_err(|_| "Saved playlist enumeration timed out".to_owned())?
}

async fn saved_mixes_inner(session: &Session) -> Result<Vec<Playlist>, String> {
    let mut offset = 0;
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut snapshot = None;
    for _ in 0..100 {
        let bytes = tokio::time::timeout(
            REQUEST_TIMEOUT,
            session.spclient().get_rootlist(offset, Some(200)),
        )
        .await
        .map_err(|_| "Saved playlist request timed out")?
        .map_err(|e| e.to_string())?;
        let page = SelectedListContent::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
        let current = revision(page.revision());
        if snapshot
            .as_ref()
            .is_some_and(|previous| previous != &current)
        {
            return Err(
                "Saved playlists changed during pagination; retry on next update".to_owned(),
            );
        }
        snapshot = Some(current);
        let content = page
            .contents
            .as_ref()
            .ok_or("Missing saved playlist page")?;
        result.extend(
            mixes_from_page(content)?
                .into_iter()
                .filter(|p| seen.insert(p.id.clone())),
        );
        let Some(next) = next_offset(content, page.length(), offset)? else {
            return Ok(result);
        };
        offset = next;
    }
    Err("Saved playlist enumeration exceeded its page limit".to_owned())
}

fn mixes_from_page(content: &ListItems) -> Result<Vec<Playlist>, String> {
    if content.items.len() != content.meta_items.len() {
        return Err("Missing saved playlist metadata; keeping the existing cache".to_owned());
    }
    let mut result = Vec::new();
    for (item, meta) in content.items.iter().zip(&content.meta_items) {
        let attributes = meta.attributes.get_or_default();
        if attributes.deleted_by_owner() {
            continue;
        }
        if meta.owner_username() != "spotify" && attributes.format() != "blend" {
            continue;
        }
        if meta.status_code() != 0 && !(200..300).contains(&meta.status_code()) {
            return Err(format!(
                "Saved playlist metadata unavailable (status {})",
                meta.status_code()
            ));
        }
        let Ok(SpotifyUri::Playlist { id, .. }) = SpotifyUri::from_uri(item.uri()) else {
            continue;
        };
        result.push(Playlist {
            id: id.to_base62().map_err(|e| e.to_string())?,
            name: attributes.name().to_owned(),
            owner_id: meta.owner_username().to_owned(),
            owner_name: None,
            snapshot_id: revision(meta.revision()),
            num_tracks: meta.length().max(0) as usize,
            tracks: None,
            collaborative: false,
            session_playlist: true,
            cover_url: attributes
                .picture_size
                .iter()
                .map(|p| p.url())
                .find(|u| u.starts_with("https://"))
                .map(str::to_owned),
        });
    }
    Ok(result)
}

fn next_offset(content: &ListItems, total: i32, expected: usize) -> Result<Option<usize>, String> {
    if total < 0 || content.pos() < 0 || content.pos() as usize != expected {
        return Err("Spotify returned an inconsistent playlist page".to_owned());
    }
    let next = expected + content.items.len();
    if !content.truncated() && next >= total as usize {
        return Ok(None);
    }
    if next <= expected {
        return Err("Spotify returned a non-advancing playlist page".to_owned());
    }
    Ok(Some(next))
}

fn playlist_contents(
    page: &SelectedListContent,
    offset: usize,
    confirmed_empty: bool,
) -> Result<Option<&ListItems>, String> {
    if let Some(content) = page.contents.as_ref() {
        return Ok(Some(content));
    }
    // Spotify omits the contents message on explicitly empty playlists.
    if offset == 0
        && (page.length == Some(0)
            || (confirmed_empty
                && page.length.is_none()
                && page.attributes.is_some()
                && !page.revision().is_empty()
                && page.diff.is_none()
                && page.sync_result.is_none()))
    {
        return Ok(None);
    }
    Err(format!(
        "Missing playlist contents (reported length: {:?})",
        page.length
    ))
}

/// Preserve playlist order, duplicates and unavailable tracks; never cache partial contents.
pub async fn tracks(
    session: &Session,
    playlist: &Playlist,
    cached: Option<&Playlist>,
) -> Result<(Vec<Playable>, String), String> {
    let confirmed_empty = playlist.num_tracks == 0
        && cached.is_none_or(|p| p.tracks.as_ref().is_none_or(Vec::is_empty));
    tokio::time::timeout(
        Duration::from_secs(90),
        tracks_inner(session, &playlist.id, cached, confirmed_empty),
    )
    .await
    .map_err(|_| "Playlist contents timed out; keeping the existing cache".to_owned())?
}

async fn tracks_inner(
    session: &Session,
    id: &str,
    cached: Option<&Playlist>,
    confirmed_empty: bool,
) -> Result<(Vec<Playable>, String), String> {
    let uri = SpotifyUri::from_uri(&format!("spotify:playlist:{id}")).map_err(|e| e.to_string())?;
    let SpotifyUri::Playlist { id, .. } = uri else {
        return Err("Invalid playlist ID".to_owned());
    };
    let mut items = Vec::new();
    let mut offset = 0;
    let mut snapshot = None;
    let mut finished = false;
    for _ in 0..100 {
        let endpoint = format!(
            "/playlist/v2/playlist/{}?from={offset}&length=200",
            id.to_base62().map_err(|e| e.to_string())?
        );
        let bytes = tokio::time::timeout(
            REQUEST_TIMEOUT,
            session
                .spclient()
                .request(&http::Method::GET, &endpoint, None, None),
        )
        .await
        .map_err(|_| "Playlist request timed out")?
        .map_err(|e| e.to_string())?;
        let page = SelectedListContent::parse_from_bytes(&bytes).map_err(|e| e.to_string())?;
        let current = revision(page.revision());
        if snapshot.as_ref().is_some_and(|s| s != &current) {
            return Err("Playlist changed during pagination; retry on next update".to_owned());
        }
        snapshot = Some(current);
        let Some(content) = playlist_contents(&page, offset, confirmed_empty)? else {
            finished = true;
            break;
        };
        items.extend(content.items.iter().cloned());
        match next_offset(content, page.length(), offset)? {
            Some(next) => offset = next,
            None => {
                finished = true;
                break;
            }
        }
    }
    if !finished {
        return Err("Playlist exceeded its page limit".to_owned());
    }
    let cached_tracks: HashMap<_, _> = cached
        .and_then(|p| p.tracks.as_ref())
        .into_iter()
        .flatten()
        .map(|p| (p.uri(), p.clone()))
        .collect();
    // Personalized root-list revisions can differ from actual contents revisions.
    if let Some(cached) = cached
        && snapshot.as_ref() == Some(&cached.snapshot_id)
        && let Some(tracks) = &cached.tracks
        && tracks.len() == items.len()
        && tracks.iter().zip(&items).all(|(p, i)| p.uri() == i.uri())
    {
        return Ok((tracks.clone(), cached.snapshot_id.clone()));
    }
    let tracks = stream::iter(items.into_iter().enumerate())
        .map(|(index, item)| {
            let cached = cached_tracks.get(item.uri()).cloned();
            async move {
                if let Some(mut track) = cached {
                    track.set_list_index(index);
                    track.set_added_at(
                        item.attributes
                            .as_ref()
                            .and_then(|a| a.timestamp)
                            .and_then(chrono::DateTime::from_timestamp_millis),
                    );
                    return Ok(track);
                }
                let uri = SpotifyUri::from_uri(item.uri()).map_err(|e| e.to_string())?;
                let audio =
                    tokio::time::timeout(REQUEST_TIMEOUT, AudioItem::get_file(session, uri))
                        .await
                        .map_err(|_| "Track metadata timed out")?
                        .map_err(|e| e.to_string())?;
                let mut track =
                    Track::from_audio_item(audio).ok_or("Unsupported item in saved mix")?;
                track.list_index = index;
                track.added_at = item
                    .attributes
                    .as_ref()
                    .and_then(|a| a.timestamp)
                    .and_then(chrono::DateTime::from_timestamp_millis);
                Ok::<_, String>(Playable::Track(track))
            }
        })
        .buffered(4)
        .try_collect()
        .await?;
    Ok((tracks, snapshot.unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use librespot_core::{cache::Cache, config::SessionConfig};
    use librespot_metadata::{Metadata, playlist::Playlist as SessionPlaylist};
    use librespot_protocol::playlist4_external::{Item, ListAttributes, MetaItem};

    fn entry(owner: &str, format: &str) -> (Item, MetaItem) {
        let mut item = Item::new();
        item.set_uri(format!("spotify:playlist:{}", "1".repeat(22)));
        let mut attrs = ListAttributes::new();
        attrs.set_name("Test mix".to_owned());
        attrs.set_format(format.to_owned());
        let mut meta = MetaItem::new();
        meta.set_owner_username(owner.to_owned());
        meta.set_length(50);
        meta.set_revision(vec![1, 2, 3]);
        meta.attributes = protobuf::MessageField::some(attrs);
        (item, meta)
    }

    #[test]
    fn saved_list_includes_spotify_mixes_and_blend_but_not_other_users_playlists() {
        let mut page = ListItems::new();
        for (owner, format) in [
            ("spotify", "daily-mix"),
            ("friend", "blend"),
            ("friend", ""),
        ] {
            let (item, meta) = entry(owner, format);
            page.items.push(item);
            page.meta_items.push(meta);
        }
        let mixes = mixes_from_page(&page).unwrap();
        assert_eq!(mixes.len(), 2);
        assert!(
            mixes
                .iter()
                .all(|p| p.session_playlist && !p.collaborative && p.tracks.is_none())
        );
        assert_eq!(mixes[0].snapshot_id, "010203");
        assert_eq!(mixes[0].num_tracks, 50);
    }

    #[test]
    fn missing_metadata_is_an_error_not_an_empty_library() {
        let mut page = ListItems::new();
        page.items.push(entry("spotify", "blend").0);
        assert!(mixes_from_page(&page).is_err());
    }

    #[test]
    fn pagination_rejects_missing_and_repeated_pages() {
        let mut page = ListItems::new();
        page.set_pos(0);
        page.set_truncated(false);
        assert_eq!(next_offset(&page, 0, 0).unwrap(), None);
        assert!(next_offset(&page, 2, 0).is_err());
        page.items.push(entry("spotify", "blend").0);
        page.set_truncated(true);
        assert_eq!(next_offset(&page, 2, 0).unwrap(), Some(1));
        assert!(next_offset(&page, 2, 1).is_err());
        page.set_pos(1);
        page.set_truncated(false);
        assert_eq!(next_offset(&page, 2, 1).unwrap(), None);
    }

    #[test]
    fn explicitly_empty_playlist_is_distinct_from_missing_contents() {
        let mut page = SelectedListContent::new();
        assert!(playlist_contents(&page, 0, false).is_err());
        page.set_length(0);
        assert!(playlist_contents(&page, 0, false).unwrap().is_none());
        page.set_length(10);
        assert!(playlist_contents(&page, 0, false).is_err());
    }

    #[test]
    fn metadata_only_empty_response_requires_confirmation_from_saved_list() {
        let mut page = SelectedListContent::new();
        page.set_revision(vec![1, 2, 3]);
        page.attributes = protobuf::MessageField::some(ListAttributes::new());
        assert!(playlist_contents(&page, 0, false).is_err());
        assert!(playlist_contents(&page, 0, true).unwrap().is_none());
    }

    #[test]
    fn denied_playlist_metadata_does_not_prune_cache() {
        let (item, mut meta) = entry("spotify", "blend");
        meta.set_status_code(403);
        let mut page = ListItems::new();
        page.items.push(item);
        page.meta_items.push(meta);
        assert!(mixes_from_page(&page).is_err());
    }

    #[test]
    #[ignore = "requires cached playback credentials and network; no audio is started"]
    fn live_saved_mixes_and_blend() {
        let cache = Cache::new(
            Some(crate::config::cache_path("librespot")),
            None,
            None,
            None,
        )
        .unwrap();
        let credentials = cache.credentials().expect("cached playback credentials");
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
            let mixes = saved_mixes(&session).await.unwrap();
            assert!(
                !mixes.is_empty(),
                "save a mix or Blend in Spotify before running this probe"
            );
            let mut total = 0;
            for mix in &mixes {
                let start = std::time::Instant::now();
                let (tracks, revision) = super::tracks(&session, mix, None)
                    .await
                    .expect("complete playlist contents");
                // Root-list counts can lag personalized contents, just like revisions.
                assert!(!revision.is_empty());
                let uri = SpotifyUri::from_uri(&format!("spotify:playlist:{}", mix.id)).unwrap();
                let expected =
                    tokio::time::timeout(REQUEST_TIMEOUT, SessionPlaylist::get(&session, &uri))
                        .await
                        .unwrap()
                        .unwrap();
                let expected_uris: Vec<_> =
                    expected.tracks().map(|u| u.to_uri().unwrap()).collect();
                assert_eq!(
                    tracks.iter().map(Playable::uri).collect::<Vec<_>>(),
                    expected_uris
                );
                assert!(tracks.iter().all(|p| !p.is_suggested()));
                println!(
                    "Verified format={:?}: {} ordered tracks with metadata in {:.2}s",
                    expected.attributes.format,
                    tracks.len(),
                    start.elapsed().as_secs_f64()
                );
                let mut cached = mix.clone();
                cached.snapshot_id = revision.clone();
                cached.tracks = Some(tracks.clone());
                let cache_start = std::time::Instant::now();
                let (reloaded, cached_revision) =
                    super::tracks(&session, mix, Some(&cached)).await.unwrap();
                // Personalized playlists can change revision without changing tracks.
                assert!(!cached_revision.is_empty());
                assert_eq!(
                    reloaded.iter().map(Playable::uri).collect::<Vec<_>>(),
                    tracks.iter().map(Playable::uri).collect::<Vec<_>>()
                );
                println!(
                    "Cached refresh: {:.2}s",
                    cache_start.elapsed().as_secs_f64()
                );
                total += tracks.len();
            }
            println!(
                "Verified {} saved mixes / Blend playlists, {total} tracks; no player started",
                mixes.len()
            );
            session.shutdown();
        });
    }
}
