//! Where every page sits in a single scrolling column.
//!
//! The load-bearing fact is that real PDFs mix page sizes and rotations freely
//! within one document, so a scroll offset cannot be computed as
//! `page_index * page_height`. Everything here stores or derives per-page offsets
//! instead, and the tests lean on the mixed-size case for that reason.

use std::ops::Range;

use porpoise_doc::PageGeometry;

use crate::fit::{FitMode, fit_scale};

/// The gap between pages in a scrolling column, in PDF points.
///
/// Lives here rather than in the shell because it is a property of the layout, and
/// because more than one caller has to agree on it: the viewer builds a layout to
/// paint, and `porpoise info` builds one to describe. It was previously declared
/// separately in each — plus a third time in an integration test — so `info` could
/// have reported a scroll height for a layout the viewer does not build, and nothing
/// would have compared the two.
pub const PAGE_GAP_PT: f64 = 12.0;

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
    tallest_page_height_pt: f64,
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
        let mut tallest_page_height_pt = 0.0_f64;

        for page in pages {
            let height_pt = sanitize(page.height_pt);
            tops.push(cursor_pt);
            bottoms.push(cursor_pt + height_pt);
            cursor_pt += height_pt + gap_pt;
            content_width_pt = content_width_pt.max(sanitize(page.width_pt));
            tallest_page_height_pt = tallest_page_height_pt.max(height_pt);
        }

        // The column ends at the last page's bottom edge, not after a trailing gap.
        let content_height_pt = bottoms.last().copied().unwrap_or(0.0);

        Self {
            tops,
            bottoms,
            content_height_pt,
            content_width_pt,
            tallest_page_height_pt,
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

    /// Height of the tallest page.
    #[must_use]
    pub fn tallest_page_height_pt(&self) -> f64 {
        self.tallest_page_height_pt
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

    /// Zoom at which the largest page fits entirely inside the viewport.
    ///
    /// Uses the bounding box across *all* pages — widest width, tallest height —
    /// rather than the page currently on screen. Fitting the current page would
    /// change the zoom every time you scrolled between two page sizes, which reads
    /// as the document jumping around.
    #[must_use]
    pub fn fit_page_scale(&self, viewport_width_pt: f32, viewport_height_pt: f32) -> f32 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "page dimensions are small; f32 is the precision the renderer works in"
        )]
        let largest = PageGeometry {
            width_pt: self.content_width_pt as f32,
            height_pt: self.tallest_page_height_pt as f32,
        };
        fit_scale(
            FitMode::Page,
            largest,
            viewport_width_pt,
            viewport_height_pt,
        )
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
    fn non_finite_viewport_is_rejected_rather_than_panicking() {
        let layout = ScrollLayout::vertical(&[letter()], 10.0);
        assert_eq!(layout.visible_pages(f64::NAN, 800.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, f64::NAN), 0..0);
        assert_eq!(layout.visible_pages(0.0, 0.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, -10.0), 0..0);
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

    // --- Fit modes over a whole document -------------------------------------

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
    fn tallest_page_is_tracked_independently_of_the_widest() {
        // A landscape page can be the widest while a portrait one is the tallest.
        let layout = ScrollLayout::vertical(&[page(1224.0, 400.0), letter()], 10.0);
        assert_eq!(layout.content_width_pt(), 1224.0);
        assert_eq!(layout.tallest_page_height_pt(), 792.0);
    }

    #[test]
    fn fit_page_uses_the_bounding_box_across_all_pages() {
        // Widest is 1224, tallest is 792. In a 1224x792 viewport that is exactly
        // 1:1; anything larger than 1:1 would clip one of the two pages.
        let layout = ScrollLayout::vertical(&[page(1224.0, 400.0), letter()], 10.0);
        assert_eq!(layout.fit_page_scale(1224.0, 792.0), 1.0);

        // Halving the viewport halves the zoom.
        assert_eq!(layout.fit_page_scale(612.0, 396.0), 0.5);
    }

    #[test]
    fn fit_page_is_never_larger_than_fit_width() {
        // Fit-page has to satisfy the height constraint as well, so it can only be
        // more restrictive.
        let layout = ScrollLayout::vertical(&[letter(), page(792.0, 612.0)], 10.0);
        for (width, height) in [(400.0, 300.0), (1000.0, 200.0), (2000.0, 5000.0)] {
            let page_fit = layout.fit_page_scale(width, height);
            let width_fit = layout.fit_width_scale(width);
            assert!(
                page_fit <= width_fit + f32::EPSILON,
                "in {width}x{height}, fit-page {page_fit} exceeded fit-width {width_fit}"
            );
        }
    }

    #[test]
    fn fit_page_survives_an_empty_or_degenerate_layout() {
        let empty = ScrollLayout::vertical(&[], 10.0);
        let scale = empty.fit_page_scale(1000.0, 1000.0);
        assert!(scale.is_finite() && scale > 0.0, "got {scale}");

        let degenerate = ScrollLayout::vertical(&[page(f32::NAN, 0.0)], 10.0);
        for (width, height) in [(0.0, 0.0), (f32::NAN, f32::NAN), (-5.0, 10.0)] {
            let scale = degenerate.fit_page_scale(width, height);
            assert!(
                scale.is_finite() && scale > 0.0,
                "{width}x{height} gave {scale}"
            );
        }
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
}
