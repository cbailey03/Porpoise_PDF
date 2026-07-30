//! The page grid you drag pages around in.
//!
//! Reordering one page at a time from the toolbar is fine for a small fix and tedious
//! for a real reshuffle, which is what this is for.
//!
//! # It reuses the render pipeline rather than adding one
//!
//! A thumbnail is just a page at a small zoom, so it goes through the same worker pool
//! and the same texture cache as the main view, keyed at its own zoom rung. That means
//! no second pipeline to keep correct, no separate cache to budget, and thumbnails are
//! virtualized for free — only the rows on screen are ever requested, so a 400-page
//! document costs about twenty tiny renders rather than four hundred.
//!
//! "The same pipeline" has twice turned out to be less true than this comment claimed,
//! both times because a policy the main view owned was applied to a cache the grid shares:
//!
//! - A page must be allowed **two cached rungs at once**, the main view's and the grid's.
//!   `PageCache::retain_bucket` contradicted that and was deleted; the page-window
//!   predicate that replaced it had the same effect for any page outside the window, which
//!   is what made the last few thumbnails flicker. See [`crate::retain`].
//! - Asking for a render must go through **one** decision, or the grid keeps asking for a
//!   page the main view has given up on. See [`crate::queue`].
//!
//! Both were "reuses the same pipeline" being true of the mechanism and false of the
//! policy, so the pattern is worth naming: anything shared here needs a shared *decision*,
//! not just a shared structure.
//!
//! # Dragging is a gesture, not a command
//!
//! Dropping page 7 onto slot 2 produces `MovePage { from: 7, to: 2 }` — the command that
//! already existed, which the toolbar and the control channel also produce. So this
//! module adds a way to *author* an edit and no new capability, exactly like the file
//! picker authoring an `open`. Whether the panel is showing is a different matter and
//! does get a command, because it changes what is on screen and an agent that opens it
//! can also close it. See `docs/goal-4-plan.md` §7.
//!
//! Clicking a page to *go* to it is the same idea applied to `GoToPage`, and picking
//! several out to move together is the same idea again: ctrl+click, shift+click and a
//! marquee all author a `set_selection`, and dragging the result authors a `move_pages`.
//! Which of those a press means depends on the mode and on where the press began — see
//! [`GridMode`]. Which pages are picked is [`crate::selection`]'s decision, not this
//! module's; all that happens here is turning rectangles into positions.

use eframe::egui;
use eframe::egui::containers::scroll_area::{DragScroll, ScrollSource};
use porpoise_doc::{Document, PageOrder};
use porpoise_view::{CacheKey, PageCache, PageNumber, ZoomBucket};

use crate::queue::RenderQueue;
use crate::selection::{Pick, Selection};
use crate::tiles::FULL_UV;

/// How wide a thumbnail should be, in points of screen space.
///
/// Wide enough that a drawing sheet is recognisable — the point of a grid is deciding
/// *which* page you are looking at — and small enough that a row holds several.
const THUMBNAIL_WIDTH: f32 = 120.0;

/// Space around each thumbnail, for the drop highlight and the page number.
const CELL_PADDING: f32 = 10.0;

/// Height reserved under each thumbnail for its page number.
const LABEL_HEIGHT: f32 = 16.0;

/// What a click in the grid means.
///
/// # Why a mode, and not one gesture that guesses
///
/// A click has to mean one thing at a time, and "go to this page" and "pick this page
/// out" are both reasonable meanings for the same press. Rather than guess, the panel
/// asks: navigate, or reorganize. Then within each mode every gesture is unambiguous,
/// which is the rule the rest of the program already follows — one decision, one
/// producer. See [`crate::edits`] for the same idea applied to the toolbar.
///
/// # How the gestures avoid each other
///
/// Worth writing down, because the first attempt at this did not work at all. A widget
/// that senses only drags is marked dragged the instant the button goes down — with no
/// click to disambiguate there is nothing to wait for — and `dnd_drag_source` then
/// repaints its contents into a tooltip layer, re-parenting the widget inside it, so a
/// click nested in there resolves to an id that has moved and never fires.
///
/// So reorganize mode does not use `dnd_drag_source`. Its cells sense
/// [`egui::Sense::click_and_drag`], which egui explicitly supports by postponing the
/// decision until the pointer has moved far enough or been released, and the drag is
/// carried by [`egui::DragAndDrop`] directly. The cost is the postponement itself: a
/// reorder starts a few pixels of movement in, which is how every drag-and-drop list
/// behaves.
///
/// The marquee is kept off that decision entirely by *where it starts*: a drag beginning
/// on a cell moves pages, and one beginning on empty space draws a selection box. Two
/// gestures that look identical, told apart by their origin rather than by a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GridMode {
    /// Click a page to scroll the main view to it. The default, because finding a page
    /// is what the grid is opened for far more often than reshuffling one.
    #[default]
    Navigate,
    /// Pick pages out and drag them into a new order. See [`crate::selection`].
    Reorganize,
}

impl GridMode {
    /// Every mode, for the tab row and the protocol's error message.
    ///
    /// Kept exhaustive by `every_mode_is_listed`, which matches on each variant and so
    /// fails to compile when one is added — the same mechanism
    /// `Command::shell_commands` uses.
    pub(crate) const EVERY: [Self; 2] = [Self::Navigate, Self::Reorganize];

    /// This mode's name on the wire.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Navigate => "navigate",
            Self::Reorganize => "reorganize",
        }
    }

    /// Every wire name, for an error that has to name the alternatives.
    pub(crate) fn every_name() -> String {
        Self::EVERY
            .iter()
            .map(|mode| mode.wire_name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What the tab says.
    fn label(self) -> &'static str {
        match self {
            Self::Navigate => "Navigation",
            Self::Reorganize => "Reorganize",
        }
    }

    /// What the tab's tooltip says the mode does. Worth spelling out, because the whole
    /// point of the mode is that the same click does two different things — and because
    /// ctrl and shift are not discoverable by clicking about.
    fn hint(self) -> &'static str {
        match self {
            Self::Navigate => "Click a page to go to it",
            Self::Reorganize => {
                "Click to pick a page, ctrl+click for several, shift+click for a range, \
                 or drag a box over empty space. Drag a page to move what is picked."
            }
        }
    }
}

/// Everything the grid needs from the open document.
///
/// Passed in rather than reached for, so this module never sees the viewer's state and
/// stays the only thing that knows how the grid is arranged.
pub(crate) struct Grid<'a> {
    pub(crate) order: &'a PageOrder,
    pub(crate) document: &'a Document,
    pub(crate) cache: &'a mut PageCache<egui::TextureHandle>,
    /// Where a missing thumbnail is asked for.
    ///
    /// The main view's queue, not one of the grid's own — so the two cannot disagree about
    /// when a page has been given up on. See [`crate::queue`].
    pub(crate) queue: RenderQueue<'a>,
    /// Display position of the page the main view is showing, highlighted here.
    pub(crate) current: usize,
    /// What a click does. See [`GridMode`].
    pub(crate) mode: GridMode,
    /// Which pages are picked out. Only drawn, and only read, in
    /// [`GridMode::Reorganize`].
    pub(crate) selection: &'a Selection,
    /// Physical pixels per screen point, so a thumbnail is rasterized at the size it
    /// will actually be drawn.
    pub(crate) pixels_per_point: f32,
}

/// The zoom rung thumbnails are rasterized at.
///
/// Derived from the widest page so that the widest thumbnail lands near
/// [`THUMBNAIL_WIDTH`] and narrower pages come out proportionally smaller — the same
/// rule the main column uses, so a grid of mixed page sizes reads the way the document
/// does. Quantized to a rung because that is what the cache is keyed by.
pub(crate) fn bucket_for(widest_page_pt: f64, pixels_per_point: f32) -> ZoomBucket {
    if !widest_page_pt.is_finite() || widest_page_pt <= 0.0 {
        return ZoomBucket::enclosing(1.0);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "page widths are ordinary page dimensions"
    )]
    let wanted = THUMBNAIL_WIDTH * pixels_per_point / widest_page_pt as f32;
    ZoomBucket::enclosing(wanted)
}

/// How many thumbnails fit across `available` points of width.
///
/// At least one, however narrow the panel: a column of clipped thumbnails is more use
/// than a division by zero.
pub(crate) fn columns_for(available: f32) -> usize {
    let cell = THUMBNAIL_WIDTH + CELL_PADDING * 2.0;
    if !available.is_finite() || available < cell {
        return 1;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded above by the panel width in points"
    )]
    let columns = (available / cell) as usize;
    columns.max(1)
}

/// How many rows `pages` needs at `columns` across.
pub(crate) fn rows_for(pages: usize, columns: usize) -> usize {
    if columns == 0 {
        return pages;
    }
    pages.div_ceil(columns)
}

/// What the grid did this frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Drawn {
    /// A move, if one was dropped: which display positions, and where the group goes.
    ///
    /// Nothing is changed here — the caller turns this into a command, so a drag goes
    /// through the same dispatch as every other edit.
    pub(crate) moved: Option<(Vec<usize>, usize)>,
    /// A click on a thumbnail in reorganize mode, and what it was asking for.
    ///
    /// The position, not the resulting set: what ctrl+click *does* to a selection is
    /// [`crate::selection`]'s decision, and this module would only get a second opinion
    /// wrong. Same reason the drag reports a position rather than a new order.
    pub(crate) picked: Option<(usize, Pick)>,
    /// Display positions a finished marquee covered, replacing the selection.
    ///
    /// `Some(empty)` is meaningful and different from `None`: dragging a box over blank
    /// space is how you deselect everything.
    pub(crate) marquee: Option<Vec<usize>>,
    /// A thumbnail that was clicked, in display position. Only ever set in
    /// [`GridMode::Navigate`].
    ///
    /// Turned into `GoToPage` by the caller, same reasoning as `moved`.
    pub(crate) navigated: Option<usize>,
    /// A mode tab that was clicked, if it was not the one already showing.
    ///
    /// A command too, for the reason the panel's own visibility is one: it changes what
    /// is on screen, and anything that can be entered has to be leavable.
    pub(crate) mode: Option<GridMode>,
    /// Source pages the grid has on screen.
    ///
    /// Reported because the caller decides eviction and the texture cache has two
    /// consumers; see [`crate::retain`], which exists because this was not reported and
    /// the grid's own thumbnails were being evicted out from under it.
    pub(crate) showing: Vec<usize>,
}

/// Draws the grid, reporting what it drew.
pub(crate) fn draw(ui: &mut egui::Ui, grid: &mut Grid<'_>) -> Drawn {
    // Above the scroll area rather than inside it, so the tabs stay put while the grid
    // scrolls under them — and so the mode is still switchable with no pages to show.
    let mut drawn = Drawn {
        mode: tabs(ui, grid.mode),
        ..Drawn::default()
    };
    ui.separator();

    let pages = grid.order.len();
    if pages == 0 {
        ui.label("no pages");
        return drawn;
    }

    let widest = grid
        .document
        .geometry()
        .iter()
        .map(|page| f64::from(page.width_pt))
        .fold(0.0_f64, f64::max);
    let bucket = bucket_for(widest, grid.pixels_per_point);
    let columns = columns_for(ui.available_width());
    let rows = rows_for(pages, columns);
    let cell_height = THUMBNAIL_WIDTH * 1.4 + LABEL_HEIGHT + CELL_PADDING * 2.0;

    // `show_rows` only calls back for the rows on screen, which is what keeps a
    // 400-page grid from rasterizing 400 thumbnails — and what makes `showing` a
    // viewport-sized list rather than a document-sized one.
    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        // Drag-to-scroll would be a third meaning for a drag on empty space, and it is
        // the marquee's. Off explicitly rather than relying on the default, which only
        // happens to be off because this is not a touch screen.
        .scroll_source(ScrollSource {
            drag: DragScroll::Never,
            ..ScrollSource::default()
        })
        .show_rows(ui, cell_height, rows, |ui, row_range| {
            // What the marquee measures against: the visible part of the scroll area,
            // which excludes the scroll bar, so a drag on that is not a selection.
            let viewport = ui.clip_rect();
            let mut cells: Vec<(usize, egui::Rect)> = Vec::new();

            for row in row_range {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let position = row * columns + column;
                        if position >= pages {
                            break;
                        }
                        let outcome = cell(ui, grid, position, bucket);
                        if let Some(dropped) = outcome.dropped {
                            drawn.moved = Some((dropped, position));
                        }
                        if outcome.clicked {
                            drawn.navigated = Some(position);
                        }
                        if let Some(pick) = outcome.picked {
                            drawn.picked = Some((position, pick));
                        }
                        cells.push((position, outcome.rect));
                        // The source page, because that is what the cache is keyed by.
                        drawn.showing.extend(grid.order.source_of(position));
                    }
                });
            }

            // After the cells, because it needs their rectangles for both jobs: telling a
            // marquee from a page drag, and working out what the box covers.
            if grid.mode == GridMode::Reorganize
                && let Some(box_) = marquee(ui, viewport, &cells)
            {
                let covered = covered_by(&box_.rect, &cells);
                paint_marquee(ui, &box_.rect, &covered, &cells);
                if box_.finished {
                    drawn.marquee = Some(covered);
                }
            }
        });

    drawn
}

/// The mode tabs. Returns the mode asked for, if it is not the one already showing.
fn tabs(ui: &mut egui::Ui, current: GridMode) -> Option<GridMode> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        for mode in GridMode::EVERY {
            if ui
                .selectable_label(mode == current, mode.label())
                .on_hover_text(mode.hint())
                .clicked()
                && mode != current
            {
                chosen = Some(mode);
            }
        }
    });
    chosen
}

/// What one cell reported.
#[derive(Debug, Clone)]
struct CellOutcome {
    /// The positions dragged from, if a drop landed here this frame.
    dropped: Option<Vec<usize>>,
    /// Whether this thumbnail was clicked in navigation mode.
    clicked: bool,
    /// What a click in reorganize mode was asking of the selection.
    picked: Option<Pick>,
    /// Where the cell was drawn. Needed after the fact for the marquee, which has to
    /// tell a drag that began on a page from one that began on empty space.
    rect: egui::Rect,
}

impl Default for CellOutcome {
    /// Nothing happened, and nothing was drawn.
    ///
    /// Hand-written because `egui::Rect` has no `Default`, and [`egui::Rect::NOTHING`] is
    /// the right blank anyway: it is empty, so a marquee cannot start "on" it and it
    /// intersects no box.
    fn default() -> Self {
        Self {
            dropped: None,
            clicked: false,
            picked: None,
            rect: egui::Rect::NOTHING,
        }
    }
}

/// One thumbnail, wired up for whichever mode the panel is in.
fn cell(
    ui: &mut egui::Ui,
    grid: &mut Grid<'_>,
    position: usize,
    bucket: ZoomBucket,
) -> CellOutcome {
    let Some(page) = grid.order.source_of(position) else {
        return CellOutcome::default();
    };
    let geometry = grid.document.geometry().get(page).copied();
    let size = match geometry {
        Some(page) if page.width_pt > 0.0 && page.height_pt > 0.0 => {
            let scale = THUMBNAIL_WIDTH / page.width_pt;
            egui::vec2(THUMBNAIL_WIDTH, page.height_pt * scale)
        }
        // A degenerate page still needs a slot, or the grid's positions would stop
        // matching the document's.
        _ => egui::vec2(THUMBNAIL_WIDTH, THUMBNAIL_WIDTH),
    };

    let key = CacheKey::new(page, bucket);
    let texture = grid.cache.get(key).map(egui::TextureHandle::id);
    if texture.is_none() {
        grid.queue.want(key, grid.pixels_per_point);
    }

    let selected =
        grid.mode == GridMode::Reorganize && grid.selection.contains_position(grid.order, position);
    let thumbnail = Thumbnail {
        position,
        size,
        texture,
        current: position == grid.current,
        selected,
    };

    // The one place the mode is read. Everything else about a cell is the same either
    // way, which is why the modes differ by what wraps `paint` and nothing more.
    match grid.mode {
        GridMode::Navigate => {
            let response = egui::Frame::default()
                .show(ui, |ui| paint(ui, &thumbnail, egui::Sense::click()))
                .inner;
            CellOutcome {
                rect: response.rect,
                clicked: response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked(),
                ..CellOutcome::default()
            }
        }
        GridMode::Reorganize => {
            let response = egui::Frame::default()
                .show(ui, |ui| {
                    paint(ui, &thumbnail, egui::Sense::click_and_drag())
                })
                .inner;

            // What travels when the drag starts. A drag from a page that is not picked
            // out takes that page alone — and the click reported alongside makes it the
            // selection, so the highlight always matches what is moving.
            let mut picked = None;
            if response.drag_started() {
                let group = if selected {
                    grid.selection.positions(grid.order)
                } else {
                    picked = Some(Pick::Only);
                    vec![position]
                };
                egui::DragAndDrop::set_payload(ui.ctx(), Dragged(group));
            }

            // Read before the click, because a release that completes a drag must not
            // also register as a pick on whatever it landed on.
            let dropped = response.dnd_release_payload::<Dragged>();
            if dropped.is_none() && response.clicked() {
                let (toggle, range) =
                    ui.input(|i| (i.modifiers.command || i.modifiers.ctrl, i.modifiers.shift));
                picked = Some(Pick::of(toggle, range));
            }

            CellOutcome {
                rect: response.rect,
                dropped: dropped.map(|group| group.0.clone()),
                clicked: false,
                picked,
            }
        }
    }
}

/// The pages in flight during a drag, as display positions.
///
/// A newtype rather than a bare `Vec<usize>` because the payload is looked up *by type*:
/// anything else in the program that dragged a list of numbers would otherwise be
/// accepted here as a page move.
#[derive(Debug, Clone)]
struct Dragged(Vec<usize>);

/// A marquee in progress.
struct Marquee {
    /// The box, in screen coordinates.
    rect: egui::Rect,
    /// Whether the pointer came up this frame, which is when the selection is committed.
    finished: bool,
}

/// The selection box being dragged, if one is.
///
/// Told apart from a page drag by where the press began rather than by which widget
/// claimed it: raw pointer state, and then the cell rectangles to rule out a press that
/// landed on a page. Doing it this way means the cells and the marquee never compete for
/// the same press, so neither has to win a hit test.
fn marquee(ui: &egui::Ui, viewport: egui::Rect, cells: &[(usize, egui::Rect)]) -> Option<Marquee> {
    // A page is in flight, so this drag is a move and not a selection.
    if egui::DragAndDrop::has_any_payload(ui.ctx()) {
        return None;
    }

    let (origin, latest, down, released) = ui.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.pointer.any_released(),
        )
    });
    if !(down || released) {
        return None;
    }
    let origin = origin?;
    let latest = latest?;
    if !viewport.contains(origin) {
        return None;
    }
    // The gesture that starts on a page is that page's, not the marquee's.
    if cells.iter().any(|(_, rect)| rect.contains(origin)) {
        return None;
    }

    Some(Marquee {
        rect: egui::Rect::from_two_pos(origin, latest),
        finished: released,
    })
}

/// Display positions whose thumbnail the box touches.
///
/// Intersection rather than containment: dragging a box that clips the corner of a page
/// is asking for that page, and requiring full enclosure makes a selection near the edge
/// of the panel impossible.
fn covered_by(box_: &egui::Rect, cells: &[(usize, egui::Rect)]) -> Vec<usize> {
    cells
        .iter()
        .filter(|(_, rect)| rect.intersects(*box_))
        .map(|(position, _)| *position)
        .collect()
}

/// Paints the box and tints what it is about to take.
///
/// Live rather than on release, because a selection you cannot see until you let go is
/// one you have to redo. The tint is painted over the cells that are already drawn, which
/// is the whole reason this runs after them.
fn paint_marquee(
    ui: &egui::Ui,
    box_: &egui::Rect,
    covered: &[usize],
    cells: &[(usize, egui::Rect)],
) {
    let painter = ui.painter();
    let accent = ui.visuals().selection.bg_fill;
    for (position, rect) in cells {
        if covered.contains(position) {
            painter.rect_filled(*rect, 2.0, accent.gamma_multiply(0.35));
        }
    }
    painter.rect_filled(*box_, 0.0, accent.gamma_multiply(0.15));
    painter.rect_stroke(
        *box_,
        0.0,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Inside,
    );
}

/// One thumbnail's appearance, separated from how it is interacted with.
struct Thumbnail {
    /// Display position, which is what the page number under it counts.
    position: usize,
    size: egui::Vec2,
    /// `None` until the render lands.
    texture: Option<egui::TextureId>,
    /// Whether this is the page the main view is showing.
    current: bool,
    /// Whether this page is picked out. Only ever true in reorganize mode.
    selected: bool,
}

/// Paints a thumbnail and its page number, and returns the image's own response.
///
/// Shared by both modes, so a page reads the same whichever tab is up. Not quite
/// pixel-identical: `dnd_drop_zone` fills its frame in reorganize mode, which is what
/// puts a panel of boxes behind the thumbnails there. That is the drop target showing
/// itself, so it is left alone.
fn paint(ui: &mut egui::Ui, thumbnail: &Thumbnail, sense: egui::Sense) -> egui::Response {
    ui.vertical(|ui| {
        let (rect, response) = ui.allocate_exact_size(thumbnail.size, sense);
        match thumbnail.texture {
            Some(texture) => {
                ui.painter()
                    .image(texture, rect, FULL_UV, egui::Color32::WHITE);
            }
            None => {
                // The same placeholder the main view uses, for the same reason: a grey
                // box that becomes a page beats the grid reflowing once each thumbnail
                // arrives.
                ui.painter()
                    .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
            }
        }
        // Picked out: tinted over the page rather than outlined, so it stays legible when
        // twelve in a row are selected — twelve outlines read as a grid, a wash of colour
        // reads as a group.
        if thumbnail.selected {
            let accent = ui.visuals().selection.bg_fill;
            ui.painter()
                .rect_filled(rect, 2.0, accent.gamma_multiply(0.35));
            ui.painter().rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(2.0, accent),
                egui::StrokeKind::Inside,
            );
        }
        // Where the main view is, outlined as well as numbered in colour, because a
        // coloured number is easy to miss in a panel of forty thumbnails. White rather
        // than the accent so it is still tellable from a selected page.
        if thumbnail.current {
            ui.painter().rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(2.0, egui::Color32::WHITE),
                egui::StrokeKind::Inside,
            );
        }
        // The page number under each one, because the whole job of the grid is telling
        // you which page you are looking at or about to move.
        let number = PageNumber::from_index(thumbnail.position);
        if thumbnail.current {
            ui.colored_label(ui.visuals().selection.bg_fill, format!("{number}"));
        } else {
            ui.label(format!("{number}"));
        }
        response
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrow_panel_still_shows_one_column() {
        // Rather than dividing by zero or showing nothing.
        assert_eq!(columns_for(0.0), 1);
        assert_eq!(columns_for(10.0), 1);
        assert_eq!(columns_for(f32::NAN), 1);
    }

    #[test]
    fn a_wider_panel_shows_more_columns() {
        let cell = THUMBNAIL_WIDTH + CELL_PADDING * 2.0;
        assert_eq!(columns_for(cell), 1);
        assert_eq!(columns_for(cell * 3.0), 3);
        assert!(columns_for(cell * 10.0) >= 10);
    }

    #[test]
    fn rows_cover_every_page() {
        // A page that fell off the last row would be unreachable by drag, so the
        // rounding has to go up.
        for pages in [1_usize, 2, 5, 6, 7, 400] {
            for columns in 1..=5 {
                let rows = rows_for(pages, columns);
                assert!(
                    rows * columns >= pages,
                    "{pages} pages in {columns} columns needs more than {rows} rows"
                );
                assert!(
                    (rows.saturating_sub(1)) * columns < pages,
                    "{pages} pages in {columns} columns wastes a row at {rows}"
                );
            }
        }
    }

    #[test]
    fn no_columns_does_not_lose_pages() {
        // Defensive: `columns_for` never returns zero, but dividing by one that did
        // would panic, and losing pages silently would be worse.
        assert_eq!(rows_for(7, 0), 7);
    }

    #[test]
    fn a_thumbnail_rung_is_far_below_full_size() {
        // A 1224 pt drawing sheet at 120 pt wide is about a tenth scale. If this ever
        // came out near 1.0, the grid would rasterize full-size pages and a 400-page
        // document would be unusable.
        let bucket = bucket_for(1224.0, 1.0);
        assert!(
            bucket.scale() < 0.2,
            "thumbnail scale was {}",
            bucket.scale()
        );
        assert!(bucket.scale() > 0.0);
    }

    #[test]
    fn a_narrow_page_gets_a_bigger_rung_than_a_wide_one() {
        // Thumbnails are sized from the *widest* page so mixed documents stay
        // proportional, which means a document of small pages zooms in more.
        let wide = bucket_for(1224.0, 1.0).scale();
        let narrow = bucket_for(200.0, 1.0).scale();
        assert!(narrow > wide, "narrow {narrow} should exceed wide {wide}");
    }

    #[test]
    fn a_degenerate_page_width_does_not_produce_a_nonsense_rung() {
        for width in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let scale = bucket_for(width, 1.0).scale();
            assert!(scale.is_finite() && scale > 0.0, "width {width} -> {scale}");
        }
    }

    /// Four cells in a row, each 100 wide with a 10 pt gap, 140 tall.
    fn row() -> Vec<(usize, egui::Rect)> {
        (0..4)
            .map(|position| {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "four positions in a test fixture"
                )]
                let x = position as f32 * 110.0;
                (
                    position,
                    egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(100.0, 140.0)),
                )
            })
            .collect()
    }

    #[test]
    fn a_box_takes_the_cells_it_touches() {
        // Spans the first two and stops in the gap before the third.
        let box_ = egui::Rect::from_min_max(egui::pos2(5.0, 5.0), egui::pos2(205.0, 100.0));
        assert_eq!(covered_by(&box_, &row()), vec![0, 1]);
    }

    #[test]
    fn clipping_a_corner_is_enough() {
        // Intersection rather than containment: requiring a cell to be wholly inside
        // makes selecting anything at the edge of a narrow panel impossible.
        let box_ = egui::Rect::from_min_max(egui::pos2(95.0, 135.0), egui::pos2(115.0, 200.0));
        assert_eq!(covered_by(&box_, &row()), vec![0, 1]);
    }

    #[test]
    fn a_box_over_empty_space_takes_nothing() {
        // How you deselect, so it must come back empty rather than unchanged.
        let box_ = egui::Rect::from_min_max(egui::pos2(0.0, 300.0), egui::pos2(400.0, 400.0));
        assert_eq!(covered_by(&box_, &row()), Vec::<usize>::new());
    }

    #[test]
    fn a_box_dragged_upwards_still_selects() {
        // `Rect::from_two_pos` normalizes, but this is what would break if a future
        // change built the rect by hand from min/max — and dragging up-and-left is at
        // least half of all drags.
        let box_ = egui::Rect::from_two_pos(egui::pos2(205.0, 100.0), egui::pos2(5.0, 5.0));
        assert_eq!(covered_by(&box_, &row()), vec![0, 1]);
    }

    #[test]
    fn a_box_covering_everything_takes_every_cell() {
        let box_ = egui::Rect::from_min_max(egui::pos2(-10.0, -10.0), egui::pos2(500.0, 500.0));
        assert_eq!(covered_by(&box_, &row()), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_cell_that_was_never_drawn_is_never_covered() {
        // `Rect::NOTHING` is what a skipped cell reports, and a marquee must not pick it
        // up — it would select a page that is not on screen.
        let cells = vec![(7_usize, egui::Rect::NOTHING)];
        let box_ = egui::Rect::from_min_max(egui::pos2(-1e6, -1e6), egui::pos2(1e6, 1e6));
        assert_eq!(covered_by(&box_, &cells), Vec::<usize>::new());
    }

    #[test]
    fn every_mode_is_listed() {
        // The enforcement, same as `Command::shell_commands`: a variant added without
        // being put in `EVERY` fails to compile here, and an unlisted mode would be one
        // with no tab — reachable over the wire and not by hand.
        for mode in GridMode::EVERY {
            match mode {
                GridMode::Navigate | GridMode::Reorganize => {}
            }
        }
        assert_eq!(GridMode::EVERY.len(), 2);
    }

    #[test]
    fn the_grid_opens_in_navigation_mode() {
        // Finding a page is the common reason to open the panel; reshuffling is the rare
        // one. A wrong default here is a click that silently reorders the document.
        assert_eq!(GridMode::default(), GridMode::Navigate);
    }

    #[test]
    fn every_mode_has_a_distinct_wire_name_and_label() {
        let mut names: Vec<&str> = GridMode::EVERY.iter().map(|m| m.wire_name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two modes share a wire name");

        let mut labels: Vec<&str> = GridMode::EVERY.iter().map(|m| m.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total, "two tabs read the same");
    }

    #[test]
    fn the_error_message_names_every_mode() {
        // An agent that guessed wrong has no other way to find out what to say.
        let listed = GridMode::every_name();
        for mode in GridMode::EVERY {
            assert!(
                listed.contains(mode.wire_name()),
                "{listed:?} does not mention {}",
                mode.wire_name()
            );
        }
    }

    #[test]
    fn a_mode_round_trips_through_its_wire_name() {
        for mode in GridMode::EVERY {
            let json = serde_json::to_value(mode).expect("a mode serializes");
            assert_eq!(json, serde_json::Value::from(mode.wire_name()));
            let back: GridMode = serde_json::from_value(json).expect("and comes back");
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn a_high_dpi_screen_asks_for_more_pixels() {
        // The thumbnail is drawn at a fixed size in points, so a 2x screen needs twice
        // the pixels or it looks soft.
        let single = bucket_for(1224.0, 1.0).scale();
        let double = bucket_for(1224.0, 2.0).scale();
        assert!(double > single, "2x {double} should exceed 1x {single}");
    }
}
