//! Viewport logic for a scrolling document view: where pages sit, and which of
//! them are worth rasterizing right now.
//!
//! Everything here is pure arithmetic over page geometry, with no windowing or
//! GPU dependency. That is deliberate. It makes the parts most likely to be
//! subtly wrong — scroll offsets across heterogeneous page sizes, visible-set
//! computation, cache eviction — unit-testable without a window, and it caps the
//! cost of swapping the shell to one crate. See `docs/goal-1-plan.md`, section 3.
//!
//! All coordinates are in PDF points (1/72 inch) unless named otherwise. Zoom is
//! applied when converting to physical pixels, not here.

mod cache;
mod zoom;

pub use cache::{CacheKey, PageCache};
pub use zoom::ZoomBucket;

use std::ops::Range;

use porpoise_doc::PageGeometry;

/// The pages worth rasterizing, in the order they should be requested.
///
/// Visible pages come first, then alternating outward — one after, one before,
/// widening — up to `prefetch` pages either side. Order matters because the
/// render queue is served in order and the viewport can move before it drains,
/// so anything speculative must come after everything visible.
///
/// Scrolling down is more common than up, so each outward step takes the page
/// *after* the viewport before the one before it.
#[must_use]
pub fn request_order(visible: Range<usize>, prefetch: usize, page_count: usize) -> Vec<usize> {
    if page_count == 0 {
        return Vec::new();
    }
    let last = page_count.saturating_sub(1);

    let mut order: Vec<usize> = visible.clone().filter(|page| *page <= last).collect();

    for step in 1..=prefetch {
        // After the visible range. `visible.end` is exclusive, so the first step
        // is the page just past it.
        let after = visible.end.saturating_add(step - 1);
        if after <= last {
            order.push(after);
        }
        // Before it.
        if let Some(before) = visible.start.checked_sub(step) {
            order.push(before);
        }
    }

    order.dedup();
    order
}

/// Where every page sits in a single vertically scrolling column.
///
/// Built once per document. Because real PDFs mix page sizes and rotations, this
/// stores per-page offsets rather than assuming a uniform page height.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollLayout {
    /// Top edge of each page, ascending. One entry per page.
    tops: Vec<f64>,
    /// Bottom edge of each page, ascending. One entry per page.
    bottoms: Vec<f64>,
    content_height_pt: f64,
    content_width_pt: f64,
}

impl ScrollLayout {
    /// Stacks `pages` into a single column, separated by `gap_pt` points.
    ///
    /// A non-finite or negative gap is treated as zero, and non-finite page
    /// dimensions are treated as zero, so that a malformed document produces a
    /// degenerate layout rather than poisoning every later offset with `NaN`.
    #[must_use]
    pub fn vertical(pages: &[PageGeometry], gap_pt: f64) -> Self {
        let gap_pt = if gap_pt.is_finite() && gap_pt > 0.0 {
            gap_pt
        } else {
            0.0
        };

        let sanitize = |value: f32| {
            let value = f64::from(value);
            if value.is_finite() && value > 0.0 {
                value
            } else {
                0.0
            }
        };

        let mut tops = Vec::with_capacity(pages.len());
        let mut bottoms = Vec::with_capacity(pages.len());
        let mut cursor_pt = 0.0_f64;
        let mut content_width_pt = 0.0_f64;

        for page in pages {
            let height_pt = sanitize(page.height_pt);
            tops.push(cursor_pt);
            bottoms.push(cursor_pt + height_pt);
            cursor_pt += height_pt + gap_pt;
            content_width_pt = content_width_pt.max(sanitize(page.width_pt));
        }

        // The column ends at the last page's bottom edge, not after a trailing gap.
        let content_height_pt = bottoms.last().copied().unwrap_or(0.0);

        Self {
            tops,
            bottoms,
            content_height_pt,
            content_width_pt,
        }
    }

    /// Number of pages in the layout.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.tops.len()
    }

    /// Total scrollable height.
    #[must_use]
    pub fn content_height_pt(&self) -> f64 {
        self.content_height_pt
    }

    /// Width of the widest page.
    #[must_use]
    pub fn content_width_pt(&self) -> f64 {
        self.content_width_pt
    }

    /// Top edge of page `index`.
    #[must_use]
    pub fn page_top_pt(&self, index: usize) -> Option<f64> {
        self.tops.get(index).copied()
    }

    /// Bottom edge of page `index`.
    #[must_use]
    pub fn page_bottom_pt(&self, index: usize) -> Option<f64> {
        self.bottoms.get(index).copied()
    }

    /// The pages intersecting a viewport of `viewport_height_pt` whose top edge
    /// is at `viewport_top_pt`.
    ///
    /// This is the input to virtualization: only these pages, plus a prefetch
    /// margin, are worth holding rasterized.
    #[must_use]
    pub fn visible_pages(&self, viewport_top_pt: f64, viewport_height_pt: f64) -> Range<usize> {
        if self.tops.is_empty()
            || !viewport_top_pt.is_finite()
            || !viewport_height_pt.is_finite()
            || viewport_height_pt <= 0.0
        {
            return 0..0;
        }
        let viewport_bottom_pt = viewport_top_pt + viewport_height_pt;

        // Both edge lists are ascending, so each bound is one binary search.
        let start = self
            .bottoms
            .partition_point(|&bottom| bottom <= viewport_top_pt);
        let end = self.tops.partition_point(|&top| top < viewport_bottom_pt);

        start..end.max(start)
    }

    /// Zoom needed to fit the *widest* page to `viewport_width_pt`.
    ///
    /// Deliberately based on the widest page rather than the current one. A
    /// document view uses a single zoom throughout, so that scrolling past a
    /// landscape page does not resize every other page — which is what happens if
    /// you fit each page independently.
    #[must_use]
    pub fn fit_width_scale(&self, viewport_width_pt: f32) -> f32 {
        // `content_width_pt` is already sanitized in `vertical`, so this only has
        // to survive a degenerate viewport.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "page widths are small; f32 is the precision the renderer works in"
        )]
        let widest = PageGeometry {
            width_pt: self.content_width_pt as f32,
            height_pt: 1.0,
        };
        fit_scale(FitMode::Width, widest, viewport_width_pt, f32::INFINITY)
    }

    /// The page containing `y_pt`, saturating to the first or last page when
    /// `y_pt` falls outside the content or into an inter-page gap.
    ///
    /// Used by paged scrolling to decide which page to snap to.
    #[must_use]
    pub fn page_at_pt(&self, y_pt: f64) -> Option<usize> {
        if self.tops.is_empty() || !y_pt.is_finite() {
            return None;
        }
        let last = self.tops.len().saturating_sub(1);
        let candidate = self
            .tops
            .partition_point(|&top| top <= y_pt)
            .saturating_sub(1);
        Some(candidate.min(last))
    }
}

/// How a page should be sized relative to the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FitMode {
    /// Scale so the page's width exactly fills the viewport width.
    Width,
    /// Scale so the entire page is visible, letterboxing the shorter axis.
    Page,
    /// An explicit zoom factor, ignoring the viewport.
    Fixed(f32),
}

/// Smallest zoom [`fit_scale`] will return.
pub const MIN_SCALE: f32 = 0.01;

/// Largest zoom [`fit_scale`] will return.
///
/// This is a usability bound, not a safety one — `porpoise-render` enforces the
/// limits that actually protect memory.
pub const MAX_SCALE: f32 = 64.0;

/// The zoom factor that satisfies `mode` for one page in a given viewport.
///
/// Viewport dimensions are in points, so a caller working in physical pixels
/// should divide by its device pixel ratio first. The result is always finite and
/// within [`MIN_SCALE`]..=[`MAX_SCALE`], including when the page or viewport is
/// degenerate — a malformed document should letterbox oddly, not crash the view.
#[must_use]
pub fn fit_scale(
    mode: FitMode,
    page: PageGeometry,
    viewport_width_pt: f32,
    viewport_height_pt: f32,
) -> f32 {
    let usable = |value: f32| value.is_finite() && value > 0.0;

    let scale = match mode {
        FitMode::Fixed(scale) => scale,
        FitMode::Width => {
            if usable(viewport_width_pt) && usable(page.width_pt) {
                viewport_width_pt / page.width_pt
            } else {
                1.0
            }
        }
        FitMode::Page => {
            let fits_width = usable(viewport_width_pt) && usable(page.width_pt);
            let fits_height = usable(viewport_height_pt) && usable(page.height_pt);
            match (fits_width, fits_height) {
                // The whole page is visible only if both axes fit, so take the
                // more restrictive of the two.
                (true, true) => {
                    (viewport_width_pt / page.width_pt).min(viewport_height_pt / page.height_pt)
                }
                (true, false) => viewport_width_pt / page.width_pt,
                (false, true) => viewport_height_pt / page.height_pt,
                (false, false) => 1.0,
            }
        }
    };

    if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(width_pt: f32, height_pt: f32) -> PageGeometry {
        PageGeometry {
            width_pt,
            height_pt,
        }
    }

    /// US Letter at 72 DPI.
    fn letter() -> PageGeometry {
        page(612.0, 792.0)
    }

    #[test]
    fn empty_document_has_no_pages_and_no_height() {
        let layout = ScrollLayout::vertical(&[], 10.0);
        assert_eq!(layout.page_count(), 0);
        assert_eq!(layout.content_height_pt(), 0.0);
        assert_eq!(layout.visible_pages(0.0, 800.0), 0..0);
        assert_eq!(layout.page_at_pt(0.0), None);
    }

    #[test]
    fn uniform_pages_stack_with_gaps_but_no_trailing_gap() {
        let layout = ScrollLayout::vertical(&[letter(), letter(), letter()], 10.0);
        assert_eq!(layout.page_top_pt(0), Some(0.0));
        assert_eq!(layout.page_bottom_pt(0), Some(792.0));
        assert_eq!(layout.page_top_pt(1), Some(802.0));
        assert_eq!(layout.page_top_pt(2), Some(1604.0));
        // 3 pages + 2 gaps, with nothing after the final page.
        assert_eq!(layout.content_height_pt(), 792.0 * 3.0 + 10.0 * 2.0);
    }

    #[test]
    fn heterogeneous_page_sizes_shift_later_offsets() {
        // The case that breaks `index * height`: a landscape page mid-document.
        let layout = ScrollLayout::vertical(&[letter(), page(792.0, 612.0), letter()], 0.0);
        assert_eq!(layout.page_top_pt(1), Some(792.0));
        assert_eq!(layout.page_top_pt(2), Some(1404.0));
        assert_eq!(layout.content_height_pt(), 2196.0);
        // Content width tracks the widest page, which is the landscape one.
        assert_eq!(layout.content_width_pt(), 792.0);
    }

    #[test]
    fn viewport_spanning_a_boundary_sees_both_pages() {
        let layout = ScrollLayout::vertical(&[letter(), letter(), letter()], 10.0);
        // Straddles the page 0 / page 1 boundary at y = 792..802.
        assert_eq!(layout.visible_pages(700.0, 200.0), 0..2);
    }

    #[test]
    fn viewport_inside_one_page_sees_only_that_page() {
        let layout = ScrollLayout::vertical(&[letter(), letter(), letter()], 10.0);
        assert_eq!(layout.visible_pages(100.0, 200.0), 0..1);
        assert_eq!(layout.visible_pages(900.0, 100.0), 1..2);
    }

    #[test]
    fn viewport_taller_than_content_sees_every_page() {
        let layout = ScrollLayout::vertical(&[letter(), letter()], 10.0);
        assert_eq!(layout.visible_pages(0.0, 10_000.0), 0..2);
    }

    #[test]
    fn viewport_in_a_gap_sees_no_page() {
        let layout = ScrollLayout::vertical(&[letter(), letter()], 100.0);
        // Strictly inside the 792..892 gap.
        assert_eq!(layout.visible_pages(800.0, 50.0), 1..1);
    }

    #[test]
    fn viewport_past_the_end_sees_no_page() {
        let layout = ScrollLayout::vertical(&[letter()], 10.0);
        let empty = layout.visible_pages(5_000.0, 800.0);
        assert!(empty.is_empty(), "expected empty range, got {empty:?}");
    }

    #[test]
    fn overscroll_above_the_document_still_sees_the_first_page() {
        let layout = ScrollLayout::vertical(&[letter()], 10.0);
        assert_eq!(layout.visible_pages(-500.0, 800.0), 0..1);
    }

    #[test]
    fn page_at_point_saturates_outside_the_content() {
        let layout = ScrollLayout::vertical(&[letter(), letter()], 10.0);
        assert_eq!(layout.page_at_pt(-100.0), Some(0));
        assert_eq!(layout.page_at_pt(0.0), Some(0));
        assert_eq!(layout.page_at_pt(500.0), Some(0));
        assert_eq!(layout.page_at_pt(900.0), Some(1));
        assert_eq!(layout.page_at_pt(99_999.0), Some(1));
    }

    #[test]
    fn degenerate_input_does_not_produce_nan_offsets() {
        // A malformed document must not poison every subsequent offset.
        let layout = ScrollLayout::vertical(
            &[page(f32::NAN, f32::NAN), letter(), page(-5.0, -5.0)],
            f64::NAN,
        );
        assert!(layout.content_height_pt().is_finite());
        assert_eq!(layout.content_height_pt(), 792.0);
        assert_eq!(layout.page_top_pt(1), Some(0.0));
        for index in 0..layout.page_count() {
            assert!(
                layout.page_top_pt(index).is_some_and(f64::is_finite),
                "page {index} has a non-finite top"
            );
        }
    }

    #[test]
    fn request_order_puts_every_visible_page_before_any_prefetch() {
        let order = request_order(10..13, 3, 100);
        // Visible pages first, in document order.
        assert_eq!(&order[..3], &[10, 11, 12]);
        // Then outward, after before before.
        assert_eq!(&order[3..], &[13, 9, 14, 8, 15, 7]);
    }

    #[test]
    fn request_order_clamps_at_both_ends_of_the_document() {
        // At the start there is nothing before page 0.
        let order = request_order(0..2, 3, 10);
        assert_eq!(order, vec![0, 1, 2, 3, 4]);

        // At the end there is nothing after the last page.
        let order = request_order(8..10, 3, 10);
        assert_eq!(order, vec![8, 9, 7, 6, 5]);
    }

    #[test]
    fn request_order_with_no_prefetch_is_just_the_visible_range() {
        assert_eq!(request_order(4..7, 0, 100), vec![4, 5, 6]);
    }

    #[test]
    fn request_order_handles_an_empty_document_or_empty_viewport() {
        assert!(request_order(0..0, 3, 0).is_empty());
        // An empty visible range still prefetches around where the viewport sits,
        // starting at that position and working outward.
        let order = request_order(5..5, 2, 10);
        assert_eq!(order, vec![5, 4, 6, 3]);
    }

    #[test]
    fn request_order_never_names_a_page_twice() {
        // A prefetch margin wider than the document must not repeat pages.
        let order = request_order(1..3, 50, 4);
        let mut seen = order.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), order.len(), "duplicates in {order:?}");
        for page in &order {
            assert!(*page < 4, "page {page} is past the end in {order:?}");
        }
    }

    #[test]
    fn content_fit_width_uses_the_widest_page_not_the_first() {
        // Portrait first, landscape second. Fitting to the first page would zoom
        // in far enough to clip the second one.
        let layout = ScrollLayout::vertical(&[letter(), page(1224.0, 792.0)], 10.0);
        assert_eq!(layout.content_width_pt(), 1224.0);
        // 1224 pt of content in a 1224 pt viewport is 1:1, not 2x.
        assert_eq!(layout.fit_width_scale(1224.0), 1.0);
        assert_eq!(layout.fit_width_scale(612.0), 0.5);
    }

    #[test]
    fn content_fit_width_survives_an_empty_or_degenerate_layout() {
        let empty = ScrollLayout::vertical(&[], 10.0);
        let scale = empty.fit_width_scale(1000.0);
        assert!(scale.is_finite() && scale > 0.0, "got {scale}");

        let degenerate = ScrollLayout::vertical(&[page(f32::NAN, f32::NAN)], 10.0);
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let scale = degenerate.fit_width_scale(width);
            assert!(
                scale.is_finite() && scale > 0.0,
                "width {width} gave {scale}"
            );
        }
    }

    #[test]
    fn fit_width_scales_the_page_to_the_viewport_width() {
        // A 612 pt page in a 1224 pt viewport is exactly 2x.
        assert_eq!(fit_scale(FitMode::Width, letter(), 1224.0, 400.0), 2.0);
        assert_eq!(fit_scale(FitMode::Width, letter(), 306.0, 400.0), 0.5);
    }

    #[test]
    fn fit_width_ignores_the_viewport_height() {
        // Fit-to-width deliberately overflows vertically; that is what scrolling
        // is for.
        let wide = fit_scale(FitMode::Width, letter(), 1224.0, 10.0);
        let tall = fit_scale(FitMode::Width, letter(), 1224.0, 10_000.0);
        assert_eq!(wide, tall);
    }

    #[test]
    fn fit_page_takes_the_more_restrictive_axis() {
        // Width alone would allow 2x, but the height only allows 1x, so the whole
        // page fits only at 1x.
        let scale = fit_scale(FitMode::Page, letter(), 1224.0, 792.0);
        assert_eq!(scale, 1.0);

        // And the other way round.
        let scale = fit_scale(FitMode::Page, letter(), 612.0, 1584.0);
        assert_eq!(scale, 1.0);
    }

    #[test]
    fn fit_page_on_a_landscape_page_in_a_portrait_viewport() {
        let landscape = page(792.0, 612.0);
        // 396/792 = 0.5 by width, 612/612 = 1.0 by height. Width wins.
        assert_eq!(fit_scale(FitMode::Page, landscape, 396.0, 612.0), 0.5);
    }

    #[test]
    fn fixed_mode_passes_the_scale_through_but_still_clamps() {
        assert_eq!(fit_scale(FitMode::Fixed(3.0), letter(), 100.0, 100.0), 3.0);
        assert_eq!(
            fit_scale(FitMode::Fixed(1000.0), letter(), 100.0, 100.0),
            MAX_SCALE
        );
        assert_eq!(
            fit_scale(FitMode::Fixed(0.0), letter(), 100.0, 100.0),
            MIN_SCALE
        );
    }

    #[test]
    fn degenerate_page_or_viewport_falls_back_rather_than_dividing_by_zero() {
        let zero = page(0.0, 0.0);
        let nan = page(f32::NAN, f32::NAN);

        for mode in [FitMode::Width, FitMode::Page] {
            for geometry in [zero, nan] {
                let scale = fit_scale(mode, geometry, 1000.0, 1000.0);
                assert!(
                    scale.is_finite() && scale > 0.0,
                    "{mode:?} on {geometry:?} gave {scale}"
                );
            }
            // And a degenerate viewport against a good page.
            for (width, height) in [(0.0, 0.0), (f32::NAN, f32::NAN), (-5.0, -5.0)] {
                let scale = fit_scale(mode, letter(), width, height);
                assert!(
                    scale.is_finite() && scale > 0.0,
                    "{mode:?} in {width}x{height} gave {scale}"
                );
            }
        }
    }

    #[test]
    fn fit_scale_is_always_within_the_clamp_range() {
        // A tiny page in a huge viewport would otherwise ask for an enormous zoom.
        let scale = fit_scale(FitMode::Width, page(1.0, 1.0), 100_000.0, 100_000.0);
        assert_eq!(scale, MAX_SCALE);

        let scale = fit_scale(FitMode::Width, page(100_000.0, 100_000.0), 1.0, 1.0);
        assert_eq!(scale, MIN_SCALE);
    }

    #[test]
    fn non_finite_viewport_is_rejected_rather_than_panicking() {
        let layout = ScrollLayout::vertical(&[letter()], 10.0);
        assert_eq!(layout.visible_pages(f64::NAN, 800.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, f64::NAN), 0..0);
        assert_eq!(layout.visible_pages(0.0, 0.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, -10.0), 0..0);
    }
}
