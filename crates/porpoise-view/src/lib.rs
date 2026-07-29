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

use std::ops::Range;

use porpoise_doc::PageGeometry;

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
    fn non_finite_viewport_is_rejected_rather_than_panicking() {
        let layout = ScrollLayout::vertical(&[letter()], 10.0);
        assert_eq!(layout.visible_pages(f64::NAN, 800.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, f64::NAN), 0..0);
        assert_eq!(layout.visible_pages(0.0, 0.0), 0..0);
        assert_eq!(layout.visible_pages(0.0, -10.0), 0..0);
    }
}
