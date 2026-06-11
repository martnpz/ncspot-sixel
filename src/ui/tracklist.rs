//! Multi-line track list with album-art thumbnails, a persistent filter row
//! and configurable sorting.
//!
//! Rows are fixed-height "slots" (default 2 cells): a square sixel thumbnail
//! on the left and title/artist text beside it. Thumbnails are emitted
//! out-of-band of cursive's cell diffing, so each slot remembers what it last
//! emitted and only writes to the terminal when its image or position
//! changes; vacated slots are blanked.

use std::sync::{Arc, RwLock};

use cursive::event::{Event, EventResult, Key, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, Effect, PaletteColor, Style};
use cursive::{Cursive, Printer, Rect, Vec2, View};
use unicode_width::UnicodeWidthStr;

use crate::command::{Command, GotoMode, JumpMode, MoveAmount, MoveMode, TargetMode};
use crate::commands::CommandResult;
use crate::config::Config;
use crate::library::Library;
use crate::model::playable::{Playable, sort_playables};
use crate::queue::Queue;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::artist::ArtistView;
use crate::ui::contextmenu::ContextMenu;

#[cfg(feature = "cover")]
use crate::ui::cover::sixel::SixelImageCache;

/// What a thumbnail slot last emitted to the terminal.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
enum SlotImage {
    #[default]
    Empty,
    Blank,
    Image(String),
}

#[derive(Clone, Default)]
struct SlotState {
    image: SlotImage,
    position: Vec2,
}

pub struct TrackListView {
    content: Arc<RwLock<Vec<Playable>>>,
    queue: Arc<Queue>,
    library: Arc<Library>,
    cfg: Arc<Config>,
    #[cfg(feature = "cover")]
    images: Option<Arc<SixelImageCache>>,
    title: String,
    /// Identifier under which a chosen sort order is persisted (playlist id).
    sort_context: Option<String>,

    /// Live filter; matching is case-insensitive over title/artists/album.
    filter: String,
    filter_active: bool,
    /// Indices into `content` that match the filter.
    filtered: Vec<usize>,
    last_content_len: usize,

    /// Selection and scroll position, in `filtered` indices.
    selected: usize,
    scroll_top: usize,

    last_size: Vec2,
    /// Last emitted thumbnail per visible slot.
    slots: RwLock<Vec<SlotState>>,
}

impl TrackListView {
    pub fn new(
        content: Arc<RwLock<Vec<Playable>>>,
        queue: Arc<Queue>,
        library: Arc<Library>,
        cfg: Arc<Config>,
        #[cfg(feature = "cover")] images: Option<Arc<SixelImageCache>>,
    ) -> Self {
        let mut view = Self {
            content,
            queue,
            library,
            cfg,
            #[cfg(feature = "cover")]
            images,
            title: "Tracks".to_string(),
            sort_context: None,
            filter: String::new(),
            filter_active: false,
            filtered: Vec::new(),
            last_content_len: usize::MAX,
            selected: 0,
            scroll_top: 0,
            last_size: Vec2::zero(),
            slots: RwLock::new(Vec::new()),
        };
        view.refresh_filter();
        view
    }

    pub fn with_title(mut self, title: &str) -> Self {
        self.title = title.to_string();
        self
    }

    /// Persist `sort` commands for this list under the given id (and apply a
    /// previously saved order now).
    pub fn with_sort_context(mut self, id: &str) -> Self {
        self.sort_context = Some(id.to_string());
        let saved = self.cfg.state().playlist_orders.get(id).cloned();
        if let Some(order) = saved {
            sort_playables(&mut self.content.write().unwrap(), &order.key, &order.direction);
        }
        self
    }

    fn tracks_config<T>(&self, get: impl Fn(&crate::config::TracksPaneConfig) -> Option<T>) -> Option<T> {
        self.cfg
            .values()
            .layout
            .as_ref()
            .and_then(|layout| layout.tracks.as_ref())
            .and_then(get)
    }

    fn row_height(&self) -> usize {
        self.tracks_config(|t| t.row_height).unwrap_or(2).max(1) as usize
    }

    fn filter_row_enabled(&self) -> bool {
        self.tracks_config(|t| t.filter_row).unwrap_or(true)
    }

    /// Width in cells reserved for thumbnails, 0 when disabled/unavailable.
    fn thumb_columns(&self, cell_px: Vec2) -> usize {
        #[cfg(feature = "cover")]
        {
            let enabled = self.tracks_config(|t| t.thumbnails).unwrap_or(true);
            if enabled && self.images.is_some() && self.row_height() >= 2 {
                let side_px = self.row_height() * cell_px.y;
                return side_px.div_ceil(cell_px.x.max(1));
            }
        }
        let _ = cell_px;
        0
    }

    fn refresh_filter(&mut self) {
        let content = self.content.read().unwrap();
        let query = self.filter.to_lowercase();
        self.filtered = content
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                if query.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{} {} {}",
                    item.display_left(&self.library),
                    item.display_center(&self.library),
                    item.display_right(&self.library)
                )
                .to_lowercase();
                haystack.contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.last_content_len = content.len();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// Number of visible row slots for the current size.
    fn slot_count(&self, size: Vec2) -> usize {
        let header = usize::from(self.filter_row_enabled());
        size.y.saturating_sub(header) / self.row_height()
    }

    fn clamp_scroll(&mut self) {
        let slots = self.slot_count(self.last_size).max(1);
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else if self.selected >= self.scroll_top + slots {
            self.scroll_top = self.selected + 1 - slots;
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let last = self.filtered.len().saturating_sub(1);
        self.selected = self
            .selected
            .saturating_add_signed(delta as isize)
            .min(last);
        self.clamp_scroll();
    }

    fn selected_item(&self) -> Option<Playable> {
        let content = self.content.read().unwrap();
        self.filtered
            .get(self.selected)
            .and_then(|&index| content.get(index).cloned())
    }

    fn with_selected_item_mut<R>(&self, f: impl FnOnce(&mut Playable) -> R) -> Option<R> {
        let mut content = self.content.write().unwrap();
        self.filtered
            .get(self.selected)
            .and_then(|&index| content.get_mut(index))
            .map(f)
    }

    /// Emit/refresh the thumbnail for one slot if needed.
    #[cfg(feature = "cover")]
    fn draw_thumbnail(&self, slot: usize, desired: SlotImage, position: Vec2, side_px: usize) {
        let Some(images) = &self.images else { return };
        let mut slots = self.slots.write().unwrap();
        if slots.len() <= slot {
            slots.resize(slot + 1, SlotState::default());
        }
        let state = &mut slots[slot];
        if state.image == desired && state.position == position {
            return;
        }

        let sixel = match &desired {
            SlotImage::Image(url) => images.get(url, side_px, side_px),
            SlotImage::Blank | SlotImage::Empty => {
                // Only blank a slot that previously showed something.
                if state.image == SlotImage::Empty && state.position == position {
                    return;
                }
                images.blank(side_px, side_px)
            }
        };

        match sixel {
            Some(sixel) => {
                if crate::ui::cover::sixel::emit(&sixel, position).is_ok() {
                    state.image = desired;
                    state.position = position;
                }
            }
            None => {
                // Image still encoding: blank the slot if it shows a stale
                // image, so the user never sees the wrong cover.
                if matches!(state.image, SlotImage::Image(_))
                    && let Some(blank) = images.blank(side_px, side_px)
                    && crate::ui::cover::sixel::emit(&blank, position).is_ok()
                {
                    state.image = SlotImage::Blank;
                    state.position = position;
                }
            }
        }
    }
}

impl View for TrackListView {
    fn draw(&self, printer: &Printer) {
        let row_height = self.row_height();
        let filter_row = self.filter_row_enabled();
        let header = usize::from(filter_row);
        let slots = self.slot_count(printer.size);

        // Filter input row.
        if filter_row {
            let style = if self.filter_active {
                ColorStyle::highlight()
            } else {
                ColorStyle::secondary()
            };
            printer.with_color(style, |printer| {
                printer.print_hline((0, 0), printer.size.x, " ");
                let text = if self.filter.is_empty() && !self.filter_active {
                    "/ filter…".to_string()
                } else {
                    format!("/{}{}", self.filter, if self.filter_active { "▏" } else { "" })
                };
                printer.print((0, 0), &text);
                if !self.filter.is_empty() {
                    let count = format!("{}/{}", self.filtered.len(), self.last_content_len);
                    let x = printer.size.x.saturating_sub(count.width());
                    printer.print((x, 0), &count);
                }
            });
        }

        #[cfg(feature = "cover")]
        let cell_px =
            crate::ui::cover::cell_size_px(self.cfg.values().cover_max_scale.unwrap_or(1.0));
        #[cfg(not(feature = "cover"))]
        let cell_px = Vec2::new(8, 16);
        let thumb_cols = self.thumb_columns(cell_px);
        let side_px = row_height * cell_px.y;
        let text_x = if thumb_cols > 0 { thumb_cols + 1 } else { 0 };

        let content = self.content.read().unwrap();
        let playing_index = self.queue.get_current_index();

        for slot in 0..slots {
            let y0 = header + slot * row_height;
            let item = self
                .filtered
                .get(self.scroll_top + slot)
                .and_then(|&index| content.get(index).map(|item| (index, item)));

            let abs_position = printer.offset + Vec2::new(0, y0);

            let Some((content_index, item)) = item else {
                #[cfg(feature = "cover")]
                self.draw_thumbnail(slot, SlotImage::Empty, abs_position, side_px);
                continue;
            };

            let is_selected = self.scroll_top + slot == self.selected;
            let is_playing =
                item.is_playing(&self.queue) && playing_index == Some(content_index);

            // Background for the selected row (text area only; the thumbnail
            // area is image pixels anyway).
            if is_selected {
                printer.with_color(ColorStyle::highlight(), |printer| {
                    for line in 0..row_height {
                        printer.print_hline((text_x, y0 + line), printer.size.x - text_x, " ");
                    }
                });
            }

            let title_style = if is_playing {
                Style::from(ColorStyle::new(
                    ColorType::Color(*printer.theme.palette.custom("playing").unwrap()),
                    if is_selected {
                        ColorType::Palette(PaletteColor::Highlight)
                    } else {
                        ColorType::Color(*printer.theme.palette.custom("playing_bg").unwrap())
                    },
                ))
                .combine(Effect::Bold)
            } else if is_selected {
                Style::from(ColorStyle::highlight())
            } else {
                Style::from(ColorStyle::primary())
            };
            let sub_style = if is_selected {
                Style::from(ColorStyle::highlight()).combine(Effect::Dim)
            } else {
                Style::from(ColorStyle::secondary())
            };

            let text_width = printer.size.x.saturating_sub(text_x);
            let duration = item.display_right(&self.library);
            let title = match item {
                Playable::Track(track) => track.title.clone(),
                Playable::Episode(episode) => episode.name.clone(),
            };

            printer.with_style(title_style, |printer| {
                let max_title = text_width.saturating_sub(duration.width() + 1);
                let mut title = title.clone();
                while title.width() > max_title && !title.is_empty() {
                    title.pop();
                }
                printer.print((text_x, y0), &title);
                let x = printer.size.x.saturating_sub(duration.width());
                printer.print((x, y0), &duration);
            });

            if row_height >= 2 {
                let subtitle = Playable::format(item, "%artists — %album", &self.library);
                printer.with_style(sub_style, |printer| {
                    let mut subtitle = subtitle;
                    while subtitle.width() > text_width && !subtitle.is_empty() {
                        subtitle.pop();
                    }
                    printer.print((text_x, y0 + 1), &subtitle);
                });
            }

            #[cfg(feature = "cover")]
            {
                let desired = match item.cover_url() {
                    Some(url) if thumb_cols > 0 => SlotImage::Image(url),
                    _ => SlotImage::Blank,
                };
                if thumb_cols > 0 {
                    self.draw_thumbnail(slot, desired, abs_position, side_px);
                }
            }
        }
    }

    fn layout(&mut self, size: Vec2) {
        if size != self.last_size {
            // Positions changed: force re-emission of all thumbnails.
            self.slots.write().unwrap().clear();
        }
        self.last_size = size;
        if self.content.read().unwrap().len() != self.last_content_len {
            self.refresh_filter();
        }
        self.clamp_scroll();
    }

    fn needs_relayout(&self) -> bool {
        true
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if self.filter_active {
            match event {
                Event::Char(c) => {
                    self.filter.push(c);
                    self.refresh_filter();
                    return EventResult::consumed();
                }
                Event::Key(Key::Backspace) => {
                    self.filter.pop();
                    self.refresh_filter();
                    return EventResult::consumed();
                }
                Event::Key(Key::Esc) => {
                    self.filter.clear();
                    self.filter_active = false;
                    self.refresh_filter();
                    return EventResult::consumed();
                }
                Event::Key(Key::Enter) | Event::Key(Key::Down) | Event::Key(Key::Up) => {
                    self.filter_active = false;
                    return EventResult::consumed();
                }
                _ => return EventResult::Ignored,
            }
        }

        match event {
            Event::Char('/') if self.filter_row_enabled() => {
                self.filter_active = true;
                EventResult::consumed()
            }
            Event::Mouse {
                event: mouse_event,
                position,
                offset,
            } => {
                let local = position.saturating_sub(offset);
                match mouse_event {
                    MouseEvent::WheelUp => {
                        self.move_selection(-3);
                        EventResult::consumed()
                    }
                    MouseEvent::WheelDown => {
                        self.move_selection(3);
                        EventResult::consumed()
                    }
                    MouseEvent::Press(MouseButton::Left) => {
                        let header = usize::from(self.filter_row_enabled());
                        if self.filter_row_enabled() && local.y == 0 {
                            self.filter_active = true;
                            return EventResult::consumed();
                        }
                        let slot = (local.y - header) / self.row_height();
                        let index = self.scroll_top + slot;
                        if index < self.filtered.len() {
                            if index == self.selected {
                                // Second click on the selection plays it.
                                self.with_selected_item_mut(|item| item.play(&self.queue));
                            } else {
                                self.selected = index;
                                self.clamp_scroll();
                            }
                        }
                        EventResult::consumed()
                    }
                    _ => EventResult::Ignored,
                }
            }
            _ => EventResult::Ignored,
        }
    }

    fn important_area(&self, view_size: Vec2) -> Rect {
        Rect::from_point((view_size.x, self.selected.min(view_size.y)))
    }
}

impl ViewExt for TrackListView {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Play => {
                self.with_selected_item_mut(|item| item.play(&self.queue));
                Ok(CommandResult::Consumed(None))
            }
            Command::PlayNext => {
                self.with_selected_item_mut(|item| item.play_next(&self.queue));
                Ok(CommandResult::Consumed(None))
            }
            Command::Queue => {
                self.with_selected_item_mut(|item| item.queue(&self.queue));
                Ok(CommandResult::Consumed(None))
            }
            Command::Save => {
                self.with_selected_item_mut(|item| item.save(&self.library));
                Ok(CommandResult::Consumed(None))
            }
            Command::Delete => {
                self.with_selected_item_mut(|item| item.unsave(&self.library));
                Ok(CommandResult::Consumed(None))
            }
            Command::Add => {
                if let Some(track) = self.selected_item().and_then(|item| item.track()) {
                    let dialog = ContextMenu::add_track_dialog(
                        self.library.clone(),
                        self.queue.get_spotify(),
                        track,
                    );
                    return Ok(CommandResult::Modal(Box::new(dialog)));
                }
                Ok(CommandResult::Consumed(None))
            }
            #[cfg(feature = "share_clipboard")]
            Command::Share(mode) => {
                let url = match mode {
                    TargetMode::Selected => self.selected_item().and_then(|t| t.share_url()),
                    TargetMode::Current => self
                        .queue
                        .get_current()
                        .and_then(|t| t.as_listitem().share_url()),
                };
                if let Some(url) = url {
                    crate::sharing::write_share(url).ok();
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::Move(mode, amount) => {
                let delta = match amount {
                    MoveAmount::Extreme => i32::MAX / 2,
                    MoveAmount::Float(scale) => (self.last_size.y as f32 * scale) as i32,
                    MoveAmount::Integer(amount) => *amount,
                };
                match mode {
                    MoveMode::Up => {
                        if matches!(amount, MoveAmount::Extreme) {
                            self.selected = 0;
                            self.clamp_scroll();
                        } else {
                            self.move_selection(-delta);
                        }
                        Ok(CommandResult::Consumed(None))
                    }
                    MoveMode::Down => {
                        self.move_selection(delta);
                        Ok(CommandResult::Consumed(None))
                    }
                    MoveMode::Playing => {
                        if let Some(playing) = self.queue.get_current_index()
                            && let Some(position) =
                                self.filtered.iter().position(|&index| index == playing)
                        {
                            self.selected = position;
                            self.clamp_scroll();
                        }
                        Ok(CommandResult::Consumed(None))
                    }
                    _ => Ok(CommandResult::Ignored),
                }
            }
            Command::Open(TargetMode::Selected) => {
                if let Some(item) = self.selected_item() {
                    let target = item.as_listitem();
                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    return match target.open(queue.clone(), library.clone()) {
                        Some(view) => Ok(CommandResult::View(view)),
                        None => {
                            let contextmenu = ContextMenu::new(&*target, queue, library);
                            Ok(CommandResult::Modal(Box::new(contextmenu)))
                        }
                    };
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::Goto(mode) => {
                if let Some(item) = self.selected_item() {
                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    match mode {
                        GotoMode::Album => {
                            if let Some(album) = item.album(&queue)
                                && let Some(view) = album.open(queue, library)
                            {
                                return Ok(CommandResult::View(view));
                            }
                        }
                        GotoMode::Artist => {
                            if let Some(artists) = item.artists() {
                                return match artists.len() {
                                    0 => Ok(CommandResult::Consumed(None)),
                                    1 => {
                                        let view = ArtistView::new(queue, library, &artists[0])
                                            .into_boxed_view_ext();
                                        Ok(CommandResult::View(view))
                                    }
                                    _ => {
                                        let dialog = ContextMenu::select_artist_dialog(
                                            library, queue, artists,
                                        );
                                        Ok(CommandResult::Modal(Box::new(dialog)))
                                    }
                                };
                            }
                        }
                    }
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::ShowRecommendations(TargetMode::Selected) => {
                if let Some(item) = self.selected_item() {
                    let mut target = item.as_listitem();
                    if let Some(view) =
                        target.open_recommendations(self.queue.clone(), self.library.clone())
                    {
                        return Ok(CommandResult::View(view));
                    }
                }
                Ok(CommandResult::Consumed(None))
            }
            Command::Sort(key, direction) => {
                if let Some(id) = &self.sort_context {
                    self.library.cfg.with_state_mut(|state| {
                        state.playlist_orders.insert(
                            id.clone(),
                            crate::config::SortingOrder {
                                key: key.clone(),
                                direction: direction.clone(),
                            },
                        );
                    });
                }
                sort_playables(&mut self.content.write().unwrap(), key, direction);
                self.refresh_filter();
                Ok(CommandResult::Consumed(None))
            }
            Command::Jump(JumpMode::Query(query)) => {
                self.filter = query.clone();
                self.refresh_filter();
                Ok(CommandResult::Consumed(None))
            }
            _ => Ok(CommandResult::Ignored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_image_change_detection() {
        let a = SlotImage::Image("url-a".into());
        let b = SlotImage::Image("url-b".into());
        assert_ne!(a, b);
        assert_eq!(a, SlotImage::Image("url-a".into()));
        assert_ne!(SlotImage::Blank, SlotImage::Empty);
    }
}
