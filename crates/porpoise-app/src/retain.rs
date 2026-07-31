//! Which page textures survive a frame.
//!
//! The texture cache has **two** consumers — the scrolling page column and the thumbnail
//! grid — and this is the one place that decides what either of them still needs. That is
//! the whole reason it exists as a module rather than a few lines inside the page column,
//! which is where it used to live.
//!
//! # The bug that moved it here
//!
//! Reported as *"pages 10, 11 and 12 flicker"* on a twelve-page document. The page column
//! evicted everything outside its own window, unaware that the grid was showing the whole
//! document. So each frame: the grid asked for a thumbnail, the render landed, the column
//! evicted it because that page is far from the viewport, and the grid asked again. A
//! permanent render in flight and three thumbnails strobing.
//!
//! It is arithmetic, not a race. The window is the visible positions plus a margin either
//! side, so on a document only a little longer than that margin the pages past the end of
//! it are exactly the ones the grid can see and the column cannot. Which pages flicker is
//! decided by `RETAIN_PAGES` and the document's length, so the same document flickers the
//! same three pages every time.
//!
//! # Positions and pages are different things
//!
//! The column's window is a range of display **positions**; the cache is keyed by
//! **source** page. After any reorder those differ, so the positions have to be resolved
//! before comparing — comparing them directly evicts textures for pages that are on screen
//! and keeps ones that are not, which shows up as pages flashing grey while scrolling near
//! an edit.
//!
//! Note that keeping a *page* keeps every **rung** of it. That is deliberate and the grid
//! depends on it: a page is held at reading size for the column and at thumbnail size for
//! the grid at the same time. See `PageCache::retain_pages`.

use std::ops::Range;

use porpoise_doc::Source;

/// The sources whose textures are worth keeping.
///
/// `visible` is the display positions the page column is showing and `margin` how many
/// positions either side of it to keep as well, so that reversing direction reuses a
/// texture instead of re-rendering. `grid` is the sources the thumbnail grid is showing,
/// already resolved — it is virtualized, so that is the rows on screen and not the whole
/// document.
///
/// `source_of` maps a display position to a source. A function rather than a `PageOrder`
/// so this stays pure and the position-versus-source distinction can be tested with an
/// order that actually differs from the identity.
///
/// Takes `grid` by value and extends it, so the caller's per-frame allocation is reused
/// rather than a second one being made to merge into.
pub(crate) fn pages_to_keep(
    visible: &Range<usize>,
    margin: usize,
    grid: Vec<Source>,
    source_of: impl Fn(usize) -> Option<Source>,
) -> Vec<Source> {
    let mut keep = grid;
    let low = visible.start.saturating_sub(margin);
    let high = visible.end.saturating_add(margin);
    keep.extend((low..high).filter_map(source_of));
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of the primary document, for the tests that predate merging.
    fn p(page: usize) -> Source {
        Source { document: 0, page }
    }

    /// An unedited document, where a display position and a source page are the same thing.
    fn unedited(pages: usize) -> impl Fn(usize) -> Option<Source> {
        move |position| (position < pages).then(|| p(position))
    }

    #[test]
    fn a_page_the_grid_is_showing_survives_a_distant_viewport() {
        // The reported bug. Twelve pages, the column showing only the first — which is what
        // paged mode does — and a margin of eight. Positions 0..9 resolve to pages 0..8, so
        // pages 9, 10 and 11 are outside the column's window entirely; the grid is showing
        // them and they have to stay.
        let grid: Vec<Source> = (0..12).map(p).collect();
        let keep = pages_to_keep(&(0..1), 8, grid, unedited(12));
        for page in [9, 10, 11] {
            assert!(
                keep.contains(&p(page)),
                "page {page} would be evicted while the grid is showing it: {keep:?}"
            );
        }
    }

    #[test]
    fn the_column_keeps_a_margin_either_side_of_what_it_shows() {
        let keep = pages_to_keep(&(20..22), 3, Vec::new(), unedited(100));
        for page in 17..25 {
            assert!(keep.contains(&p(page)), "page {page} missing from {keep:?}");
        }
        assert!(!keep.contains(&p(16)), "kept too far above: {keep:?}");
        assert!(!keep.contains(&p(25)), "kept too far below: {keep:?}");
    }

    #[test]
    fn with_the_grid_closed_only_the_column_is_kept() {
        let keep = pages_to_keep(&(0..1), 2, Vec::new(), unedited(12));
        assert_eq!(keep, vec![p(0), p(1), p(2)]);
    }

    #[test]
    fn positions_are_resolved_through_the_order_before_being_kept() {
        // A reversed document: display position 0 is source page 11. Keeping *positions*
        // here would evict the pages actually on screen and hold the ones that are not,
        // which is how a reorder made pages flash grey while scrolling near an edit.
        let reversed = |position: usize| (position < 12).then(|| p(11 - position));
        let keep = pages_to_keep(&(0..1), 2, Vec::new(), reversed);
        assert_eq!(keep, vec![p(11), p(10), p(9)]);
    }

    #[test]
    fn a_window_running_off_either_end_keeps_what_exists() {
        // Near the top the margin would underflow, and near the bottom it runs past the
        // last page. Neither may panic or drop a page that is on screen.
        let keep = pages_to_keep(&(0..1), 8, Vec::new(), unedited(3));
        assert_eq!(keep, vec![p(0), p(1), p(2)]);

        let keep = pages_to_keep(&(2..3), 8, Vec::new(), unedited(3));
        assert_eq!(keep, vec![p(0), p(1), p(2)]);
    }

    #[test]
    fn an_empty_document_keeps_nothing() {
        assert!(pages_to_keep(&(0..0), 8, Vec::new(), unedited(0)).is_empty());
    }

    #[test]
    fn a_page_from_a_second_document_is_kept_independently_of_the_first() {
        // A merged document can show page 0 of two different files at once. The column's
        // window and the grid's set both name sources, not bare page numbers, so a page
        // from document 1 must never be confused with document 0's page of the same number.
        let inserted = Source {
            document: 1,
            page: 0,
        };
        let source_of = move |position: usize| match position {
            0 => Some(p(0)),
            1 => Some(inserted),
            _ => None,
        };
        let keep = pages_to_keep(&(0..2), 0, Vec::new(), source_of);
        assert!(keep.contains(&p(0)));
        assert!(keep.contains(&inserted));
        assert_eq!(
            keep.len(),
            2,
            "the two documents' pages collapsed into one: {keep:?}"
        );
    }
}
