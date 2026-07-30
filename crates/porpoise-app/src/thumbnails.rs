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
//! It also relies on a page being allowed *two* cached rungs at once: the main view's
//! and the grid's. That was already true, but only by accident — `PageCache` had a
//! `retain_bucket` that would have evicted one for the other, unused and with a doc
//! comment claiming otherwise. Removed rather than left as a trap.
//!
//! # Dragging is a gesture, not a command
//!
//! Dropping page 7 onto slot 2 produces `MovePage { from: 7, to: 2 }` — the command that
//! already existed, which the toolbar and the control channel also produce. So this
//! module adds a way to *author* an edit and no new capability, exactly like the file
//! picker authoring an `open`. Whether the panel is showing is a different matter and
//! does get a command, because it changes what is on screen and an agent that opens it
//! can also close it. See `docs/goal-4-plan.md` §7.

use eframe::egui;
use porpoise_doc::{Document, PageOrder};
use porpoise_render::RenderPool;
use porpoise_view::{CacheKey, PageCache, PageNumber, ZoomBucket};

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

/// Everything the grid needs from the open document.
///
/// Passed in rather than reached for, so this module never sees the viewer's state and
/// stays the only thing that knows how the grid is arranged.
pub(crate) struct Grid<'a> {
    pub(crate) order: &'a PageOrder,
    pub(crate) document: &'a Document,
    pub(crate) cache: &'a mut PageCache<egui::TextureHandle>,
    pub(crate) pool: &'a RenderPool,
    pub(crate) in_flight: &'a mut Vec<CacheKey>,
    /// Display position of the page the main view is showing, highlighted here.
    pub(crate) current: usize,
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

/// Draws the grid, returning a move if one was dropped this frame.
///
/// The returned pair is `(from, to)` in display positions. Nothing is changed here — the
/// caller turns it into a command, so a drag goes through the same dispatch as every
/// other edit.
pub(crate) fn draw(ui: &mut egui::Ui, grid: &mut Grid<'_>) -> Option<(usize, usize)> {
    let pages = grid.order.len();
    if pages == 0 {
        ui.label("no pages");
        return None;
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

    let mut moved = None;

    // `show_rows` only calls back for the rows on screen, which is what keeps a
    // 400-page grid from rasterizing 400 thumbnails.
    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show_rows(ui, cell_height, rows, |ui, row_range| {
            for row in row_range {
                ui.horizontal(|ui| {
                    for column in 0..columns {
                        let position = row * columns + column;
                        if position >= pages {
                            break;
                        }
                        if let Some(dropped) = cell(ui, grid, position, bucket) {
                            moved = Some((dropped, position));
                        }
                    }
                });
            }
        });

    moved
}

/// One thumbnail: a drop target wrapped around a draggable page.
fn cell(
    ui: &mut egui::Ui,
    grid: &mut Grid<'_>,
    position: usize,
    bucket: ZoomBucket,
) -> Option<usize> {
    let page = grid.order.source_of(position)?;
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
        request(grid, page, key, bucket);
    }

    let (_, dropped) = ui.dnd_drop_zone::<usize, _>(egui::Frame::default(), |ui| {
        let id = egui::Id::new(("porpoise-thumbnail", position));
        ui.dnd_drag_source(id, position, |ui| {
            ui.vertical(|ui| {
                let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::hover());
                match texture {
                    Some(texture) => {
                        ui.painter()
                            .image(texture, rect, FULL_UV, egui::Color32::WHITE);
                    }
                    None => {
                        // The same placeholder the main view uses, for the same reason:
                        // a grey box that becomes a page beats the grid reflowing once
                        // each thumbnail arrives.
                        ui.painter()
                            .rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
                    }
                }
                // The page number under each one, because the whole job of the grid is
                // telling you which page you are about to move.
                let number = PageNumber::from_index(position);
                if position == grid.current {
                    ui.colored_label(ui.visuals().selection.bg_fill, format!("{number}"));
                } else {
                    ui.label(format!("{number}"));
                }
            });
        });
    });

    dropped.map(|from| *from).filter(|from| *from != position)
}

/// Asks for a thumbnail, unless one is already queued.
fn request(grid: &mut Grid<'_>, page: usize, key: CacheKey, bucket: ZoomBucket) {
    if grid.in_flight.contains(&key) {
        return;
    }
    let scale = bucket.scale() * grid.pixels_per_point;
    if grid.pool.submit(page, scale, i64::from(bucket.rung())) {
        grid.in_flight.push(key);
    }
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

    #[test]
    fn a_high_dpi_screen_asks_for_more_pixels() {
        // The thumbnail is drawn at a fixed size in points, so a 2x screen needs twice
        // the pixels or it looks soft.
        let single = bucket_for(1224.0, 1.0).scale();
        let double = bucket_for(1224.0, 2.0).scale();
        assert!(double > single, "2x {double} should exceed 1x {single}");
    }
}
