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

/// Height reserved under each thumbnail for its page number.
///
/// Allocated rather than measured, so a row's height is arithmetic this module can state
/// up front. [`row_height`] explains why that matters.
const LABEL_HEIGHT: f32 = 16.0;

/// Tallest a thumbnail box may be, as a multiple of its width.
///
/// The box fits the tallest page in the document, so a set of landscape drawing sheets
/// gets short rows and a set of portrait pages gets taller ones — both packed tight. This
/// caps it, because one freak page in an otherwise ordinary document would otherwise make
/// every row as tall as that page needs. Anything past the cap is scaled down to fit.
///
/// 1.7 clears US Legal at 1.647, which is the tallest ordinary paper size.
const MAX_THUMBNAIL_ASPECT: f32 = 1.7;

/// egui's default `item_spacing`, used only to pick the panel's *opening* width.
///
/// The live column count is measured from the real spacing every frame, so if a style
/// changes this the cost is an initial width slightly off and nothing worse.
const ASSUMED_GAP: f32 = 8.0;

/// Room left for the scroll bar when choosing the opening width.
const SCROLL_BAR_ALLOWANCE: f32 = 16.0;

/// How wide the page grid panel opens: two columns, snug, plus the scroll bar.
///
/// Derived from the thumbnail width rather than written down separately, so the panel
/// cannot end up sized for a number of columns that no longer fit. It used to be a flat
/// 300, which reserved 140 per column while drawing 128 — leaving a 40 pt strip of dead
/// panel too narrow to hold a third column.
pub(crate) const PANEL_WIDTH: f32 = THUMBNAIL_WIDTH * 2.0 + ASSUMED_GAP + SCROLL_BAR_ALLOWANCE;

/// How far the pointer must travel before a drag on empty space is a selection box.
///
/// Without this, a *stationary* click counts as a marquee of zero size, and a click that
/// lands in the gap between two thumbnails throws the selection away. That is a near-miss
/// punishing you for missing by four pixels, which is the worst kind of bug to hit while
/// carefully building a selection — so below this distance nothing happens at all.
///
/// Note what it does *not* do: a deliberate click on blank space no longer deselects. In a
/// file browser it would, but here a selection can cost a dozen clicks to build and the
/// only ways to lose it are now explicit — pick another page, or leave the tab.
const MARQUEE_MINIMUM_DRAG: f32 = 4.0;

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

/// How many thumbnails fit across `available` points of width, `gap` apart.
///
/// The gap is passed in rather than assumed, because it has to be the spacing egui will
/// actually put between them: reserving more per column than gets drawn is what left a
/// strip of dead panel on the right, too narrow to hold another column.
///
/// At least one, however narrow the panel: a column of clipped thumbnails is more use
/// than a division by zero.
pub(crate) fn columns_for(available: f32, gap: f32) -> usize {
    let column = THUMBNAIL_WIDTH + gap.max(0.0);
    if !available.is_finite() || column <= 0.0 || available < THUMBNAIL_WIDTH {
        return 1;
    }
    // The last column needs no gap after it, so add one back before dividing.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded above by the panel width in points"
    )]
    let columns = ((available + gap.max(0.0)) / column) as usize;
    columns.max(1)
}

/// The uniform box every thumbnail is drawn inside, in points.
///
/// Tall enough for the tallest page in the document and no taller, so a set of landscape
/// drawing sheets packs into short rows instead of reserving portrait-shaped ones. Capped
/// by [`MAX_THUMBNAIL_ASPECT`]; a page past the cap is scaled down to fit rather than
/// stretching every row in the document.
///
/// Uniform on purpose. `ScrollArea::show_rows` needs *one* row height for the whole grid,
/// so a box that varied per page would put the declared height and the drawn height back
/// out of step — see [`row_height`].
pub(crate) fn box_height(page_aspects: impl Iterator<Item = f32>) -> f32 {
    let tallest = page_aspects
        .filter(|aspect| aspect.is_finite() && *aspect > 0.0)
        .fold(0.0_f32, f32::max);
    if tallest <= 0.0 {
        // No usable geometry — a square box still gives every page a slot, which is what
        // keeps the grid's positions matching the document's.
        return THUMBNAIL_WIDTH;
    }
    (THUMBNAIL_WIDTH * tallest).min(THUMBNAIL_WIDTH * MAX_THUMBNAIL_ASPECT)
}

/// How tall one row of the grid is, excluding the spacing egui puts *between* rows.
///
/// This is the number `ScrollArea::show_rows` is given, and it has to be the height a row
/// really occupies. That is not a detail: `show_rows` decides how many rows to draw by
/// dividing the viewport by this, so declaring more than gets drawn renders too few rows
/// and leaves the bottom of the panel empty. It was declared as
/// `THUMBNAIL_WIDTH * 1.4 + LABEL_HEIGHT + 20` — about 204 pt — while a landscape drawing
/// sheet draws roughly 102, so a panel with room for eight rows showed five.
///
/// `gap` is the spacing between the thumbnail and its page number, which is inside the
/// row; the spacing between rows is added by `show_rows` itself.
pub(crate) fn row_height(box_height: f32, gap: f32) -> f32 {
    box_height + gap.max(0.0) + LABEL_HEIGHT
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
    // Measured, not assumed: both of these have to match what egui will really lay out.
    // See `columns_for` and `row_height` for what went wrong when they did not.
    let gap = ui.spacing().item_spacing;
    let columns = columns_for(ui.available_width(), gap.x);
    let rows = rows_for(pages, columns);
    let box_height = box_height(
        grid.document
            .geometry()
            .iter()
            .map(|page| page.height_pt / page.width_pt),
    );
    let cell_height = row_height(box_height, gap.y);

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
                        let outcome = cell(ui, grid, position, bucket, box_height);
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

    // Last, and outside the scroll area, so the card is not clipped by it.
    if grid.mode == GridMode::Reorganize {
        paint_drag_ghost(ui.ctx());
    }

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
    box_height: f32,
) -> CellOutcome {
    let Some(page) = grid.order.source_of(position) else {
        return CellOutcome::default();
    };
    let geometry = grid.document.geometry().get(page).copied();
    // Every cell takes the same box, so the row height declared to `show_rows` is the
    // height really drawn. The page is fitted inside it, keeping its own shape.
    let size = egui::vec2(THUMBNAIL_WIDTH, box_height);
    let page_size = match geometry {
        Some(page) if page.width_pt > 0.0 && page.height_pt > 0.0 => {
            let scale = (THUMBNAIL_WIDTH / page.width_pt).min(box_height / page.height_pt);
            egui::vec2(page.width_pt * scale, page.height_pt * scale)
        }
        // A degenerate page still needs a slot, or the grid's positions would stop
        // matching the document's.
        _ => egui::vec2(THUMBNAIL_WIDTH, box_height),
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
        page_size,
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
                .inner
                // The grab hand, which `dnd_drag_source` used to provide and hand-rolling
                // the drag took away. Its own affordance matters more here than usual:
                // nothing else about a thumbnail says it can be picked up.
                .on_hover_cursor(egui::CursorIcon::Grab);

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
            if response.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }

            // Where the group would land, marked on the cell the pointer is over. Without
            // it a drag is a ghost floating over an unchanged grid, with nothing saying
            // which slot is about to take it.
            if let Some(carried) = response.dnd_hover_payload::<Dragged>() {
                if carried.0.contains(&position) {
                    // Dropping the group onto itself changes nothing, so it must not
                    // advertise that it would.
                    ui.ctx().set_cursor_icon(egui::CursorIcon::NoDrop);
                } else {
                    paint_insertion(ui, response.rect);
                }
            }

            // Read before the click, because a release that completes a drag must not
            // also register as a pick on whatever it landed on.
            let dropped = response.dnd_release_payload::<Dragged>();
            // A drop onto one of the pages being carried is not a move. Filtered here as
            // well as in `PageOrder`, so no command is produced at all rather than one
            // that comes back `unchanged`.
            let dropped = dropped.filter(|group| !group.0.contains(&position));
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

/// Marks the slot a drop would land in, down the leading edge of a cell.
///
/// A bar rather than a tint, because a tint is what "selected" already means here and two
/// meanings for one colour is how a person stops trusting either.
fn paint_insertion(ui: &egui::Ui, cell: egui::Rect) {
    let accent = ui.visuals().selection.bg_fill;
    let bar = egui::Rect::from_min_max(
        egui::pos2(cell.left() - 3.0, cell.top()),
        egui::pos2(cell.left(), cell.bottom()),
    );
    ui.painter().rect_filled(bar, 1.0, accent);
}

/// Paints what is being carried, following the pointer.
///
/// `dnd_drag_source` used to do this by repainting the cell into a tooltip layer — the
/// same mechanism that made a nested click never fire, so it is not coming back. A card
/// saying how many pages are in the air does the job it was doing, and says something the
/// old ghost could not: that a *group* is moving, not the one page under the cursor.
fn paint_drag_ghost(ctx: &egui::Context) {
    let Some(carried) = egui::DragAndDrop::payload::<Dragged>(ctx) else {
        return;
    };
    let Some(pointer) = ctx.pointer_latest_pos() else {
        return;
    };

    let count = carried.0.len();
    let text = if count == 1 {
        "1 page".to_owned()
    } else {
        format!("{count} pages")
    };

    // Its own foreground layer, so the card is above the panel and the page column rather
    // than under whichever drew last.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("porpoise-drag-ghost"),
    ));
    let galley =
        painter.layout_no_wrap(text, egui::FontId::proportional(13.0), egui::Color32::WHITE);
    // Offset down and right of the cursor, the way a drag cursor's own label sits, so the
    // card never covers the slot you are aiming at.
    let card = egui::Rect::from_min_size(
        pointer + egui::vec2(14.0, 10.0),
        galley.size() + egui::vec2(16.0, 10.0),
    );
    painter.rect_filled(card, 4.0, egui::Color32::from_black_alpha(220));
    painter.galley(
        card.center() - galley.size() * 0.5,
        galley,
        egui::Color32::WHITE,
    );
}

/// A marquee in progress.
struct Marquee {
    /// The box, in screen coordinates.
    rect: egui::Rect,
    /// Whether the pointer came up this frame, which is when the selection is committed.
    finished: bool,
}

/// Where the box being dragged started, remembered across frames.
///
/// It has to be remembered, and that is not a detail worth rediscovering. egui sets
/// `press_origin` back to `None` in the same input pass that reports the release — see
/// `InputState`, which clears it right where it pushes `PointerEvent::Released`. So the
/// one frame that has to *commit* the selection is exactly the frame that can no longer
/// say where the drag began. Reading it fresh every frame drew the box perfectly all the
/// way through the drag and then selected nothing at all when you let go.
fn marquee_origin() -> egui::Id {
    egui::Id::new("porpoise-marquee-origin")
}

/// The selection box being dragged, if one is.
///
/// Told apart from a page drag by where the press began rather than by which widget
/// claimed it: raw pointer state, and then the cell rectangles to rule out a press that
/// landed on a page. Doing it this way means the cells and the marquee never compete for
/// the same press, so neither has to win a hit test.
fn marquee(ui: &egui::Ui, viewport: egui::Rect, cells: &[(usize, egui::Rect)]) -> Option<Marquee> {
    let id = marquee_origin();
    let forget = || ui.ctx().data_mut(|data| data.remove_temp::<egui::Pos2>(id));

    // A page is in flight, so this drag is a move and not a selection.
    if egui::DragAndDrop::has_any_payload(ui.ctx()) {
        forget();
        return None;
    }

    let (pressed_at, latest, down, released) = ui.input(|i| {
        (
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.pointer.any_released(),
        )
    });

    let remembered: Option<egui::Pos2> = ui.ctx().data(|data| data.get_temp(id));
    let origin = match remembered {
        // Already tracking one, so the origin comes from memory rather than from the
        // pointer — which is the whole point.
        Some(origin) => origin,
        None => {
            // Only a fresh press can start one.
            if !down {
                return None;
            }
            let origin = pressed_at?;
            if !viewport.contains(origin) {
                return None;
            }
            // The gesture that starts on a page is that page's, not the marquee's.
            if cells.iter().any(|(_, rect)| rect.contains(origin)) {
                return None;
            }
            ui.ctx().data_mut(|data| data.insert_temp(id, origin));
            origin
        }
    };

    // The pointer left without a release ever being seen — a lost window, say. Nothing to
    // commit, and the origin must not outlive the gesture.
    if !down && !released {
        forget();
        return None;
    }

    let latest = latest.unwrap_or(origin);
    let travelled = (latest - origin).length() >= MARQUEE_MINIMUM_DRAG;
    if released {
        forget();
    }
    // A press that never travelled is not a box. See [`MARQUEE_MINIMUM_DRAG`].
    if !travelled {
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
    /// The uniform box the cell occupies. Same for every page in the document, which is
    /// what makes the row height statable — see [`row_height`].
    size: egui::Vec2,
    /// The page itself inside that box, at its own shape. Smaller than `size` for any
    /// page that is not the tallest in the document.
    page_size: egui::Vec2,
    /// `None` until the render lands.
    texture: Option<egui::TextureId>,
    /// Whether this is the page the main view is showing.
    current: bool,
    /// Whether this page is picked out. Only ever true in reorganize mode.
    selected: bool,
}

/// Paints a thumbnail and its page number, and returns the response of its box.
///
/// Shared by both modes, so a page reads the same whichever tab is up.
///
/// Everything here is allocated at a stated size — the box, and the page number under it —
/// because [`row_height`] promises `show_rows` a height and this is what has to keep that
/// promise. Nothing may size itself to its content.
fn paint(ui: &mut egui::Ui, thumbnail: &Thumbnail, sense: egui::Sense) -> egui::Response {
    ui.vertical(|ui| {
        let (box_rect, response) = ui.allocate_exact_size(thumbnail.size, sense);
        // The page centred in its box, at its own shape. A landscape sheet in a portrait
        // document sits in the middle of the slot rather than being stretched to fill it.
        let rect = egui::Rect::from_center_size(box_rect.center(), thumbnail.page_size);
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
        //
        // Allocated at [`LABEL_HEIGHT`] and painted, rather than `ui.label`, which would
        // size itself to the font and put the row's real height back out of step with what
        // `show_rows` was told.
        let (label_rect, _) = ui.allocate_exact_size(
            egui::vec2(THUMBNAIL_WIDTH, LABEL_HEIGHT),
            egui::Sense::hover(),
        );
        let number = PageNumber::from_index(thumbnail.position);
        let colour = if thumbnail.current {
            ui.visuals().selection.bg_fill
        } else {
            ui.visuals().text_color()
        };
        ui.painter().text(
            label_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            format!("{number}"),
            egui::FontId::proportional(12.0),
            colour,
        );
        response
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    /// egui's default spacing, which is what the grid is laid out with.
    const GAP: f32 = 8.0;

    #[test]
    fn a_narrow_panel_still_shows_one_column() {
        // Rather than dividing by zero or showing nothing.
        assert_eq!(columns_for(0.0, GAP), 1);
        assert_eq!(columns_for(10.0, GAP), 1);
        assert_eq!(columns_for(f32::NAN, GAP), 1);
        // A nonsense gap degrades to no gap rather than to no grid, which is the useful
        // way round: `f32::max` discards the NaN, so the columns just sit flush.
        assert_eq!(columns_for(300.0, f32::NAN), 2);
        assert!(columns_for(300.0, -50.0) >= 1);
    }

    #[test]
    fn a_wider_panel_shows_more_columns() {
        // A column is the thumbnail plus the gap after it, and the last column needs no
        // gap — so three columns fit in three thumbnails plus two gaps.
        assert_eq!(columns_for(THUMBNAIL_WIDTH, GAP), 1);
        assert_eq!(columns_for(THUMBNAIL_WIDTH * 3.0 + GAP * 2.0, GAP), 3);
        assert!(columns_for((THUMBNAIL_WIDTH + GAP) * 10.0, GAP) >= 10);
    }

    #[test]
    fn the_panel_opens_wide_enough_for_two_columns() {
        // The regression this width exists for: it used to reserve 140 points per column
        // while drawing 128, leaving a strip of dead panel too narrow for a third column.
        // A default that fits one column would be worse — half the grid for no reason.
        let inside = PANEL_WIDTH - SCROLL_BAR_ALLOWANCE;
        assert_eq!(
            columns_for(inside, ASSUMED_GAP),
            2,
            "the panel opened at {PANEL_WIDTH} which lays out \
             {} columns, not two",
            columns_for(inside, ASSUMED_GAP)
        );
        // And snug: not so wide that a third column nearly fits.
        assert!(
            inside < THUMBNAIL_WIDTH * 3.0,
            "the panel is wider than two columns need"
        );
    }

    #[test]
    fn a_landscape_document_gets_short_rows() {
        // The bug this arithmetic exists for. A 28-page landscape drawing set was given
        // rows of 204 points while each one drew about 102, so `show_rows` divided the
        // viewport by the wrong number and rendered five rows into a panel with room for
        // eight — leaving a third of the panel blank below the last page.
        //
        // A 34x22 in sheet is 0.647 tall for its width.
        let landscape = box_height(std::iter::once(22.0 / 34.0));
        assert!(
            landscape < THUMBNAIL_WIDTH,
            "a landscape sheet reserved {landscape} points of height for a 120 wide box"
        );
        let row = row_height(landscape, 8.0);
        assert!(
            row < 120.0,
            "a landscape row came to {row} points; the old constant was 204"
        );
    }

    #[test]
    fn a_portrait_document_gets_taller_rows_than_a_landscape_one() {
        // Both packed tight, which is the point: the box fits the document rather than
        // some fixed guess about page shape.
        let portrait = box_height(std::iter::once(11.0 / 8.5));
        let landscape = box_height(std::iter::once(8.5 / 11.0));
        assert!(portrait > landscape, "{portrait} should exceed {landscape}");
        assert!(
            portrait > THUMBNAIL_WIDTH,
            "a letter page is taller than wide"
        );
    }

    #[test]
    fn the_box_fits_the_tallest_page_in_a_mixed_document() {
        // Every row is the same height, so the box has to clear the tallest page or that
        // page would be cropped.
        let mixed = box_height([0.65_f32, 1.294, 0.5].into_iter());
        let tallest = box_height(std::iter::once(1.294_f32));
        assert!(
            (mixed - tallest).abs() < 0.01,
            "mixed {mixed} should match the tallest page {tallest}"
        );
    }

    #[test]
    fn one_freak_page_does_not_stretch_every_row() {
        // A 10:1 page in an otherwise ordinary document would otherwise make every row
        // ten thumbnails tall, and the grid would show one page at a time.
        let freak = box_height([1.294_f32, 10.0].into_iter());
        assert!(
            freak <= THUMBNAIL_WIDTH * MAX_THUMBNAIL_ASPECT,
            "a freak page produced a {freak} point row"
        );
    }

    #[test]
    fn a_document_with_no_usable_geometry_still_gets_a_row_height() {
        // Every page needs a slot whatever its dimensions say, or the grid's positions
        // would stop matching the document's.
        for aspects in [vec![], vec![0.0_f32], vec![f32::NAN], vec![-1.0]] {
            let height = box_height(aspects.into_iter());
            assert!(height.is_finite() && height > 0.0, "got {height}");
            let row = row_height(height, 8.0);
            assert!(row.is_finite() && row > height, "got {row}");
        }
    }

    #[test]
    fn a_row_leaves_room_for_the_page_number() {
        // `show_rows` is told this number and lays rows out by it, so the label has to be
        // inside it — a row that forgot the label would overlap the next one.
        let box_h = box_height(std::iter::once(1.294_f32));
        assert!(row_height(box_h, 8.0) >= box_h + LABEL_HEIGHT);
        // A negative gap from a hostile style must not shrink the row below the box.
        assert!(row_height(box_h, -50.0) >= box_h + LABEL_HEIGHT);
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
    fn a_stationary_press_is_too_small_to_be_a_box() {
        // The regression this constant exists for: a click landing in the gap between two
        // thumbnails used to build a zero-size marquee, which covered nothing, which threw
        // the whole selection away. Missing a page by four pixels must not cost the
        // selection you spent a dozen clicks building.
        let origin = egui::pos2(100.0, 100.0);
        for drift in [0.0_f32, 1.0, 2.0] {
            let moved = origin + egui::vec2(drift, 0.0);
            assert!(
                (moved - origin).length() < MARQUEE_MINIMUM_DRAG,
                "{drift} px counted as a drag"
            );
        }
        // And a real drag still clears the bar comfortably.
        let dragged = origin + egui::vec2(40.0, 25.0);
        assert!((dragged - origin).length() >= MARQUEE_MINIMUM_DRAG);
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
