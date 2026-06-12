//! Multi-pane screen that shows several views side by side, configured
//! through the `[layout]` table in the config file.
//!
//! The pane layout is registered as the `"panes"` screen inside the regular
//! [Layout](crate::ui::layout::Layout), so the statusbar, command line and all
//! fullscreen screens keep working unchanged.

use cursive::Rect;
use cursive::align::HAlign;
use cursive::direction::Direction;
use cursive::event::{Event, EventResult, MouseEvent};
use cursive::theme::{ColorStyle, Effect, Style};
use cursive::view::{CannotFocus, Selector};
use cursive::{Cursive, Printer, Vec2, View};
use log::error;
use unicode_width::UnicodeWidthStr;

use crate::command::{Command, PaneDirection};
use crate::commands::CommandResult;
use crate::config::LayoutConfig;
use crate::traits::ViewExt;

pub struct Pane {
    kind: String,
    /// Stack of views; the last entry is shown. `Back` pops down to the
    /// first (root) view, which is never removed.
    views: Vec<Box<dyn ViewExt>>,
    /// Area of this pane, computed during layout. In view coordinates.
    rect: Rect,
}

impl Pane {
    fn top(&self) -> &dyn ViewExt {
        self.views.last().unwrap().as_ref()
    }

    fn top_mut(&mut self) -> &mut Box<dyn ViewExt> {
        self.views.last_mut().unwrap()
    }
}

/// Empty pane content shown until a real view is swapped in (e.g. the tracks
/// pane before a playlist was opened).
pub struct PlaceholderPane {
    title: String,
    text: String,
}

impl PlaceholderPane {
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
        }
    }
}

impl View for PlaceholderPane {
    fn draw(&self, printer: &Printer<'_, '_>) {
        printer.with_color(ColorStyle::secondary(), |printer| {
            let x = HAlign::Center.get_offset(self.text.width(), printer.size.x);
            printer.print((x, printer.size.y / 2), &self.text);
        });
    }
}

impl ViewExt for PlaceholderPane {
    fn title(&self) -> String {
        self.title.clone()
    }
}

struct Column {
    width_pct: u8,
    /// Per-pane height percentages. Empty = distribute evenly.
    height_pcts: Vec<u8>,
    panes: Vec<Pane>,
}

pub struct PaneLayoutView {
    columns: Vec<Column>,
    /// (column, row) of the focused pane.
    focused: (usize, usize),
    last_size: Vec2,
    /// Horizontal gap: between columns and at left/right screen edges.
    gap_x: usize,
    /// Vertical gap: between rows and at top/bottom screen edges.
    gap_y: usize,
    /// Horizontal padding: inside the border left/right.
    padding_x: usize,
    /// Vertical padding: inside the border top/bottom.
    padding_y: usize,
}

/// Draw a rounded border box at `rect` with the title centred in the top edge.
/// Focused islands get a brighter colour and bold title; unfocused are dimmed.
fn draw_island(printer: &Printer, rect: Rect, title: &str, focused: bool) {
    let w = rect.width();
    let h = rect.height();
    if w < 2 || h < 2 {
        return;
    }
    let tl = rect.top_left();
    let inner_w = w - 2;

    // Truncate title so it fits with one space of padding on each side.
    let max_title = inner_w.saturating_sub(2);
    let mut title_display = title.to_string();
    while title_display.width() > max_title && !title_display.is_empty() {
        title_display.pop();
    }
    let title_padded = if title_display.is_empty() {
        String::new()
    } else {
        format!(" {title_display} ")
    };
    let title_w = title_padded.width();
    let left_fill = inner_w.saturating_sub(title_w) / 2;
    let right_fill = inner_w.saturating_sub(title_w).saturating_sub(left_fill);

    let border_style = if focused {
        ColorStyle::title_primary()
    } else {
        ColorStyle::title_secondary()
    };
    let title_style = if focused {
        Style::from(ColorStyle::title_primary()).combine(Effect::Bold)
    } else {
        Style::from(ColorStyle::title_secondary())
    };

    printer.with_color(border_style, |p| {
        // Top edge: ╭───[back ]─ Title ─────╮
        let dashes_l = "─".repeat(left_fill);
        let dashes_r = "─".repeat(right_fill);
        p.print(tl, &format!("╭{dashes_l}"));
        p.with_style(title_style, |p| p.print(tl + (1 + left_fill, 0), &title_padded));
        p.print(tl + (1 + left_fill + title_w, 0), &format!("{dashes_r}╮"));

        // Bottom edge
        p.print(tl + (0, h - 1), &format!("╰{}╯", "─".repeat(inner_w)));

        // Side edges
        for row in 1..h - 1 {
            p.print(tl + (0, row), "│");
            p.print(tl + (w - 1, row), "│");
        }
    });
}

/// Show `view` in the "tracks" pane when the pane screen is currently
/// active, otherwise push it onto the fullscreen layout stack. Use this for
/// view-opening paths that bypass the command pipeline (mouse clicks,
/// context menu actions).
pub fn show_view(s: &mut cursive::Cursive, view: Box<dyn ViewExt>) {
    use crate::ext_traits::CursiveExt;

    let mut view = Some(view);
    // Try "tracks" pane first; fall back to "browser" for single-column layouts.
    s.call_on_name("panes", |panes: &mut PaneLayoutView| {
        if let Some(v) = view.take() {
            let returned = panes.push_to_pane("tracks", v);
            if let Some(v) = returned {
                view = panes.push_to_pane("browser", v);
            }
        }
    });
    match view {
        None => log::debug!("opened view in tracks/browser pane"),
        Some(view) => {
            log::debug!("opened view fullscreen");
            s.on_layout(|_, mut layout| layout.push_view(view));
        }
    }
}

impl PaneLayoutView {
    /// Build the pane layout from `config`, taking the views from `make_view`.
    /// Pane names that `make_view` doesn't know are skipped with an error log.
    pub fn new(
        config: &LayoutConfig,
        mut make_view: impl FnMut(&str) -> Option<Box<dyn ViewExt>>,
    ) -> Self {
        let total: u32 = config.columns.iter().map(|c| c.width as u32).sum();
        let config = if config.columns.is_empty() || total > 100 {
            error!("invalid [layout] config (column widths sum to {total}%), using default");
            &LayoutConfig::default()
        } else {
            config
        };

        let mut columns = Vec::new();
        for column_config in &config.columns {
            let mut panes = Vec::new();
            for kind in &column_config.panes {
                match make_view(kind) {
                    Some(view) => panes.push(Pane {
                        kind: kind.clone(),
                        views: vec![view],
                        rect: Rect::from_size((0, 0), (0, 0)),
                    }),
                    None => error!(r#"unknown pane "{kind}" in [layout] config, skipping"#),
                }
            }
            if !panes.is_empty() {
                // Validate and store height percentages; discard if length
                // doesn't match the number of successfully built panes.
                let height_pcts = column_config
                    .heights
                    .as_deref()
                    .filter(|h| h.len() == panes.len())
                    .map(|h| h.to_vec())
                    .unwrap_or_default();
                columns.push(Column {
                    width_pct: column_config.width,
                    height_pcts,
                    panes,
                });
            }
        }

        Self {
            columns,
            focused: (0, 0),
            last_size: Vec2::zero(),
            gap_x: config.gap_x.or(config.gap).unwrap_or(1) as usize,
            gap_y: config.gap_y.or(config.gap).unwrap_or(1) as usize,
            padding_x: config.padding_x.or(config.padding).unwrap_or(0) as usize,
            padding_y: config.padding_y.or(config.padding).unwrap_or(0) as usize,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.columns
            .get(self.focused.0)
            .and_then(|column| column.panes.get(self.focused.1))
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        self.columns
            .get_mut(self.focused.0)
            .and_then(|column| column.panes.get_mut(self.focused.1))
    }

    fn move_focus(&mut self, direction: PaneDirection) {
        let (mut col, mut row) = self.focused;
        match direction {
            PaneDirection::Left if col > 0 => {
                col -= 1;
            }
            PaneDirection::Right if col + 1 < self.columns.len() => {
                col += 1;
            }
            PaneDirection::Up if row > 0 => {
                row -= 1;
            }
            PaneDirection::Down => {
                if row + 1 < self.columns[col].panes.len() {
                    row += 1;
                }
            }
            PaneDirection::Next => {
                row += 1;
                if row >= self.columns[col].panes.len() {
                    row = 0;
                    col = (col + 1) % self.columns.len();
                }
            }
            _ => return,
        }
        row = row.min(self.columns[col].panes.len() - 1);

        if (col, row) != self.focused {
            if let Some(pane) = self.focused_pane() {
                pane.top().on_leave();
            }
            self.focused = (col, row);
        }
    }

    /// Push a view onto the stack of the first pane of `kind`. Returns the
    /// view back when no pane of that kind is configured.
    fn push_to_pane(&mut self, kind: &str, view: Box<dyn ViewExt>) -> Option<Box<dyn ViewExt>> {
        for (column_index, column) in self.columns.iter_mut().enumerate() {
            for (pane_index, pane) in column.panes.iter_mut().enumerate() {
                if pane.kind == kind {
                    pane.top().on_leave();
                    pane.views.push(view);
                    let rect = pane.rect;
                    let inset_x = 1 + self.padding_x;
                    let inset_y = 1 + self.padding_y;
                    pane.top_mut()
                        .layout(rect.size().saturating_sub((2 * inset_x, 2 * inset_y)));
                    self.focused = (column_index, pane_index);
                    return None;
                }
            }
        }
        Some(view)
    }

    /// Pop the focused pane's stack. Returns whether something was popped.
    fn pop_focused(&mut self) -> bool {
        let (inset_x, inset_y) = (1 + self.padding_x, 1 + self.padding_y);
        if let Some(pane) = self.focused_pane_mut()
            && pane.views.len() > 1
        {
            pane.top().on_leave();
            pane.views.pop();
            let rect = pane.rect;
            pane.top_mut()
                .layout(rect.size().saturating_sub((2 * inset_x, 2 * inset_y)));
            return true;
        }
        false
    }

    fn pane_at(&self, position: Vec2) -> Option<(usize, usize)> {
        for (column_index, column) in self.columns.iter().enumerate() {
            for (pane_index, pane) in column.panes.iter().enumerate() {
                if pane.rect.contains(position) {
                    return Some((column_index, pane_index));
                }
            }
        }
        None
    }
}

impl View for PaneLayoutView {
    fn draw(&self, printer: &Printer<'_, '_>) {
        for (column_index, column) in self.columns.iter().enumerate() {
            for (pane_index, pane) in column.panes.iter().enumerate() {
                let focused = (column_index, pane_index) == self.focused;
                let rect = pane.rect;

                // Island border with title in the top edge.
                let title = if pane.views.len() > 1 {
                    format!("< {}", pane.top().title())
                } else {
                    pane.top().title()
                };
                draw_island(printer, rect, &title, focused);

                // Content is inset by border (1) + padding on each side.
                let inset_x = 1 + self.padding_x;
                let inset_y = 1 + self.padding_y;
                let content_w = rect.width().saturating_sub(2 * inset_x);
                let content_h = rect.height().saturating_sub(2 * inset_y);
                let content = &printer
                    .offset(rect.top_left() + (inset_x, inset_y))
                    .cropped((content_w, content_h))
                    .focused(focused);
                // Wipe the content area before each draw so text from a
                // previous view can't bleed through when views are swapped.
                // The shadow buffer makes this free when content is unchanged.
                for y in 0..content_h {
                    content.print_hline((0, y), content_w, " ");
                }
                pane.top().draw(content);
            }
        }
    }

    fn layout(&mut self, size: Vec2) {
        self.last_size = size;

        let column_count = self.columns.len();
        let mut x = 0;
        for (column_index, column) in self.columns.iter_mut().enumerate() {
            let width = if column_index + 1 == column_count {
                size.x.saturating_sub(x)
            } else {
                size.x * column.width_pct as usize / 100
            };

            let pane_count = column.panes.len();
            let mut y = 0;
            for (pane_index, pane) in column.panes.iter_mut().enumerate() {
                let height = if pane_index + 1 == pane_count {
                    size.y.saturating_sub(y)
                } else if let Some(&pct) = column.height_pcts.get(pane_index) {
                    size.y * pct as usize / 100
                } else {
                    size.y / pane_count
                };
                // Island rect: inset by gap_x/gap_y so adjacent panes have
                // a 2*gap separation and a gap outer margin.
                let ix = x + self.gap_x;
                let iy = y + self.gap_y;
                let iw = width.saturating_sub(2 * self.gap_x);
                let ih = height.saturating_sub(2 * self.gap_y);
                pane.rect = Rect::from_size((ix, iy), (iw, ih));
                // Content lives inside the border (1 cell) + per-axis padding.
                let inset_x = 1 + self.padding_x;
                let inset_y = 1 + self.padding_y;
                pane.top_mut()
                    .layout(Vec2::new(iw.saturating_sub(2 * inset_x), ih.saturating_sub(2 * inset_y)));
                y += height;
            }
            x += width;
        }
    }

    fn needs_relayout(&self) -> bool {
        true
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        if let Event::Mouse {
            position, offset, ..
        } = event
        {
            // Focus the pane under the cursor on click, then forward.
            let local = position.saturating_sub(offset);
            if let Some(target) = self.pane_at(local) {
                let is_press = matches!(
                    event,
                    Event::Mouse {
                        event: MouseEvent::Press(_),
                        ..
                    }
                );
                if target != self.focused && is_press {
                    self.focused = target;
                }
                let rect = self.columns[target.0].panes[target.1].rect;
                let views_count = self.columns[target.0].panes[target.1].views.len();

                // Title-row click: pop stack when one is open, otherwise
                // forward to the view's own title action (e.g. browser dropdown).
                if is_press && local.y == rect.top() {
                    if views_count > 1 {
                        self.pop_focused();
                        return EventResult::consumed();
                    }
                    let pane = &mut self.columns[target.0].panes[target.1];
                    return pane.top_mut().on_title_action();
                }

                let pane = &mut self.columns[target.0].panes[target.1];

                return pane
                    .top_mut()
                    .on_event(event.relativized(rect.top_left() + (1 + self.padding_x, 1 + self.padding_y)));
            }
            return EventResult::Ignored;
        }

        match self.focused_pane_mut() {
            Some(pane) => pane.top_mut().on_event(event),
            None => EventResult::Ignored,
        }
    }

    fn call_on_any(&mut self, selector: &Selector, callback: cursive::event::AnyCb) {
        for column in &mut self.columns {
            for pane in &mut column.panes {
                for view in &mut pane.views {
                    view.call_on_any(selector, callback);
                }
            }
        }
    }

    fn take_focus(&mut self, source: Direction) -> Result<EventResult, CannotFocus> {
        match self.focused_pane_mut() {
            Some(pane) => pane.top_mut().take_focus(source),
            None => Err(CannotFocus),
        }
    }
}

impl ViewExt for PaneLayoutView {
    fn title(&self) -> String {
        "Home".to_string()
    }

    fn on_leave(&self) {
        if let Some(pane) = self.focused_pane() {
            pane.top().on_leave();
        }
    }

    fn on_command(&mut self, s: &mut Cursive, cmd: &Command) -> Result<CommandResult, String> {
        if let Command::FocusPane(direction) = cmd {
            self.move_focus(*direction);
            return Ok(CommandResult::Consumed(None));
        }

        let result = match self.focused_pane_mut() {
            Some(pane) => pane.top_mut().on_command(s, cmd)?,
            None => CommandResult::Ignored,
        };

        // A pane wants to open a view (e.g. a playlist's tracks, an album):
        // try "tracks" pane first, fall back to "browser" for single-column
        // layouts, then let Layout handle it as a fullscreen push.
        if let CommandResult::View(view) = result {
            let mut returned = self.push_to_pane("tracks", view);
            if let Some(v) = returned {
                returned = self.push_to_pane("browser", v);
            }
            return match returned {
                None => Ok(CommandResult::Consumed(None)),
                Some(view) => Ok(CommandResult::View(view)),
            };
        }

        // Back pops the focused pane's stack before bubbling up to Layout.
        if matches!(cmd, Command::Back)
            && matches!(result, CommandResult::Ignored)
            && self.pop_focused()
        {
            return Ok(CommandResult::Consumed(None));
        }

        Ok(result)
    }
}
