use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard};
use std::{env, fs, process};

use cursive::theme::Theme;
use log::{debug, error};
use ncspot::{CONFIGURATION_FILE_NAME, USER_STATE_FILE_NAME};

use crate::command::{SortDirection, SortKey};
use crate::model::playable::Playable;
use crate::queue;
use crate::serialization::{CBOR, Serializer, TOML};

pub const CACHE_VERSION: u16 = 1;
pub const DEFAULT_COMMAND_KEY: char = ':';

/// The playback state when ncspot is started.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Default,
}

/// The focussed library tab when ncspot is started.
#[derive(Clone, Serialize, Deserialize, Debug, Hash, strum_macros::EnumIter)]
#[serde(rename_all = "lowercase")]
pub enum LibraryTab {
    Tracks,
    Albums,
    Artists,
    Playlists,
    Podcasts,
    Browse,
}

/// The format used to represent tracks in a list.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct TrackFormat {
    pub left: Option<String>,
    pub center: Option<String>,
    pub right: Option<String>,
}

impl TrackFormat {
    pub fn default() -> Self {
        Self {
            left: Some(String::from("%artists - %title")),
            center: Some(String::from("%album")),
            right: Some(String::from("%saved %duration")),
        }
    }
}

/// The format used when sending desktop notifications about playback status.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct NotificationFormat {
    pub title: Option<String>,
    pub body: Option<String>,
}

impl NotificationFormat {
    pub fn default() -> Self {
        Self {
            title: Some(String::from("%title")),
            body: Some(String::from("%artists")),
        }
    }
}

/// The configuration of ncspot.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct ConfigValues {
    pub command_key: Option<char>,
    pub initial_screen: Option<String>,
    pub default_keybindings: Option<bool>,
    pub keybindings: Option<HashMap<String, String>>,
    pub theme: Option<ConfigTheme>,
    pub use_nerdfont: Option<bool>,
    pub flip_status_indicators: Option<bool>,
    pub audio_cache: Option<bool>,
    pub audio_cache_size: Option<u32>,
    pub backend: Option<String>,
    pub backend_device: Option<String>,
    /// Requested PipeWire quantum in frames at 44100 Hz; lower values reduce output latency.
    pub pipewire_quantum: Option<u32>,
    pub volnorm: Option<bool>,
    pub volnorm_pregain: Option<f64>,
    pub notify: Option<bool>,
    pub bitrate: Option<u32>,
    pub gapless: Option<bool>,
    pub shuffle: Option<bool>,
    pub repeat: Option<queue::RepeatSetting>,
    pub cover_max_scale: Option<f32>,
    /// Deprecated: covers are always rendered with sixel. Kept so existing
    /// configs that still set it continue to parse; the value is ignored.
    pub cover_backend: Option<String>,
    pub playback_state: Option<PlaybackState>,
    pub track_format: Option<TrackFormat>,
    pub notification_format: Option<NotificationFormat>,
    pub statusbar_format: Option<String>,
    /// Format of the terminal window title. Supports the same placeholders as
    /// `statusbar_format` (e.g. `%title`, `%artists`). Defaults to
    /// `"ncspot - %title"`. Set to an empty string to leave the title untouched.
    pub title_format: Option<String>,
    pub library_tabs: Option<Vec<LibraryTab>>,
    pub hide_display_names: Option<bool>,
    pub ap_port: Option<u16>,
    pub device_name: Option<String>,
    pub layout: Option<LayoutConfig>,
    pub statusbar: Option<StatusbarConfig>,
    pub icons: Option<IconsConfig>,
    pub lyrics: Option<LyricsConfig>,
}

/// Configuration for the bottom player bar, set in the `[statusbar]` table.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct StatusbarConfig {
    /// Rows occupied by the progress bar. 1 = one line (default), 2-3 = taller.
    pub height: Option<u8>,
    /// Progress bar character style: `"square"` (█/░, default), `"thin"` (━/┉),
    /// or `"rounded"` (█/░ with nerd-font half-circle end caps).
    pub style: Option<String>,
    /// Left end cap for the `"rounded"` bar style.
    /// Default `""` (nf-ple-left_half_circle_thick).
    pub progress_cap_left: Option<String>,
    /// Right end cap for the `"rounded"` bar style.
    /// Default `""` (nf-ple-right_half_circle_thick).
    pub progress_cap_right: Option<String>,
    /// Controls row button style: `"square"` (default, `button_open`/`button_close`
    /// delimiters) or `"rounded"` (nerd-font half-circle pill around each icon).
    pub controls_style: Option<String>,
    /// Opening delimiter for control buttons (`"square"` style). Default `"[ "`.
    pub button_open: Option<String>,
    /// Closing delimiter for control buttons (`"square"` style). Default `" ]"`.
    pub button_close: Option<String>,
    /// Left pill cap for the `"rounded"` controls style.
    /// Default `""` (nf-ple-left_half_circle_thick).
    pub controls_cap_left: Option<String>,
    /// Right pill cap for the `"rounded"` controls style.
    /// Default `""` (nf-ple-right_half_circle_thick).
    pub controls_cap_right: Option<String>,
    /// Text (or icon) prepended to the title in the top border when the current
    /// track comes from Spotify recommendations. Set to `""` to disable.
    /// Default `"[N] "`.
    pub suggested_tag: Option<String>,
    /// Animate a bright left-to-right shimmer over a recommended track's title.
    /// Default `true`.
    pub suggested_shimmer: Option<bool>,
}

/// Configuration of the lyrics view, set in the `[lyrics]` table.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct LyricsConfig {
    /// Horizontal position of the lyrics text.
    pub align: Option<LyricsAlign>,
}

#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LyricsAlign {
    Left,
    #[default]
    Center,
    Right,
}

/// User overrides for the icons used across the UI, set in the `[icons]`
/// table. Unset icons fall back to nerd-font or ASCII defaults depending on
/// `use_nerdfont`.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct IconsConfig {
    pub playing: Option<String>,
    pub paused: Option<String>,
    pub stopped: Option<String>,
    pub repeat: Option<String>,
    pub repeat_track: Option<String>,
    pub repeat_off: Option<String>,
    pub shuffle: Option<String>,
    pub smart_shuffle: Option<String>,
    pub shuffle_off: Option<String>,
    pub player_prev: Option<String>,
    pub player_next: Option<String>,
    pub saved: Option<String>,
    pub updating: Option<String>,
    pub lyrics_current: Option<String>,
    pub filter: Option<String>,
    pub pane_back: Option<String>,
    pub pane_menu: Option<String>,
    pub menu_search: Option<String>,
    pub menu_friends: Option<String>,
    pub menu_playlists: Option<String>,
    pub menu_mixes: Option<String>,
    pub menu_albums: Option<String>,
    pub menu_artists: Option<String>,
    pub menu_recommendations: Option<String>,
    pub menu_lyrics: Option<String>,
    pub menu_queue: Option<String>,
    pub menu_add_to_queue: Option<String>,
    pub menu_add_to_playlist: Option<String>,
    pub menu_remove_from_playlist: Option<String>,
    pub menu_remove_recommendation: Option<String>,
    pub menu_share: Option<String>,
}

/// The icons that can be customized through [IconsConfig].
#[derive(Clone, Copy, Debug)]
pub enum IconKind {
    Playing,
    Paused,
    Stopped,
    Repeat,
    RepeatTrack,
    RepeatOff,
    Shuffle,
    SmartShuffle,
    ShuffleOff,
    PlayerPrev,
    PlayerNext,
    Saved,
    Updating,
    /// Marker shown next to the currently sung lyrics line.
    LyricsCurrent,
    /// Icon shown at the start of the filter/search input row in the track list.
    Filter,
    /// Prefix shown in a pane title when a previous view can be popped (back navigation).
    PaneBack,
    /// Suffix shown in a pane title when the view has a dropdown menu.
    PaneMenu,
    // Drop-menu item icons — one per selectable entry.
    MenuSearch,
    MenuFriends,
    MenuPlaylists,
    MenuMixes,
    MenuAlbums,
    MenuArtists,
    MenuRecommendations,
    MenuLyrics,
    MenuQueue,
    MenuAddToQueue,
    MenuAddToPlaylist,
    MenuRemoveFromPlaylist,
    MenuRemoveRecommendation,
    MenuShare,
}

impl IconKind {
    /// (nerd-font default, ASCII default)
    fn defaults(self) -> (&'static str, &'static str) {
        match self {
            Self::Playing => ("\u{f04b} ", "▶ "),
            Self::Paused => ("\u{f04c} ", "▮▮"),
            Self::Stopped => ("\u{f04d} ", "◼ "),
            Self::Repeat => ("\u{f0456} ", "R"),
            Self::RepeatTrack => ("\u{f0458} ", "R"),
            Self::RepeatOff => ("X", "X"),
            Self::Shuffle => ("\u{f049d} ", "S"),
            Self::SmartShuffle => ("\u{f0ae2} ", "S~"),
            Self::ShuffleOff => ("X", "X"),
            Self::PlayerPrev => ("\u{f04ae} ", "|<"),
            Self::PlayerNext => ("\u{f04ad} ", ">|"),
            Self::Saved => ("\u{f012c}", "✓"),
            Self::Updating => ("\u{f04e6} ", "[U] "),
            Self::LyricsCurrent => ("\u{f075a} ", "♪ "),
            Self::Filter => ("\u{f002} ", "/ "),
            Self::PaneBack => ("\u{f0141} ", "< "),
            Self::PaneMenu => (" \u{f0140}", " ▾"),
            Self::MenuSearch => ("\u{f002} ", "/ "),
            Self::MenuFriends => ("\u{f0c0} ", "& "),
            Self::MenuPlaylists => ("\u{f03a} ", "= "),
            Self::MenuMixes => ("\u{f1bc} ", "≈ "),
            Self::MenuAlbums => ("\u{f001} ", "♫ "),
            Self::MenuArtists => ("\u{f007} ", "@ "),
            Self::MenuRecommendations => ("\u{f005} ", "* "),
            Self::MenuLyrics => ("\u{f075a} ", "♪ "),
            Self::MenuQueue => ("\u{f03a} ", "= "),
            Self::MenuAddToQueue => ("\u{f04b} ", "> "),
            Self::MenuAddToPlaylist => ("\u{f0fe} ", "+ "),
            Self::MenuRemoveFromPlaylist => ("\u{f146} ", "- "),
            Self::MenuRemoveRecommendation => ("\u{f0a76} ", "✗ "),
            Self::MenuShare => ("\u{f064} ", "↗ "),
        }
    }
}

/// Configuration of the multi-pane "panes" screen.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct LayoutConfig {
    pub columns: Vec<ColumnConfig>,
    /// Options for the "browser" pane.
    pub browser: Option<BrowserConfig>,
    /// Options for the "tracks" pane.
    pub tracks: Option<TracksPaneConfig>,
    /// Options for the "info" pane.
    pub info: Option<InfoConfig>,
    /// Empty cells between the screen edge and between adjacent pane islands
    /// (sets both x and y). Default 1. Overridden by `gap_x` / `gap_y`.
    pub gap: Option<u8>,
    /// Horizontal gap only (between columns and left/right screen edges).
    pub gap_x: Option<u8>,
    /// Vertical gap only (between rows and top/bottom screen edges).
    pub gap_y: Option<u8>,
    /// Extra padding inside each island border on every side. Default 0.
    /// Overridden by `padding_x` / `padding_y`.
    pub padding: Option<u8>,
    /// Horizontal padding only (left/right inside the border).
    pub padding_x: Option<u8>,
    /// Vertical padding only (top/bottom inside the border).
    pub padding_y: Option<u8>,
}

/// Options for the browser pane (`[layout.browser]`).
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct BrowserConfig {
    /// Sections shown in the dropdown, in order. Valid: "search",
    /// "playlists", "albums", "artists", "recommendations".
    pub sections: Option<Vec<String>>,
    /// Section selected at startup.
    pub default_section: Option<String>,
    /// Sorting of section lists: "default" (Spotify order) or "name".
    pub sort: Option<String>,
    /// Height of each row in the playlist/album lists, in cells.
    /// 1 disables thumbnails. Default 2.
    pub row_height: Option<u8>,
    /// Show cover-art thumbnails in the playlist and album lists (requires
    /// sixel support). Default true.
    pub thumbnails: Option<bool>,
}

/// Options for the tracks pane (`[layout.tracks]`).
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct TracksPaneConfig {
    /// Height of a track row in cells; 1 disables thumbnails. Default 2.
    pub row_height: Option<u8>,
    /// Show album-art thumbnails (requires sixel). Default true.
    pub thumbnails: Option<bool>,
    /// Show the persistent filter input row. Default true.
    pub filter_row: Option<bool>,
    /// Default sort order applied when opening a playlist or album.
    /// Values: `"last_added"`, `"first_added"`, `"duration"`,
    /// `"alphabetical"`, `"reverse"` (reverse alphabetical).
    /// Omit to keep Spotify's original order. A manual `sort` command
    /// overrides this and is remembered per playlist.
    pub default_sort: Option<String>,
}

/// Options for the info pane (`[layout.info]`).
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct InfoConfig {
    /// Number of artist genres displayed. Default 2.
    pub genres: Option<u8>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ColumnConfig {
    /// Width of the column as a percentage of the screen width.
    pub width: u8,
    /// Panes stacked vertically in this column, top to bottom. Valid names:
    /// playlists, tracks, cover, lyrics, queue.
    pub panes: Vec<String>,
    /// Height of each pane as a percentage of the column height. Must match
    /// the length of `panes`; values need not sum to 100 (the last pane gets
    /// the remainder). Omit to distribute evenly.
    pub heights: Option<Vec<u8>>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            columns: vec![
                ColumnConfig {
                    width: 50,
                    panes: vec!["browser".into()],
                    heights: None,
                },
                ColumnConfig {
                    width: 50,
                    panes: vec!["info".into(), "lyrics".into()],
                    heights: Some(vec![30, 70]),
                },
            ],
            browser: None,
            tracks: None,
            info: None,
            gap: None,
            gap_x: None,
            gap_y: None,
            padding: None,
            padding_x: None,
            padding_y: None,
        }
    }
}

/// The ncspot theme.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ConfigTheme {
    pub background: Option<String>,
    pub primary: Option<String>,
    pub secondary: Option<String>,
    pub title: Option<String>,
    pub playing: Option<String>,
    pub playing_selected: Option<String>,
    pub playing_bg: Option<String>,
    pub highlight: Option<String>,
    pub highlight_bg: Option<String>,
    pub highlight_inactive_bg: Option<String>,
    pub error: Option<String>,
    pub error_bg: Option<String>,
    pub statusbar_progress: Option<String>,
    pub statusbar_progress_bg: Option<String>,
    pub statusbar: Option<String>,
    pub statusbar_bg: Option<String>,
    /// Text color for the controls row (buttons + time/vol). Falls back to `statusbar`.
    pub statusbar_controls: Option<String>,
    /// Background color for the controls row. Falls back to `statusbar_bg`.
    pub statusbar_controls_bg: Option<String>,
    /// Icon color for an active/pressed control button — enabled shuffle/repeat,
    /// or the brief blink when play/prev/next is clicked. Default white.
    pub statusbar_controls_active: Option<String>,
    /// Pill background behind each icon in the `"rounded"` controls style.
    /// Default a gray (light black).
    pub statusbar_controls_button_bg: Option<String>,
    pub cmdline: Option<String>,
    pub cmdline_bg: Option<String>,
    pub search_match: Option<String>,
    pub dropdown_fg: Option<String>,
    pub dropdown_bg: Option<String>,
    pub dropdown_focused_fg: Option<String>,
    pub dropdown_focused_bg: Option<String>,
    pub dropdown_border: Option<String>,
}

/// The ordering that is used when representing a playlist.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SortingOrder {
    pub key: SortKey,
    pub direction: SortDirection,
}

/// The runtime state of the music queue.
#[derive(Serialize, Default, Deserialize, Debug, Clone)]
pub struct QueueState {
    pub current_track: Option<usize>,
    pub random_order: Option<Vec<usize>>,
    pub track_progress: std::time::Duration,
    pub queue: Vec<Playable>,
}

/// The last playlist or album the user opened, persisted across sessions.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LastOpenedItem {
    /// `"playlist"` or `"album"`
    pub kind: String,
    pub id: String,
}

/// Runtime state that should be persisted accross sessions.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UserState {
    pub volume: u16,
    pub shuffle: bool,
    /// Visual-only flag: when true the shuffle button shows the "smart shuffle" icon.
    /// Playback behaviour is identical to regular shuffle.
    #[serde(default)]
    pub smart_shuffle_visual: bool,
    pub repeat: queue::RepeatSetting,
    pub queuestate: QueueState,
    pub playlist_orders: HashMap<String, SortingOrder>,
    pub cache_version: u16,
    /// Unix epoch seconds of the last full library API sync, used to gate
    /// re-syncing on launch behind a freshness TTL.
    #[serde(default)]
    pub last_library_sync: Option<i64>,
    pub playback_state: PlaybackState,
    /// Last opened playlist or album; re-fetched on startup.
    #[serde(default)]
    pub last_opened: Option<LastOpenedItem>,
}

impl Default for UserState {
    fn default() -> Self {
        Self {
            volume: u16::MAX,
            shuffle: false,
            smart_shuffle_visual: false,
            repeat: queue::RepeatSetting::None,
            queuestate: QueueState::default(),
            playlist_orders: HashMap::new(),
            cache_version: 0,
            last_library_sync: None,
            playback_state: PlaybackState::Default,
            last_opened: None,
        }
    }
}

/// Configuration files are read/written relative to this directory.
static BASE_PATH: RwLock<Option<PathBuf>> = RwLock::new(None);

/// The complete configuration (state + user configuration) of ncspot.
pub struct Config {
    /// The configuration file path.
    filename: String,
    /// Configuration set by the user, read only.
    values: RwLock<ConfigValues>,
    /// Runtime state which can't be edited by the user, read/write.
    state: RwLock<UserState>,
}

impl Config {
    /// Create a default configuration from in-memory defaults, without touching the filesystem.
    #[cfg(test)]
    pub fn new_for_test() -> std::sync::Arc<Self> {
        Self::new_for_test_with_values(ConfigValues::default())
    }

    #[cfg(test)]
    pub fn new_for_test_with_values(values: ConfigValues) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            filename: String::new(),
            values: RwLock::new(values),
            state: RwLock::new(UserState::default()),
        })
    }

    /// Generate the configuration from the user configuration file and the runtime state file.
    /// `filename` can be used to look for a differently named configuration file.
    pub fn new(filename: Option<String>) -> Self {
        let filename = filename.unwrap_or(CONFIGURATION_FILE_NAME.to_owned());
        let values = load(&filename).unwrap_or_else(|e| {
            eprint!(
                "There is an error in your configuration file at {}:\n\n{e}",
                user_configuration_directory()
                    .map(|ref mut path| {
                        path.push(CONFIGURATION_FILE_NAME);
                        path.to_string_lossy().to_string()
                    })
                    .expect("configuration directory expected but not found")
            );
            process::exit(1);
        });

        let mut userstate = {
            let path = config_path(USER_STATE_FILE_NAME);
            CBOR.load_or_generate_default(path, || Ok(UserState::default()), true)
                .expect("could not load user state")
        };

        if let Some(shuffle) = values.shuffle {
            userstate.shuffle = shuffle;
        }

        if let Some(repeat) = values.repeat {
            userstate.repeat = repeat;
        }

        if let Some(playback_state) = values.playback_state.clone() {
            userstate.playback_state = playback_state;
        }

        Self {
            filename,
            values: RwLock::new(values),
            state: RwLock::new(userstate),
        }
    }

    /// Get the user configuration values.
    pub fn values(&self) -> RwLockReadGuard<'_, ConfigValues> {
        self.values.read().unwrap()
    }

    /// Resolve an icon: user override from `[icons]`, then the nerd-font
    /// default if `use_nerdfont` is set, then the ASCII default.
    pub fn icon(&self, kind: IconKind) -> String {
        let values = self.values();
        let user_icon = values.icons.as_ref().and_then(|icons| {
            match kind {
                IconKind::Playing => &icons.playing,
                IconKind::Paused => &icons.paused,
                IconKind::Stopped => &icons.stopped,
                IconKind::Repeat => &icons.repeat,
                IconKind::RepeatTrack => &icons.repeat_track,
                IconKind::RepeatOff => &icons.repeat_off,
                IconKind::Shuffle => &icons.shuffle,
                IconKind::SmartShuffle => &icons.smart_shuffle,
                IconKind::ShuffleOff => &icons.shuffle_off,
                IconKind::PlayerPrev => &icons.player_prev,
                IconKind::PlayerNext => &icons.player_next,
                IconKind::Saved => &icons.saved,
                IconKind::Updating => &icons.updating,
                IconKind::LyricsCurrent => &icons.lyrics_current,
                IconKind::Filter => &icons.filter,
                IconKind::PaneBack => &icons.pane_back,
                IconKind::PaneMenu => &icons.pane_menu,
                IconKind::MenuSearch => &icons.menu_search,
                IconKind::MenuFriends => &icons.menu_friends,
                IconKind::MenuPlaylists => &icons.menu_playlists,
                IconKind::MenuMixes => &icons.menu_mixes,
                IconKind::MenuAlbums => &icons.menu_albums,
                IconKind::MenuArtists => &icons.menu_artists,
                IconKind::MenuRecommendations => &icons.menu_recommendations,
                IconKind::MenuLyrics => &icons.menu_lyrics,
                IconKind::MenuQueue => &icons.menu_queue,
                IconKind::MenuAddToQueue => &icons.menu_add_to_queue,
                IconKind::MenuAddToPlaylist => &icons.menu_add_to_playlist,
                IconKind::MenuRemoveFromPlaylist => &icons.menu_remove_from_playlist,
                IconKind::MenuRemoveRecommendation => &icons.menu_remove_recommendation,
                IconKind::MenuShare => &icons.menu_share,
            }
            .clone()
        });

        user_icon.unwrap_or_else(|| {
            let (nerdfont, ascii) = kind.defaults();
            if values.use_nerdfont.unwrap_or(false) {
                nerdfont.to_string()
            } else {
                ascii.to_string()
            }
        })
    }

    /// Get the runtime user state values.
    pub fn state(&self) -> RwLockReadGuard<'_, UserState> {
        self.state.read().unwrap()
    }

    /// Modify the internal user state through a shared reference using a closure.
    pub fn with_state_mut<F>(&self, cb: F)
    where
        F: Fn(&mut UserState),
    {
        let mut state_guard = self.state.write().unwrap();
        cb(&mut state_guard);
    }

    /// Update the version number of the runtime user state. This should be done before saving it to
    /// disk.
    fn update_state_cache_version(&self) {
        self.with_state_mut(|state| state.cache_version = CACHE_VERSION);
    }

    /// Save runtime state to the user configuration directory.
    pub fn save_state(&self) {
        self.update_state_cache_version();

        let path = config_path(USER_STATE_FILE_NAME);
        debug!("saving user state to {}", path.display());
        if let Err(e) = CBOR.write(path, &*self.state()) {
            error!("Could not save user state: {e}");
        }
    }

    /// Create a [Theme] from the user supplied theme in the configuration file.
    pub fn build_theme(&self) -> Theme {
        crate::theme::load(&self.values().theme)
    }

    /// Attempt to reload the configuration from the configuration file.
    ///
    /// This only updates the values stored in memory but doesn't perform any additional actions
    /// like updating active keybindings.
    pub fn reload(&self) -> Result<(), Box<dyn Error>> {
        let cfg = load(&self.filename)?;
        *self.values.write().unwrap() = cfg;
        Ok(())
    }
}

/// Parse the configuration file with name `filename` at the configuration base path.
fn load(filename: &str) -> Result<ConfigValues, String> {
    let path = config_path(filename);
    TOML.load_or_generate_default(path, || Ok(ConfigValues::default()), false)
}

/// The per-user ncspot directories. Only the config and cache directories are
/// needed; data/state dirs from the old cross-platform layout were unused.
pub struct ProjectDirs {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
}

/// Resolve `$<env_var>/ncspot`, falling back to `$HOME/<fallback>/ncspot`
/// (the XDG base-directory convention used on Linux).
fn xdg_dir(env_var: &str, fallback: &str) -> Option<PathBuf> {
    let base = env::var_os(env_var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(fallback)))?;
    Some(base.join("ncspot"))
}

/// Returns the ncspot config/cache directories if they could be determined,
/// or an error otherwise.
pub fn try_proj_dirs() -> Result<ProjectDirs, String> {
    match *BASE_PATH
        .read()
        .map_err(|_| String::from("Poisoned RWLock"))?
    {
        Some(ref basepath) => Ok(ProjectDirs {
            config_dir: basepath.join(".config"),
            cache_dir: basepath.join(".cache"),
        }),
        None => Ok(ProjectDirs {
            config_dir: xdg_dir("XDG_CONFIG_HOME", ".config")
                .ok_or_else(|| String::from("Couldn't determine config directory"))?,
            cache_dir: xdg_dir("XDG_CACHE_HOME", ".cache")
                .ok_or_else(|| String::from("Couldn't determine cache directory"))?,
        }),
    }
}

/// Return the path to the current user's configuration directory, or None if it couldn't be found.
/// This function does not guarantee correct permissions or ownership of the directory!
pub fn user_configuration_directory() -> Option<PathBuf> {
    let project_directories = try_proj_dirs().ok()?;
    Some(project_directories.config_dir)
}

/// Return the path to the current user's cache directory, or None if one couldn't be found. This
/// function does not guarantee correct permissions or ownership of the directory!
pub fn user_cache_directory() -> Option<PathBuf> {
    let project_directories = try_proj_dirs().ok()?;
    Some(project_directories.cache_dir)
}

/// Force create the configuration directory at the default project location, removing anything that
/// isn't a directory but has the same name. Return the path to the configuration file inside the
/// directory.
///
/// This doesn't create the file, only the containing directory.
pub fn config_path(file: &str) -> PathBuf {
    let cfg_dir = user_configuration_directory().unwrap();
    if cfg_dir.exists() && !cfg_dir.is_dir() {
        fs::remove_file(&cfg_dir).expect("unable to remove old config file");
    }
    if !cfg_dir.exists() {
        fs::create_dir_all(&cfg_dir).expect("can't create config folder");
    }
    let mut cfg = cfg_dir.to_path_buf();
    cfg.push(file);
    cfg
}

/// Create the cache directory at the default project location, preserving it if it already exists,
/// and return the path to the cache file inside the directory.
///
/// This doesn't create the file, only the containing directory.
pub fn cache_path(file: &str) -> PathBuf {
    let cache_dir = user_cache_directory().unwrap();
    if !cache_dir.exists() {
        fs::create_dir_all(&cache_dir).expect("can't create cache folder");
    }
    let mut pb = cache_dir.to_path_buf();
    pb.push(file);
    pb
}

/// Set the configuration base path. All configuration files are read/written relative to this path.
pub fn set_configuration_base_path(base_path: Option<PathBuf>) {
    if let Some(basepath) = base_path {
        if !basepath.exists() {
            fs::create_dir_all(&basepath).expect("could not create basepath directory");
        }
        *BASE_PATH.write().unwrap() = Some(basepath);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let example = include_str!("../config.toml.example");
        let values: ConfigValues =
            toml::from_str(example).expect("config.toml.example must parse into ConfigValues");

        let layout = values.layout.expect("example should define [layout]");
        assert_eq!(layout.columns.len(), 2);
        assert_eq!(layout.columns[0].panes, vec!["browser"]);
        assert_eq!(layout.columns[1].panes, vec!["info", "lyrics"]);
        let browser = layout.browser.expect("example should define [layout.browser]");
        assert_eq!(browser.default_section.as_deref(), Some("playlists"));
        assert_eq!(browser.sections.map(|s| s.len()), Some(7));
        let tracks = layout.tracks.expect("example should define [layout.tracks]");
        assert_eq!(tracks.row_height, Some(2));
        assert_eq!(layout.info.and_then(|i| i.genres), Some(2));
        assert_eq!(values.pipewire_quantum, Some(1024));
        assert_eq!(values.initial_screen.as_deref(), Some("panes"));
    }

    #[test]
    fn icon_resolution_precedence() {
        // ASCII defaults without nerdfont
        let cfg = Config::new_for_test();
        assert_eq!(cfg.icon(IconKind::Playing), "▶ ");
        assert_eq!(cfg.icon(IconKind::Saved), "✓");

        // Nerd-font defaults with use_nerdfont
        let cfg = Config::new_for_test_with_values(ConfigValues {
            use_nerdfont: Some(true),
            ..Default::default()
        });
        assert_eq!(cfg.icon(IconKind::Playing), "\u{f04b} ");
        assert_eq!(cfg.icon(IconKind::Saved), "\u{f012c}");

        // User overrides beat both defaults
        let cfg = Config::new_for_test_with_values(ConfigValues {
            use_nerdfont: Some(true),
            icons: Some(IconsConfig {
                playing: Some(">> ".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(cfg.icon(IconKind::Playing), ">> ");
        // Unset icons still fall back to the nerd-font default
        assert_eq!(cfg.icon(IconKind::Paused), "\u{f04c} ");
    }
}
