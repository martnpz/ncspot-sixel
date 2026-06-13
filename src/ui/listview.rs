use cursive::view::scroll::Scroller;
use log::info;
use std::cmp::{Ordering, max, min};
use std::sync::{Arc, RwLock};

use cursive::align::HAlign;
use cursive::event::{Callback, Event, EventResult, MouseButton, MouseEvent};
use cursive::theme::{ColorStyle, ColorType, PaletteColor};
use cursive::traits::View;
use cursive::view::scroll;
use cursive::{Cursive, Printer, Rect, Vec2, XY};
use unicode_width::UnicodeWidthStr;

use crate::command::{Command, GotoMode, InsertSource, JumpMode, MoveAmount, MoveMode, TargetMode};
use crate::commands::CommandResult;
use crate::config::Config;
use crate::events::EventManager;
use crate::library::Library;
use crate::model::album::Album;
use crate::model::artist::Artist;
use crate::model::episode::Episode;
use crate::model::playable::Playable;
use crate::model::playlist::Playlist;
use crate::model::show::Show;
use crate::model::track::Track;
use crate::queue::Queue;
#[cfg(feature = "share_clipboard")]
use crate::sharing::{read_share, write_share};
use crate::spotify::UriType;
use crate::traits::{IntoBoxedViewExt, ListItem, ViewExt};
use crate::ui::album::AlbumView;
use crate::ui::artist::ArtistView;
use crate::ui::contextmenu::ContextMenu;
use crate::ui::pagination::Pagination;

#[cfg(feature = "cover")]
use crate::ui::cover::sixel::SixelImageCache;

#[cfg(feature = "cover")]
#[derive(Clone, Debug, PartialEq, Eq, Default)]
enum SlotImage {
    #[default]
    Empty,
    /// Sixel was emitted on the first frame after invalidation but the
    /// crossterm diff for that frame may have overwritten it (stale shadow
    /// buffer).  A second frame is scheduled; that frame finds an empty diff
    /// and the emission sticks, after which the state advances to Image.
    Pending,
    Blank,
    Image(String),
}

#[cfg(feature = "cover")]
#[derive(Clone, Default)]
struct SlotState {
    image: SlotImage,
    position: Vec2,
}

pub enum MouseHandleResult {
    Handled(EventResult),
    Unhandled(Command),
}

pub struct ListView<I: ListItem> {
    content: Arc<RwLock<Vec<I>>>,
    last_content_len: usize,
    selected: usize,
    search_query: String,
    search_indexes: Vec<usize>,
    search_selected_index: usize,
    last_size: Vec2,
    scroller: scroll::Core,
    queue: Arc<Queue>,
    library: Arc<Library>,
    pagination: Pagination<I>,
    title: String,
    cfg: Option<Arc<Config>>,
    #[cfg(feature = "cover")]
    images: Option<Arc<SixelImageCache>>,
    #[cfg(feature = "cover")]
    events: Option<EventManager>,
    #[cfg(feature = "cover")]
    slots: RwLock<Vec<SlotState>>,
}

impl<I: ListItem> Scroller for ListView<I> {
    fn get_scroller_mut(&mut self) -> &mut scroll::Core {
        &mut self.scroller
    }

    fn get_scroller(&self) -> &scroll::Core {
        &self.scroller
    }
}

impl<I: ListItem + Clone> ListView<I> {
    pub fn new(content: Arc<RwLock<Vec<I>>>, queue: Arc<Queue>, library: Arc<Library>) -> Self {
        let result = Self {
            content,
            last_content_len: 0,
            selected: 0,
            search_query: String::new(),
            search_indexes: Vec::new(),
            search_selected_index: 0,
            last_size: Vec2::new(0, 0),
            scroller: scroll::Core::new(),
            queue,
            library,
            pagination: Pagination::default(),
            title: "".to_string(),
            cfg: None,
            #[cfg(feature = "cover")]
            images: None,
            #[cfg(feature = "cover")]
            events: None,
            #[cfg(feature = "cover")]
            slots: RwLock::new(Vec::new()),
        };
        result.try_paginate();
        result
    }

    pub fn with_title(mut self, title: &str) -> Self {
        self.title = title.to_string();
        self
    }

    /// Clear the slot cache without erasing terminal content.
    /// Call this when another view has drawn over this view's terminal area
    /// (e.g. a browser section switch), so the next draw re-emits all sixels.
    #[cfg(feature = "cover")]
    pub fn invalidate_slots(&self) {
        self.slots.write().unwrap().clear();
    }

    /// Enable cover thumbnails. `cfg` supplies the `[layout.browser]` config;
    /// `images` is the shared sixel cache (None when sixel is unavailable).
    /// `events` is used to schedule a second draw frame after the first frame
    /// clears any stale shadow-buffer content in the thumbnail columns.
    #[cfg(feature = "cover")]
    pub fn with_thumbnails(
        mut self,
        cfg: Arc<Config>,
        images: Option<Arc<SixelImageCache>>,
        events: EventManager,
    ) -> Self {
        self.cfg = Some(cfg);
        self.images = images;
        self.events = Some(events);
        self
    }

    fn browser_config<T>(&self, get: impl Fn(&crate::config::BrowserConfig) -> Option<T>) -> Option<T> {
        self.cfg.as_ref()?.values().layout.as_ref()?.browser.as_ref().and_then(get)
    }

    fn row_height(&self) -> usize {
        #[cfg(feature = "cover")]
        if self.images.is_some() {
            return self.browser_config(|b| b.row_height).unwrap_or(2).max(1) as usize;
        }
        1
    }

    fn thumb_columns(&self, cell_px: Vec2) -> usize {
        #[cfg(feature = "cover")]
        {
            let enabled = self.browser_config(|b| b.thumbnails).unwrap_or(true);
            if enabled && self.images.is_some() && self.row_height() >= 2 {
                let side_px = self.row_height() * cell_px.y;
                return side_px.div_ceil(cell_px.x.max(1));
            }
        }
        let _ = cell_px;
        0
    }

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
            // Pending is only set internally; it is never passed as desired.
            SlotImage::Blank | SlotImage::Empty | SlotImage::Pending => {
                if state.image == SlotImage::Empty && state.position == position {
                    return;
                }
                images.blank(side_px, side_px)
            }
        };
        match sixel {
            Some(sixel) => {
                if crate::ui::cover::sixel::emit(&sixel, position).is_ok() {
                    if state.image == SlotImage::Pending {
                        // Second frame: the crossterm diff for the thumbnail
                        // columns is now empty (same spaces as last frame), so
                        // this emission sticks.  Advance to the final state.
                        state.image = desired;
                        state.position = position;
                    } else {
                        // First emission for this slot (any prior state:
                        // Empty after invalidation, Blank after encoding,
                        // Image after a position change on resize, …).
                        // The crossterm diff for this frame may carry stale
                        // cell content for the thumbnail columns (full redraw
                        // on resize, section switch from a non-thumbnail
                        // section, etc.) and will arrive AFTER our /dev/tty
                        // write, overwriting the sixel.  Schedule a second
                        // frame; that frame's diff will be empty for these
                        // columns and the sixel will stick.
                        state.image = SlotImage::Pending;
                        state.position = position;
                        if let Some(ev) = &self.events {
                            ev.trigger();
                        }
                    }
                }
            }
            None => {
                // Emit a blank while the image is encoding so the previous
                // section's sixel doesn't bleed through. Skip only if we
                // already blanked this slot at this position.
                if !matches!(state.image, SlotImage::Blank)
                    && let Some(blank) = images.blank(side_px, side_px)
                    && crate::ui::cover::sixel::emit(&blank, position).is_ok()
                {
                    state.image = SlotImage::Blank;
                    state.position = position;
                }
            }
        }
    }

    pub fn get_pagination(&self) -> &Pagination<I> {
        &self.pagination
    }

    /// Return the current amount of items in `content`
    ///
    /// If `include_paginator` is `true`, the pagination entry will be included
    /// in the count.
    pub fn content_len(&self, include_paginator: bool) -> usize {
        let content_len = self.content.read().unwrap().len();

        // add 1 more row for paginator if we can paginate
        if self.can_paginate() && include_paginator {
            content_len + 1
        } else {
            content_len
        }
    }

    /// Return wether there are still items that aren't shown in the listview.
    ///
    /// `true` if there are unloaded items
    /// `false` if all items are loaded
    fn can_paginate(&self) -> bool {
        self.get_pagination().max_content().unwrap_or(0) > self.get_pagination().loaded_content()
    }

    /// Try to load more items into the list if neccessary.
    #[inline]
    fn try_paginate(&self) {
        // Paginate if there are more items
        //  AND
        //   The selected item is the current last item (keyboard scrolling)
        //    OR
        //   The scroller can't scroll further down (mouse scrolling)
        if self.can_paginate()
            && (self.selected == self.content.read().unwrap().len().saturating_sub(1)
                || !self.scroller.can_scroll_down())
        {
            self.pagination.call(&self.content, self.library.clone());
        }
    }

    /// Return whether this [ListView] has visible scrollbars.
    pub fn has_visible_scrollbars(&self) -> bool {
        self.scroller.get_show_scrollbars()
    }

    pub fn get_selected_index(&self) -> usize {
        self.selected
    }

    pub fn get_indexes_of(&self, query: &str) -> Vec<usize> {
        let content = self.content.read().unwrap();
        content
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.display_left(&self.library)
                    .to_lowercase()
                    .contains(&query[..].to_lowercase())
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn move_focus_to(&mut self, target: usize) {
        let len = self.content_len(false).saturating_sub(1);
        self.selected = min(target, len);
        self.scroller.scroll_to_y(self.selected * self.row_height());
    }

    pub fn move_focus(&mut self, delta: i32) {
        let new = self.selected as i32 + delta;
        self.move_focus_to(max(new, 0) as usize);
    }

    /// Append the currently selected item and all the following ones to the queue after the
    /// currently playing track and start playing them. Returns true if adding and playing the
    /// tracks succeeded, false otherwhise.
    fn attempt_play_all_tracks(&self) -> bool {
        let content = self.content.read().unwrap();
        let any = &(*content) as &dyn std::any::Any;
        let playables = any.downcast_ref::<Vec<Playable>>();
        let tracks = any.downcast_ref::<Vec<Track>>().map(|t| {
            t.iter()
                .map(|t| Playable::Track(t.clone()))
                .collect::<Vec<Playable>>()
        });
        if let Some(tracks) = playables.or(tracks.as_ref()) {
            let index = self.queue.append_next(tracks);
            self.queue.play(index + self.selected, true, false);
            true
        } else {
            false
        }
    }

    /// Appends the currently focused item after the currently playing item and starts playing it.
    fn play_current_item(&mut self) {
        let mut content = self.content.write().unwrap();
        if let Some(listitem) = content.get_mut(self.selected) {
            listitem.play(&self.queue);
        }
    }

    /// Get the selected row from a mouse position and offset
    fn get_selected_row(&self, position: XY<usize>, offset: XY<usize>) -> Option<usize> {
        let rh = self.row_height();
        let viewport = self.scroller.content_viewport().top_left();
        let row = position.checked_sub(offset).map(|p| p.y + viewport.y)?;
        let item = row / rh;
        (item < self.content_len(false)).then_some(item)
    }

    fn run_play_command(&mut self) {
        self.queue.clear();

        if !self.attempt_play_all_tracks() {
            self.play_current_item();
        }
    }

    /// Takes an incoming mouse event from Cursive and tries to act appropriately.
    ///
    /// Returns a MouseHandleResult which indicates whether the event has been handled by
    /// the function or if a command needs further processing.
    pub fn handle_mouse_event(&mut self, e: Event) -> MouseHandleResult {
        match e {
            Event::Mouse {
                event: MouseEvent::WheelUp,
                ..
            } => self.scroller.scroll_up(3),
            Event::Mouse {
                event: MouseEvent::WheelDown,
                ..
            } => {
                self.scroller.scroll_down(3);
                self.try_paginate();
            }
            Event::Mouse {
                event: MouseEvent::Press(MouseButton::Left),
                position,
                offset,
            } => {
                // This is safe as a mouse event is only propagated to a view when it is inside the
                // view. Therefore underflow shouldn't occur.
                let view_coordinates_click_position = position - offset;

                let drag_started = if self.has_visible_scrollbars() {
                    self.scroller.start_drag(view_coordinates_click_position)
                } else {
                    false
                };

                if drag_started {
                    log::debug!("grabbing scroller");
                } else if let Some(clicked_row_index) = self.get_selected_row(position, offset) {
                    let currently_selected_listitem = self
                        .content
                        .read()
                        .unwrap()
                        .get(clicked_row_index)
                        .map(ListItem::as_listitem);
                    let currently_selected_is_individual = currently_selected_listitem
                        .filter(|item| item.track().is_some())
                        .is_some();
                    if self.selected == clicked_row_index && currently_selected_is_individual {
                        return MouseHandleResult::Unhandled(Command::Play);
                    } else {
                        // The clicked position wasn't focused yet or the item is a collection
                        // that can be opened.
                        self.move_focus_to(clicked_row_index);
                        let content = self.content.read().unwrap();
                        let clicked_list_item =
                            content.get(self.selected).map(ListItem::as_listitem);

                        if let Some(target) = clicked_list_item
                            && let Some(view) =
                                target.open(self.queue.clone(), self.library.clone())
                        {
                            return MouseHandleResult::Handled(EventResult::Consumed(Some(
                                Callback::from_fn_once(move |s| {
                                    crate::ui::panes::show_view(s, view);
                                }),
                            )));
                        }
                    }
                }
            }
            Event::Mouse {
                event: MouseEvent::Press(MouseButton::Right),
                position,
                offset,
            } => {
                if let Some(y) = self.get_selected_row(position, offset) {
                    self.move_focus_to(y);

                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    if let Some(target) = {
                        let content = self.content.read().unwrap();
                        content.get(self.selected).map(|t| t.as_listitem())
                    } {
                        let contextmenu = ContextMenu::new(&*target, queue, library);
                        return MouseHandleResult::Handled(EventResult::Consumed(Some(
                            Callback::from_fn_once(move |s| s.add_layer(contextmenu)),
                        )));
                    }
                }
            }
            Event::Mouse {
                event: MouseEvent::Hold(MouseButton::Left),
                position,
                offset,
            } => {
                if self.has_visible_scrollbars() {
                    self.scroller.drag(position.saturating_sub(offset));
                }
            }
            Event::Mouse {
                event: MouseEvent::Release(MouseButton::Left),
                ..
            } => {
                log::debug!("releasing scroller");
                self.scroller.release_grab();
            }
            _ => {
                return MouseHandleResult::Handled(EventResult::Ignored);
            }
        }

        MouseHandleResult::Handled(EventResult::Consumed(None))
    }
}

impl<I: ListItem + Clone> View for ListView<I> {
    fn draw(&self, printer: &Printer<'_, '_>) {
        #[cfg(feature = "cover")]
        let cell_px =
            crate::ui::cover::cell_size_px(self.cfg.as_ref().and_then(|c| c.values().cover_max_scale).unwrap_or(1.0));
        #[cfg(not(feature = "cover"))]
        let cell_px = Vec2::new(8, 16);
        let thumb_cols = self.thumb_columns(cell_px);
        let text_x = if thumb_cols > 0 { thumb_cols + 1 } else { 0 };

        // Explicitly fill thumbnail columns with a stable background style so
        // the shadow-buffer cells for those columns are identical every frame.
        // Without this the cells rely on the pane wipe's default style, which
        // can vary (e.g. focused/unfocused); a style difference between frame 1
        // (first emission) and frame 2 (Pending re-emission) would produce a
        // non-empty crossterm diff that overwrites the sixel on frame 2.
        // CoverView does the same — that's why it survived resize already.
        #[cfg(feature = "cover")]
        if thumb_cols > 0 {
            let bg = ColorStyle::new(
                ColorType::Palette(PaletteColor::Background),
                ColorType::Palette(PaletteColor::Background),
            );
            printer.with_color(bg, |p| {
                for y in 0..p.size.y {
                    p.print_hline((0, y), thumb_cols, " ");
                }
            });
        }

        scroll::draw_lines(self, printer, |_, printer, i| {
            let rh = self.row_height();
            let content = self.content.read().unwrap();
            let item_count = content.len();
            let item_rows = item_count * rh;

            // Paginator row — sits after all item rows.
            if i >= item_rows {
                if self.can_paginate() {
                    let max = self.pagination.max_content().unwrap();
                    let buf = format!("{} more items, scroll to load", max - item_count);
                    printer.with_color(ColorStyle::secondary(), |p| p.print((0, 0), &buf));
                }
                return;
            }

            let item_index = i / rh;
            let row_within = i % rh;

            if item_index >= item_count {
                return;
            }

            let item = &content[item_index];
            let currently_playing =
                item.is_playing(&self.queue) && self.queue.get_current_index() == Some(item_index);
            let is_local = item.track().map(|t| t.is_local).unwrap_or_default();
            let is_playable = item.track().map(|t| t.is_playable).unwrap_or_default();

            let style = if self.selected == item_index {
                if currently_playing {
                    ColorStyle::new(
                        *printer.theme.palette.custom("playing_selected").unwrap(),
                        ColorType::Palette(PaletteColor::Highlight),
                    )
                } else if is_local {
                    ColorStyle::new(
                        ColorType::Palette(PaletteColor::Secondary),
                        ColorType::Palette(PaletteColor::Highlight),
                    )
                } else {
                    ColorStyle::highlight()
                }
            } else if currently_playing {
                ColorStyle::new(
                    ColorType::Color(*printer.theme.palette.custom("playing").unwrap()),
                    ColorType::Color(*printer.theme.palette.custom("playing_bg").unwrap()),
                )
            } else if is_local || is_playable == Some(false) {
                ColorStyle::secondary()
            } else {
                ColorStyle::primary()
            };

            if row_within > 0 {
                // Extra rows: fill text area with background, thumbnail fills the rest visually.
                printer.with_color(style, |p| {
                    p.print_hline((text_x, 0), p.size.x.saturating_sub(text_x), " ");
                });
                return;
            }

            // row_within == 0: draw text columns.
            let left = item.display_left(&self.library);
            let center = item.display_center(&self.library);
            let right = item.display_right(&self.library);
            let draw_center = !center.is_empty();
            let avail_w = printer.size.x.saturating_sub(text_x);

            // draw left string
            printer.with_color(style, |printer| {
                printer.print_hline((text_x, 0), avail_w, " ");
                printer.print((text_x, 0), &left);
            });

            // search match highlight
            if self.search_indexes.contains(&item_index) {
                let fg = *printer.theme.palette.custom("search_match").unwrap();
                let matched_style = ColorStyle::new(fg, style.back);
                let matches: Vec<(usize, usize)> = left
                    .to_lowercase()
                    .match_indices(&self.search_query)
                    .map(|m| (m.0, m.0 + m.1.len()))
                    .collect();
                for m in matches {
                    printer.with_color(matched_style, |printer| {
                        printer.print(
                            (text_x + left[0..m.0].width(), 0),
                            &left[m.0..m.1],
                        );
                    });
                }
            }

            // left string cut-off indicator
            let center_offset = text_x + avail_w / 2;
            let left_max_length = if draw_center {
                center_offset.saturating_sub(1)
            } else {
                printer.size.x.saturating_sub(right.width() + 1)
            };
            if left_max_length < text_x + left.width() {
                let offset = left_max_length.saturating_sub(1);
                printer.with_color(style, |printer| {
                    printer.print_hline((offset, 0), printer.size.x, " ");
                    printer.print((offset, 0), "..");
                });
            }

            // center string
            if draw_center {
                printer.with_color(style, |printer| {
                    printer.print((center_offset, 0), &center);
                });
                let max_length = printer.size.x.saturating_sub(right.width() + 1);
                if max_length < center_offset + center.width() {
                    let offset = max_length.saturating_sub(1);
                    printer.with_color(style, |printer| {
                        printer.print((offset, 0), "..");
                    });
                }
            }

            // right string
            let offset = HAlign::Right.get_offset(right.width(), printer.size.x);
            printer.with_color(style, |printer| {
                printer.print((offset, 0), &right);
            });

            // Emit sixel thumbnail (out-of-band; spans all rows of this item).
            #[cfg(feature = "cover")]
            if thumb_cols > 0 {
                let side_px = rh * cell_px.y;
                let abs_position = printer.offset;
                let desired = match item.cover_url() {
                    Some(url) => SlotImage::Image(url),
                    None => SlotImage::Blank,
                };
                self.draw_thumbnail(item_index, desired, abs_position, side_px);
            }
        });
    }

    fn layout(&mut self, size: Vec2) {
        if size != self.last_size {
            // Positions changed — force re-emission of all thumbnails.
            #[cfg(feature = "cover")]
            {
                self.slots.write().unwrap().clear();
                // Fire a second trigger so the resize always produces two
                // draw frames (same as set_section does). One trigger comes
                // from draw_thumbnail's first emission; this one is the
                // belt-and-suspenders redundant trigger.
                if let Some(ev) = &self.events {
                    ev.trigger();
                }
            }
        }
        self.last_size = size;

        let relayout_scroller = self.content_len(false) != self.last_content_len;
        self.last_content_len = self.content_len(true);

        let rh = self.row_height();
        scroll::layout(
            self,
            size,
            relayout_scroller,
            |_, _| {},
            |s, c| {
                let item_rows = s.content.read().unwrap().len() * rh;
                let total = item_rows + if s.can_paginate() { 1 } else { 0 };
                Vec2::new(c.x, total)
            },
        );
    }

    fn needs_relayout(&self) -> bool {
        // Always redraw when thumbnails are enabled so that after on_leave()
        // clears the slot cache, the very next frame re-emits all sixels.
        #[cfg(feature = "cover")]
        if self.images.is_some() {
            return true;
        }
        self.scroller.needs_relayout()
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        constraint
    }

    fn on_event(&mut self, e: Event) -> EventResult {
        match self.handle_mouse_event(e) {
            MouseHandleResult::Handled(event_result) => event_result,
            MouseHandleResult::Unhandled(command) => match command {
                Command::Play => {
                    self.run_play_command();
                    EventResult::consumed()
                }
                _ => EventResult::Ignored,
            },
        }
    }

    fn important_area(&self, view_size: Vec2) -> Rect {
        if self.content_len(false) > 0 {
            Rect::from_point((view_size.x, self.selected))
        } else {
            Rect::from_point((0, 0))
        }
    }
}

impl<I: ListItem + Clone> ViewExt for ListView<I> {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn on_leave(&self) {
        #[cfg(feature = "cover")]
        {
            use std::io::Write;
            let cfg = match &self.cfg { Some(c) => c, None => return };
            let cell_px =
                crate::ui::cover::cell_size_px(cfg.values().cover_max_scale.unwrap_or(1.0));
            let thumb_cols = self.thumb_columns(cell_px);
            let rh = self.row_height();
            if thumb_cols > 0 {
                let positions: Vec<Vec2> = {
                    let slots = self.slots.read().unwrap();
                    slots
                        .iter()
                        .filter(|s| !matches!(s.image, SlotImage::Empty))
                        .map(|s| s.position)
                        .collect()
                };
                if !positions.is_empty() {
                    let spaces = " ".repeat(thumb_cols);
                    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                        let mut out = Vec::new();
                        out.extend_from_slice(b"\x1b[?2026h\x1b7");
                        for pos in &positions {
                            for r in 0..rh {
                                let row = pos.y + r + 1;
                                let col = pos.x + 1;
                                out.extend_from_slice(
                                    format!("\x1b[{row};{col}H{spaces}").as_bytes(),
                                );
                            }
                        }
                        out.extend_from_slice(b"\x1b8\x1b[?2026l");
                        let _ = tty.write_all(&out);
                    }
                }
                self.slots.write().unwrap().clear();
            }
        }
    }

    fn on_command(&mut self, _s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        match cmd {
            Command::Play => {
                self.run_play_command();
                return Ok(CommandResult::Consumed(None));
            }
            Command::PlayNext => {
                info!("played next");
                let mut content = self.content.write().unwrap();
                if let Some(item) = content.get_mut(self.selected) {
                    item.play_next(&self.queue);
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Queue => {
                let mut content = self.content.write().unwrap();
                if let Some(item) = content.get_mut(self.selected) {
                    item.queue(&self.queue);
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Save => {
                let mut item = {
                    let content = self.content.read().unwrap();
                    content.get(self.selected).cloned()
                };

                if let Some(item) = item.as_mut() {
                    item.save(&self.library);
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Add => {
                let item = {
                    let content = self.content.read().unwrap();
                    content.get(self.selected).cloned()
                };

                if let Some(track) = item
                    && let Some(track) = track.track()
                {
                    let dialog = ContextMenu::add_track_dialog(
                        self.library.clone(),
                        self.queue.get_spotify(),
                        track,
                    );
                    return Ok(CommandResult::Modal(Box::new(dialog)));
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Delete => {
                let mut item = {
                    let content = self.content.read().unwrap();
                    content.get(self.selected).cloned()
                };

                if let Some(item) = item.as_mut() {
                    item.unsave(&self.library);
                }

                return Ok(CommandResult::Consumed(None));
            }
            #[cfg(feature = "share_clipboard")]
            Command::Share(mode) => {
                let url = match mode {
                    TargetMode::Selected => self.content.read().ok().and_then(|content| {
                        content.get(self.selected).and_then(ListItem::share_url)
                    }),
                    TargetMode::Current => self
                        .queue
                        .get_current()
                        .and_then(|t| t.as_listitem().share_url()),
                };

                if let Some(url) = url {
                    write_share(url).ok();
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::Jump(mode) => match mode {
                JumpMode::Query(query) => {
                    self.search_query = query.to_lowercase();
                    self.search_indexes = self.get_indexes_of(query);
                    self.search_selected_index = 0;
                    match self.search_indexes.first() {
                        Some(&index) => {
                            self.move_focus_to(index);
                            return Ok(CommandResult::Consumed(None));
                        }
                        None => return Ok(CommandResult::Consumed(None)),
                    }
                }
                JumpMode::Next => {
                    let len = self.search_indexes.len();
                    if len == 0 {
                        return Ok(CommandResult::Ignored);
                    }
                    let index = self.search_selected_index;
                    let next_index = match index.cmp(&(len - 1)) {
                        Ordering::Equal => 0,
                        _ => index + 1,
                    };
                    self.move_focus_to(self.search_indexes[next_index]);
                    self.search_selected_index = next_index;
                    return Ok(CommandResult::Consumed(None));
                }
                JumpMode::Previous => {
                    let len = self.search_indexes.len();
                    if len == 0 {
                        return Ok(CommandResult::Ignored);
                    }
                    let index = self.search_selected_index;
                    let prev_index = match index.cmp(&0) {
                        Ordering::Equal => len - 1,
                        _ => index - 1,
                    };
                    self.move_focus_to(self.search_indexes[prev_index]);
                    self.search_selected_index = prev_index;
                    return Ok(CommandResult::Consumed(None));
                }
            },
            Command::Move(mode, amount) => {
                let last_idx = self.content.read().unwrap().len().saturating_sub(1);

                match mode {
                    MoveMode::Up => {
                        if self.selected > 0 {
                            match amount {
                                MoveAmount::Extreme => self.move_focus_to(0),
                                MoveAmount::Float(scale) => {
                                    let amount = (self.last_size.y as f32) * scale;
                                    self.move_focus(-(amount as i32))
                                }
                                MoveAmount::Integer(amount) => self.move_focus(-(*amount)),
                            }
                        }
                        return Ok(CommandResult::Consumed(None));
                    }
                    MoveMode::Down => {
                        if self.selected < last_idx {
                            match amount {
                                MoveAmount::Extreme => self.move_focus_to(last_idx),
                                MoveAmount::Float(scale) => {
                                    let amount = (self.last_size.y as f32) * scale;
                                    self.move_focus(amount as i32)
                                }
                                MoveAmount::Integer(amount) => self.move_focus(*amount),
                            }
                        }
                        self.try_paginate();
                        return Ok(CommandResult::Consumed(None));
                    }
                    _ => return Ok(CommandResult::Consumed(None)),
                }
            }
            Command::Open(mode) => {
                let queue = self.queue.clone();
                let library = self.library.clone();
                let target: Option<Box<dyn ListItem>> = match mode {
                    TargetMode::Current => self.queue.get_current().map(|t| t.as_listitem()),
                    TargetMode::Selected => {
                        let content = self.content.read().unwrap();
                        content.get(self.selected).map(|t| t.as_listitem())
                    }
                };

                // if item has a dedicated view, show it; otherwise open the context menu
                if let Some(target) = target {
                    let view = target.open(queue.clone(), library.clone());
                    return match view {
                        Some(view) => Ok(CommandResult::View(view)),
                        None => {
                            let contextmenu = ContextMenu::new(&*target, queue, library);
                            Ok(CommandResult::Modal(Box::new(contextmenu)))
                        }
                    };
                }
            }
            Command::Goto(mode) => {
                let mut content = self.content.write().unwrap();
                if let Some(item) = content.get_mut(self.selected) {
                    let queue = self.queue.clone();
                    let library = self.library.clone();

                    match mode {
                        GotoMode::Album => {
                            if let Some(album) = item.album(&queue) {
                                let view =
                                    AlbumView::new(queue, library, &album).into_boxed_view_ext();
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
            }
            Command::Insert(source) => {
                let url = match source {
                    InsertSource::Input(url) => Some(url.clone()),
                    #[cfg(feature = "share_clipboard")]
                    InsertSource::Clipboard => read_share()
                        .ok()
                        .and_then(crate::spotify_url::SpotifyUrl::from_url),
                };

                let spotify = self.queue.get_spotify();

                if let Some(url) = url {
                    let target: Option<Box<dyn ListItem>> = match url.uri_type {
                        UriType::Track => spotify
                            .api
                            .track(&url.id)
                            .map(|track| Track::from(&track).as_listitem())
                            .ok(),
                        UriType::Album => spotify
                            .api
                            .album(&url.id)
                            .map(|album| Album::from(&album).as_listitem())
                            .ok(),
                        UriType::Playlist => spotify
                            .api
                            .playlist(&url.id)
                            .map(|playlist| Playlist::from(&playlist).as_listitem())
                            .ok(),
                        UriType::Artist => spotify
                            .api
                            .artist(&url.id)
                            .map(|artist| Artist::from(&artist).as_listitem())
                            .ok(),
                        UriType::Episode => spotify
                            .api
                            .episode(&url.id)
                            .map(|episode| Episode::from(&episode).as_listitem())
                            .ok(),
                        UriType::Show => spotify
                            .api
                            .show(&url.id)
                            .map(|show| Show::from(&show).as_listitem())
                            .ok(),
                    };

                    let queue = self.queue.clone();
                    let library = self.library.clone();
                    // if item has a dedicated view, show it; otherwise open the context menu
                    if let Some(target) = target {
                        let view = target.open(queue.clone(), library.clone());
                        return match view {
                            Some(view) => Ok(CommandResult::View(view)),
                            None => {
                                let contextmenu = ContextMenu::new(target.as_ref(), queue, library);
                                Ok(CommandResult::Modal(Box::new(contextmenu)))
                            }
                        };
                    }
                }

                return Ok(CommandResult::Consumed(None));
            }
            Command::ShowRecommendations(mode) => {
                let queue = self.queue.clone();
                let library = self.library.clone();
                let target: Option<Box<dyn ListItem>> = match mode {
                    TargetMode::Current => self.queue.get_current().map(|t| t.as_listitem()),
                    TargetMode::Selected => {
                        let content = self.content.read().unwrap();
                        content.get(self.selected).map(|t| t.as_listitem())
                    }
                };

                if let Some(mut target) = target {
                    let view = target.open_recommendations(queue, library);
                    return match view {
                        Some(view) => Ok(CommandResult::View(view)),
                        None => Ok(CommandResult::Consumed(None)),
                    };
                }
            }
            _ => {}
        };

        Ok(CommandResult::Ignored)
    }
}
