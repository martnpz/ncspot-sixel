//! Left "browser" pane: its title is a dropdown menu that switches between
//! sections (Search, Playlists, Albums, Artists, Recommendations). Items
//! opened here load into the "tracks" pane via the pane layout's view
//! interception.

use std::sync::{Arc, RwLock};

use cursive::event::{Event, EventResult, EventTrigger, Key, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType};
use cursive::view::Position;
use cursive::views::OnEventView;
use cursive::{Cursive, Printer, Vec2, View};
use unicode_width::UnicodeWidthStr;
use log::error;

use crate::command::{Command, TargetMode};
use crate::commands::CommandResult;
use crate::config::{Config, IconKind};
use crate::events::EventManager;
use crate::library::Library;
use crate::model::playable::Playable;
use crate::queue::Queue;
use crate::traits::{ListItem, ViewExt};
use crate::ui::listview::ListView;
use crate::ui::search_results::SearchResultsView;
use crate::ui::tracklist::TrackListView;

#[cfg(feature = "cover")]
use crate::ui::cover::sixel::SixelImageCache;

/// Thin wrapper that draws a rounded border (╭╮╰╯) around an inner view.
pub(crate) struct DropdownBorder<V: View> {
    inner: V,
}

impl<V: View> DropdownBorder<V> {
    pub(crate) fn new(inner: V) -> Self {
        Self { inner }
    }
}

impl<V: View> View for DropdownBorder<V> {
    fn draw(&self, printer: &Printer) {
        let w = printer.size.x;
        let h = printer.size.y;
        if w < 2 || h < 2 {
            self.inner.draw(printer);
            return;
        }
        let border_color = ColorStyle::new(
            ColorType::Color(*printer.theme.palette.custom("dropdown_border").unwrap()),
            ColorType::InheritParent,
        );
        printer.with_color(border_color, |p| {
            p.print((0, 0), &format!("╭{}╮", "─".repeat(w.saturating_sub(2))));
            for y in 1..h.saturating_sub(1) {
                p.print((0, y), "│");
                p.print((w.saturating_sub(1), y), "│");
            }
            p.print((0, h.saturating_sub(1)), &format!("╰{}╯", "─".repeat(w.saturating_sub(2))));
        });
        self.inner.draw(
            &printer
                .offset((1, 1))
                .cropped((w.saturating_sub(2), h.saturating_sub(2))),
        );
    }

    fn layout(&mut self, size: Vec2) {
        self.inner.layout(size.saturating_sub((2, 2)));
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        self.inner.required_size(constraint.saturating_sub((2, 2))) + (2, 2)
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        // Shift mouse event offset inward by the border thickness so that
        // the inner view computes local coordinates from its own top-left,
        // not from the border's top-left.
        let event = match event {
            Event::Mouse { offset, position, event: mouse_event } => Event::Mouse {
                offset: offset + Vec2::new(1, 1),
                position,
                event: mouse_event,
            },
            other => other,
        };
        self.inner.on_event(event)
    }

    fn needs_relayout(&self) -> bool {
        self.inner.needs_relayout()
    }

    fn call_on_any(&mut self, sel: &cursive::view::Selector, cb: cursive::event::AnyCb) {
        self.inner.call_on_any(sel, cb);
    }
}

/// Generic drop-down list that renders every row with a concrete palette
/// background (opaque over sixel graphics).
/// Focused row uses `highlight()`, all others use `highlight_inactive()`.
pub(crate) struct PaneDropdownMenu<T: Copy + Send + Sync + 'static> {
    items: Vec<(String, T)>,
    focus: usize,
    on_submit: Arc<dyn Fn(&mut Cursive, T) + Send + Sync>,
}

impl<T: Copy + Send + Sync + 'static> PaneDropdownMenu<T> {
    pub(crate) fn new<F>(items: Vec<(String, T)>, on_submit: F) -> Self
    where
        F: Fn(&mut Cursive, T) + Send + Sync + 'static,
    {
        Self { items, focus: 0, on_submit: Arc::new(on_submit) }
    }

    pub(crate) fn with_focus(mut self, focus: usize) -> Self {
        self.focus = focus.min(self.items.len().saturating_sub(1));
        self
    }
}

impl<T: Copy + Send + Sync + 'static> View for PaneDropdownMenu<T> {
    fn draw(&self, printer: &Printer) {
        let w = printer.size.x;
        for (i, (label, _)) in self.items.iter().enumerate() {
            let style = if i == self.focus {
                ColorStyle::new(
                    ColorType::Color(*printer.theme.palette.custom("dropdown_focused_fg").unwrap()),
                    ColorType::Color(*printer.theme.palette.custom("dropdown_focused_bg").unwrap()),
                )
            } else {
                ColorStyle::new(
                    ColorType::Color(*printer.theme.palette.custom("dropdown_fg").unwrap()),
                    ColorType::Color(*printer.theme.palette.custom("dropdown_bg").unwrap()),
                )
            };
            printer.with_color(style, |p| {
                p.print_hline((0, i), w, " ");
                p.print((0, i), label);
            });
        }
    }

    fn required_size(&mut self, _: Vec2) -> Vec2 {
        let w = self.items.iter().map(|(s, _)| s.width()).max().unwrap_or(0);
        Vec2::new(w, self.items.len())
    }

    fn layout(&mut self, _: Vec2) {}

    fn on_event(&mut self, event: Event) -> EventResult {
        match event {
            Event::Key(Key::Up) => {
                if self.focus > 0 {
                    self.focus -= 1;
                    EventResult::Consumed(None)
                } else {
                    EventResult::Ignored
                }
            }
            Event::Key(Key::Down) => {
                if self.focus + 1 < self.items.len() {
                    self.focus += 1;
                    EventResult::Consumed(None)
                } else {
                    EventResult::Ignored
                }
            }
            Event::Key(Key::Enter) => {
                let cb = self.on_submit.clone();
                let item = self.items[self.focus].1;
                EventResult::with_cb(move |s| cb(s, item))
            }
            Event::Mouse {
                event: MouseEvent::Press(_),
                position,
                offset,
            } => {
                let row = position.y.saturating_sub(offset.y);
                if row < self.items.len() {
                    self.focus = row;
                    let cb = self.on_submit.clone();
                    let item = self.items[row].1;
                    EventResult::with_cb(move |s| cb(s, item))
                } else {
                    EventResult::Ignored
                }
            }
            _ => EventResult::Ignored,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserSection {
    Search,
    Playlists,
    Albums,
    Artists,
    Recommendations,
}

impl BrowserSection {
    fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Playlists => "Playlists",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
            Self::Recommendations => "Recommendations",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "search" => Some(Self::Search),
            "playlists" => Some(Self::Playlists),
            "albums" => Some(Self::Albums),
            "artists" => Some(Self::Artists),
            "recommendations" => Some(Self::Recommendations),
            _ => None,
        }
    }
}

const DEFAULT_SECTIONS: [BrowserSection; 5] = [
    BrowserSection::Search,
    BrowserSection::Playlists,
    BrowserSection::Albums,
    BrowserSection::Artists,
    BrowserSection::Recommendations,
];

pub struct BrowserView {
    queue: Arc<Queue>,
    library: Arc<Library>,
    events: EventManager,

    section: BrowserSection,
    sections: Vec<BrowserSection>,

    playlists_content: Arc<RwLock<Vec<crate::model::playlist::Playlist>>>,
    albums_content: Arc<RwLock<Vec<crate::model::album::Album>>>,
    artists_content: Arc<RwLock<Vec<crate::model::artist::Artist>>>,
    playlists: ListView<crate::model::playlist::Playlist>,
    albums: ListView<crate::model::album::Album>,
    artists: ListView<crate::model::artist::Artist>,

    /// Recommendation tracks, fetched lazily on first visit.
    recommendations: Arc<RwLock<Vec<Playable>>>,
    recommendations_view: TrackListView,
    recommendations_loaded: bool,

    /// In-pane search: query input plus categorized results.
    search_input: String,
    search_input_active: bool,
    search_results: Option<SearchResultsView>,

    /// Absolute screen offset and content width of the pane, captured during
    /// draw and used to anchor the dropdown menu below the centred title.
    last_offset: RwLock<Vec2>,
    last_content_w: RwLock<usize>,
    cfg: Arc<Config>,
}

impl BrowserView {
    pub fn new(
        queue: Arc<Queue>,
        library: Arc<Library>,
        events: EventManager,
        cfg: Arc<Config>,
        #[cfg(feature = "cover")] images: Option<Arc<SixelImageCache>>,
    ) -> Self {
        let browser_cfg = cfg.values().layout.as_ref().and_then(|l| l.browser.clone());
        let browser_cfg = browser_cfg.unwrap_or_default();

        let sections: Vec<BrowserSection> = browser_cfg
            .sections
            .as_ref()
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| {
                        let section = BrowserSection::parse(name);
                        if section.is_none() {
                            error!(r#"unknown browser section "{name}", skipping"#);
                        }
                        section
                    })
                    .collect()
            })
            .filter(|sections: &Vec<_>| !sections.is_empty())
            .unwrap_or_else(|| DEFAULT_SECTIONS.to_vec());

        let section = browser_cfg
            .default_section
            .as_deref()
            .and_then(BrowserSection::parse)
            .filter(|section| sections.contains(section))
            .unwrap_or_else(|| {
                *sections
                    .iter()
                    .find(|s| **s != BrowserSection::Search)
                    .unwrap_or(&sections[0])
            });

        // Section lists: live library collections, or sorted snapshots when
        // sort = "name" is configured.
        let sort_by_name = browser_cfg.sort.as_deref() == Some("name");
        let playlists_content = if sort_by_name {
            let mut copy = library.playlists.read().unwrap().clone();
            copy.sort_by_key(|p| p.name.to_lowercase());
            Arc::new(RwLock::new(copy))
        } else {
            library.playlists.clone()
        };
        let albums_content = if sort_by_name {
            let mut copy = library.albums.read().unwrap().clone();
            copy.sort_by_key(|a| a.title.to_lowercase());
            Arc::new(RwLock::new(copy))
        } else {
            library.albums.clone()
        };
        let artists_content = if sort_by_name {
            let mut copy = library.artists.read().unwrap().clone();
            copy.sort_by_key(|a| a.name.to_lowercase());
            Arc::new(RwLock::new(copy))
        } else {
            library.artists.clone()
        };

        let recommendations: Arc<RwLock<Vec<Playable>>> = Arc::new(RwLock::new(Vec::new()));
        let recommendations_view = TrackListView::new(
            recommendations.clone(),
            queue.clone(),
            library.clone(),
            cfg.clone(),
            #[cfg(feature = "cover")]
            images.clone(),
        )
        .with_title("Recommendations");

        #[cfg(feature = "cover")]
        let playlists_list = ListView::new(playlists_content.clone(), queue.clone(), library.clone())
            .with_thumbnails(cfg.clone(), images.clone(), events.clone());
        #[cfg(not(feature = "cover"))]
        let playlists_list = ListView::new(playlists_content.clone(), queue.clone(), library.clone());

        #[cfg(feature = "cover")]
        let albums_list = ListView::new(albums_content.clone(), queue.clone(), library.clone())
            .with_thumbnails(cfg.clone(), images.clone(), events.clone());
        #[cfg(not(feature = "cover"))]
        let albums_list = ListView::new(albums_content.clone(), queue.clone(), library.clone());

        Self {
            playlists: playlists_list,
            albums: albums_list,
            artists: ListView::new(artists_content.clone(), queue.clone(), library.clone()),
            playlists_content,
            albums_content,
            artists_content,
            recommendations,
            recommendations_view,
            recommendations_loaded: false,
            search_input: String::new(),
            search_input_active: false,
            search_results: None,
            last_offset: RwLock::new(Vec2::zero()),
            last_content_w: RwLock::new(0),
            section,
            sections: sections.clone(),
            queue,
            library,
            events,
            cfg,
        }
    }

    pub fn set_section(&mut self, section: BrowserSection) {
        // All section list views share the same terminal area. Invalidate
        // their slot caches so the incoming section re-emits its sixels
        // from scratch instead of skipping because a stale position matches.
        #[cfg(feature = "cover")]
        {
            self.playlists.invalidate_slots();
            self.albums.invalidate_slots();
        }
        self.section = section;
        if section == BrowserSection::Search {
            self.search_input_active = true;
        }
        if section == BrowserSection::Recommendations && !self.recommendations_loaded {
            self.recommendations_loaded = true;
            self.load_recommendations();
        }
        self.events.trigger();
    }

    /// Fetch recommendations seeded by random saved tracks, off-thread.
    fn load_recommendations(&self) {
        let library = self.library.clone();
        let spotify = self.queue.get_spotify();
        let content = self.recommendations.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            use rand::seq::IteratorRandom;
            let seed_ids: Vec<String> = {
                let tracks = library.tracks.read().unwrap();
                let mut rng = rand::rng();
                tracks
                    .iter()
                    .filter_map(|t| t.id.clone())
                    .sample(&mut rng, 5)
            };
            if seed_ids.is_empty() {
                return;
            }
            let seeds: Vec<&str> = seed_ids.iter().map(String::as_str).collect();
            match spotify.api.recommendations(None, None, Some(seeds)) {
                Ok(recommendations) => {
                    let tracks: Vec<Playable> = recommendations
                        .tracks
                        .iter()
                        .map(|t| Playable::Track(crate::model::track::Track::from(t)))
                        .collect();
                    *content.write().unwrap() = tracks;
                    events.trigger();
                }
                Err(()) => error!("failed to fetch recommendations"),
            }
        });
    }

    fn submit_search(&mut self) {
        self.search_input_active = false;
        if self.search_input.is_empty() {
            return;
        }
        self.search_results = Some(SearchResultsView::new(
            self.search_input.clone(),
            self.events.clone(),
            self.queue.clone(),
            self.library.clone(),
        ));
    }

    /// The list view of the active section.
    fn section_view_mut(&mut self) -> &mut dyn ViewExt {
        match self.section {
            BrowserSection::Playlists => &mut self.playlists,
            BrowserSection::Albums => &mut self.albums,
            BrowserSection::Artists => &mut self.artists,
            BrowserSection::Recommendations => &mut self.recommendations_view,
            BrowserSection::Search => match &mut self.search_results {
                Some(results) => results,
                None => &mut self.playlists, // placeholder, drawn specially
            },
        }
    }

    /// Open the selected library item as a thumbnail track list (playlists
    /// and albums) or detail view (artists).
    fn open_selected(&mut self) -> Option<Box<dyn ViewExt>> {
        match self.section {
            BrowserSection::Playlists => {
                let playlist = {
                    let playlists = self.playlists_content.read().unwrap();
                    playlists.get(self.playlists.get_selected_index()).cloned()
                }?;
                playlist.open(self.queue.clone(), self.library.clone())
            }
            BrowserSection::Albums => {
                let album = {
                    let albums = self.albums_content.read().unwrap();
                    albums.get(self.albums.get_selected_index()).cloned()
                }?;
                album.open(self.queue.clone(), self.library.clone())
            }
            BrowserSection::Artists => {
                let artist = {
                    let artists = self.artists_content.read().unwrap();
                    artists.get(self.artists.get_selected_index()).cloned()
                }?;
                artist.open(self.queue.clone(), self.library.clone())
            }
            _ => None,
        }
    }

    fn open_dropdown(&self) -> EventResult {
        let offset = *self.last_offset.read().unwrap();
        let content_w = *self.last_content_w.read().unwrap();
        let current = self.section;
        let sections = self.sections.clone();
        let cfg = self.cfg.clone();

        // Mirror how draw_island centres the pane title so the dropdown
        // opens directly below the title text.
        //
        // draw_island uses inner_w = pane_rect.width() - 2.  With padding=0
        // the pane rect width is content_w + 2 (border), so inner_w = content_w.
        // The padded title is " <label> ▾ " where " ▾" is the PaneMenu icon
        // (2 display columns including its leading space).  That gives
        // title_padded_w = 1 + label.width() + 2 + 1 = label.width() + 4.
        // The same centering formula:
        //   left_fill = (inner_w - title_padded_w) / 2
        //   title_abs_x = island_tl.x + 1 + left_fill
        //               = (offset.x - 1) + 1 + left_fill
        //               = offset.x + left_fill
        //
        // The dropdown (including its own border) opens one row below the
        // island border row, which is at offset.y.
        let title_padded_w = current.label().width() + 4;
        let left_fill = content_w.saturating_sub(title_padded_w) / 2;
        let anchor = Vec2::new(offset.x + left_fill, offset.y);

        EventResult::with_cb(move |s: &mut Cursive| {
            let items: Vec<(String, BrowserSection)> = sections
                .iter()
                .map(|&sec| {
                    let icon = cfg.icon(match sec {
                        BrowserSection::Search => IconKind::MenuSearch,
                        BrowserSection::Playlists => IconKind::MenuPlaylists,
                        BrowserSection::Albums => IconKind::MenuAlbums,
                        BrowserSection::Artists => IconKind::MenuArtists,
                        BrowserSection::Recommendations => IconKind::MenuRecommendations,
                    });
                    (format!(" {icon}{} ", sec.label()), sec)
                })
                .collect();
            let focus = sections.iter().position(|&sec| sec == current).unwrap_or(0);
            let menu_view = PaneDropdownMenu::new(items, |s, section| {
                s.pop_layer();
                s.call_on_name("browser", move |browser: &mut Self| {
                    browser.set_section(section);
                });
            })
            .with_focus(focus);

            let menu = OnEventView::new(DropdownBorder::new(menu_view))
                .on_event(Key::Esc, |s| { s.pop_layer(); })
                .on_event(Event::Char('q'), |s| { s.pop_layer(); })
                .on_event(
                    EventTrigger::from_fn(|e| {
                        matches!(e, Event::Mouse { event: MouseEvent::Press(_), .. })
                    }),
                    |s| { s.pop_layer(); },
                );
            s.screen_mut().add_layer_at(Position::absolute(anchor), menu);
        })
    }
}

impl View for BrowserView {
    fn draw(&self, printer: &Printer) {
        *self.last_offset.write().unwrap() = printer.offset;
        *self.last_content_w.write().unwrap() = printer.size.x;

        match self.section {
            BrowserSection::Playlists => self.playlists.draw(printer),
            BrowserSection::Albums => self.albums.draw(printer),
            BrowserSection::Artists => self.artists.draw(printer),
            BrowserSection::Recommendations => self.recommendations_view.draw(printer),
            BrowserSection::Search => {
                // Input row on top, results below.
                let style = if self.search_input_active {
                    ColorStyle::highlight()
                } else {
                    ColorStyle::secondary()
                };
                printer.with_color(style, |printer| {
                    printer.print_hline((0, 0), printer.size.x, " ");
                    let text = if self.search_input.is_empty() && !self.search_input_active {
                        "Search…".to_string()
                    } else {
                        format!(
                            "{}{}",
                            self.search_input,
                            if self.search_input_active { "▏" } else { "" }
                        )
                    };
                    printer.print((0, 0), &text);
                });

                if let Some(results) = &self.search_results {
                    let printer = printer
                        .offset((0, 1))
                        .cropped((printer.size.x, printer.size.y.saturating_sub(1)));
                    results.draw(&printer);
                }
            }
        }
    }

    fn layout(&mut self, size: Vec2) {
        match self.section {
            BrowserSection::Playlists => self.playlists.layout(size),
            BrowserSection::Albums => self.albums.layout(size),
            BrowserSection::Artists => self.artists.layout(size),
            BrowserSection::Recommendations => self.recommendations_view.layout(size),
            BrowserSection::Search => {
                if let Some(results) = &mut self.search_results {
                    results.layout(Vec2::new(size.x, size.y.saturating_sub(1)));
                }
            }
        }
    }

    fn needs_relayout(&self) -> bool {
        true
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if self.section == BrowserSection::Search {
            if self.search_input_active {
                match event {
                    Event::Char(c) => {
                        self.search_input.push(c);
                        return EventResult::consumed();
                    }
                    Event::Key(Key::Backspace) => {
                        self.search_input.pop();
                        return EventResult::consumed();
                    }
                    Event::Key(Key::Enter) => {
                        self.submit_search();
                        return EventResult::consumed();
                    }
                    Event::Key(Key::Esc) => {
                        self.search_input_active = false;
                        return EventResult::consumed();
                    }
                    _ => return EventResult::Ignored,
                }
            }
            match event {
                Event::Char('/') => {
                    self.search_input_active = true;
                    return EventResult::consumed();
                }
                Event::Mouse {
                    event: MouseEvent::Press(MouseButton::Left),
                    position,
                    offset,
                } if position.saturating_sub(offset).y == 0 => {
                    self.search_input_active = true;
                    return EventResult::consumed();
                }
                Event::Mouse { .. } => {
                    if let Some(results) = &mut self.search_results {
                        return results.on_event(event.relativized((0, 1)));
                    }
                    return EventResult::Ignored;
                }
                _ => {}
            }
            if let Some(results) = &mut self.search_results {
                return results.on_event(event);
            }
            return EventResult::Ignored;
        }

        self.section_view_mut().on_event(event)
    }
}

impl ViewExt for BrowserView {
    fn title(&self) -> String {
        self.section.label().to_string()
    }

    fn on_resume(&self) {
        // A view that was pushed on top of us was popped: the terminal area
        // was overwritten while it was showing. Invalidate slot state so the
        // next draw re-emits all sixels instead of deduplication-skipping them.
        #[cfg(feature = "cover")]
        {
            self.playlists.invalidate_slots();
            self.albums.invalidate_slots();
        }
    }

    fn has_title_action(&self) -> bool {
        true
    }

    fn on_title_action(&mut self) -> EventResult {
        self.open_dropdown()
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        // Shift+O opens the section dropdown from the keyboard.
        if matches!(cmd, Command::Open(TargetMode::Current)) {
            if let EventResult::Consumed(Some(callback)) = self.open_dropdown() {
                callback(s);
            }
            return Ok(CommandResult::Consumed(None));
        }

        if matches!(cmd, Command::Open(TargetMode::Selected))
            && let Some(view) = self.open_selected()
        {
            return Ok(CommandResult::View(view));
        }

        self.section_view_mut().on_command(s, cmd)
    }
}
