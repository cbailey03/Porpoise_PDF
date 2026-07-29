//! What to rasterize next, and in what order.
//!
//! Separate from layout because it is a *policy*, not geometry: which speculative
//! pages are worth a worker's time, and which order gives the best chance of a
//! page being ready before the viewport reaches it.

use std::ops::Range;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
