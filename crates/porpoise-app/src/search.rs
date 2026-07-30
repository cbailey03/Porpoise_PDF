//! Which pages the grid shows, from what was typed in the search box.
//!
//! Turning `"1, 4-6, 9"` into a set of pages is arithmetic over a string, so it lives
//! here rather than in [`crate::thumbnails`] and is tested without a window — the same
//! split [`crate::edits`] and [`crate::selection`] made.
//!
//! # It searches page numbers, not page text
//!
//! Deliberately, and worth writing down because "search" invites the other reading. The
//! render backend can in fact give us the text: `hayro_interpret::Glyph::as_unicode` walks
//! the `ToUnicode` cmap and the Adobe Glyph List for us, so a text index is a `Device`
//! implementation away rather than a font-parsing project.
//!
//! It is not what this is, for two reasons. Text search needs an extraction layer big
//! enough to be its own piece of work, and it answers *nothing* for the documents this is
//! most used on: a scanned sheet has no text objects at all, and a CAD export often draws
//! its labels as outlines. A page-number search works on every document, which is the
//! whole of what it promises.
//!
//! # Half-typed queries stay calm
//!
//! The box filters as you type, so every prefix of a real query is a query too. `"5-9"`
//! passes through `"5"` and `"5-"` on the way, and if `"5-"` meant "page 5 to the end" the
//! grid would flash the whole document between two keystrokes. So a range missing its
//! upper bound is read as just the lower one, and only a *leading* dash — `"-9"`, which no
//! prefix of a normal query produces — means "from the start".

use std::collections::BTreeSet;

/// Which pages the grid is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PageFilter {
    /// Nothing typed: every page.
    #[default]
    All,
    /// A query resolved to these display positions, ascending. Possibly empty, which is
    /// different from [`Self::All`] and means "nothing matched" rather than "no filter".
    Only(Vec<usize>),
}

impl PageFilter {
    /// Reads a query against a document of `page_count` pages.
    ///
    /// Never fails. A query is a filter, not a command, and refusing to parse one
    /// mid-keystroke would make the box unusable; anything unreadable simply matches
    /// nothing. Page numbers count from 1, as everywhere a person can see one, so `0` is
    /// not a page and contributes nothing.
    pub(crate) fn parse(query: &str, page_count: usize) -> Self {
        if query.trim().is_empty() {
            return Self::All;
        }

        let mut positions = BTreeSet::new();
        for token in query.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (low, high) = match token.split_once('-') {
                Some((from, to)) => match (bound(from), bound(to)) {
                    // Nonsense anywhere in a range makes the whole range nonsense. Not
                    // "treat the bad end as 1", which invented page 1 out of "0-0".
                    (Bound::Nonsense, _) | (_, Bound::Nonsense) => continue,
                    // A bare dash is the whole document, which is what somebody clearing
                    // a range back to nothing passes through.
                    (Bound::Missing, Bound::Missing) => (1, page_count),
                    (Bound::Missing, Bound::Number(high)) => (1, high),
                    // See the module docs: a range with no upper bound *yet* is the lower
                    // bound alone, so typing towards "5-9" never flashes the document.
                    (Bound::Number(low), Bound::Missing) => (low, low),
                    (Bound::Number(low), Bound::Number(high)) => (low, high),
                },
                None => match bound(token) {
                    Bound::Number(single) => (single, single),
                    Bound::Missing | Bound::Nonsense => continue,
                },
            };

            // Written the forgiving way round, so "9-5" is the range somebody meant.
            let (low, high) = if low <= high {
                (low, high)
            } else {
                (high, low)
            };
            // Clamped *before* the loop, not inside it: "1-99999999999" on a 400-page
            // document would otherwise spin through a hundred million numbers to add four
            // hundred, on every keystroke.
            let low = low.max(1);
            let high = high.min(page_count);
            if low > high {
                continue;
            }
            positions.extend((low..=high).map(|number| number - 1));
        }

        Self::Only(positions.into_iter().collect())
    }

    /// Whether a query is narrowing the grid.
    ///
    /// True even when the query matched nothing — the grid is still narrowed, to nothing,
    /// and that is a state worth telling somebody about rather than showing an empty panel.
    pub(crate) fn is_narrowed(&self) -> bool {
        matches!(self, Self::Only(_))
    }

    /// How many pages the grid will show.
    pub(crate) fn shown(&self, page_count: usize) -> usize {
        match self {
            Self::All => page_count,
            Self::Only(positions) => positions.len(),
        }
    }

    /// The display position the grid should draw in its `slot`th cell.
    ///
    /// The indirection the whole module exists for: the grid counts slots, the document
    /// counts positions, and with a filter up they are different numbers. Everything the
    /// grid reports back — a move, a pick, a page to keep a texture for — is a *position*,
    /// so this is the only place the two are allowed to meet.
    pub(crate) fn position_at(&self, slot: usize, page_count: usize) -> Option<usize> {
        match self {
            Self::All => (slot < page_count).then_some(slot),
            Self::Only(positions) => positions.get(slot).copied(),
        }
    }
}

/// One end of a range, or one number on its own.
///
/// Three outcomes rather than `Option`, because "nothing there" and "not a number" have to
/// be told apart: an absent bound opens the range, while a nonsensical one voids it.
/// Collapsing them is what made `"0-0"` produce page 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    /// Nothing between the separators — an open end.
    Missing,
    /// A number. Zero is kept rather than refused, because clamping it to the first page
    /// is what makes `"0-2"` mean pages 1 to 2 while `"0"` alone still means no page.
    Number(usize),
    /// Not a number at all.
    Nonsense,
}

/// Reads one bound.
fn bound(text: &str) -> Bound {
    let text = text.trim();
    if text.is_empty() {
        return Bound::Missing;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Bound::Nonsense;
    }
    // All digits, so parsing can only fail by overrunning a `usize` — and a number that
    // long is past the end of any document, which the clamp already handles.
    Bound::Number(text.parse().unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positions shown for `query` against a ten-page document.
    fn shown(query: &str) -> Vec<usize> {
        match PageFilter::parse(query, 10) {
            PageFilter::All => (0..10).collect(),
            PageFilter::Only(positions) => positions,
        }
    }

    #[test]
    fn an_empty_query_shows_everything() {
        // Distinct from a query that matched nothing, which is what `is_narrowed` is for.
        assert_eq!(PageFilter::parse("", 10), PageFilter::All);
        assert_eq!(PageFilter::parse("   ", 10), PageFilter::All);
        assert!(!PageFilter::parse("", 10).is_narrowed());
        assert_eq!(PageFilter::parse("", 10).shown(10), 10);
    }

    #[test]
    fn a_single_number_shows_that_page() {
        // Page 3 is at position 2: numbers count from 1, positions from 0.
        assert_eq!(shown("3"), vec![2]);
    }

    #[test]
    fn a_range_shows_every_page_in_it() {
        assert_eq!(shown("5-9"), vec![4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_list_shows_each_page() {
        assert_eq!(shown("1,4,7"), vec![0, 3, 6]);
    }

    #[test]
    fn lists_and_ranges_combine() {
        assert_eq!(shown("1, 4-6, 9"), vec![0, 3, 4, 5, 8]);
    }

    #[test]
    fn whitespace_is_ignored() {
        assert_eq!(shown("  2 ,  4 - 6 "), vec![1, 3, 4, 5]);
    }

    #[test]
    fn overlapping_parts_are_not_shown_twice() {
        // A page drawn twice would give the grid two cells claiming one position, and a
        // drag from the second would be a move from somewhere the person did not click.
        assert_eq!(shown("2-4,3,3-5"), vec![1, 2, 3, 4]);
    }

    #[test]
    fn pages_come_out_in_document_order() {
        // However they were typed. The grid reads them as slots, so out-of-order input
        // would shuffle the panel without anybody asking for a reorder.
        assert_eq!(shown("9,1,5"), vec![0, 4, 8]);
    }

    #[test]
    fn a_backwards_range_is_read_the_way_it_was_meant() {
        assert_eq!(shown("9-5"), shown("5-9"));
    }

    // --- Typing towards a query ----------------------------------------------

    #[test]
    fn a_range_with_no_end_yet_is_just_its_start() {
        // The reason this is not "5 to the end": every prefix of "5-9" is itself a query,
        // and flashing the whole document between two keystrokes is worse than showing
        // one page a moment early.
        assert_eq!(shown("5-"), vec![4]);
    }

    #[test]
    fn a_leading_dash_means_from_the_start() {
        // No prefix of an ordinary query looks like this, so it can afford to mean
        // something more useful.
        assert_eq!(shown("-3"), vec![0, 1, 2]);
    }

    #[test]
    fn every_prefix_of_a_real_query_is_readable() {
        // The property that matters for a box that filters as you type: nothing a person
        // passes through on the way to "1, 4-6, 9" may panic or match wildly.
        let full = "1, 4-6, 9";
        for end in 0..=full.len() {
            let Some(prefix) = full.get(..end) else {
                continue;
            };
            let filter = PageFilter::parse(prefix, 10);
            let count = filter.shown(10);
            assert!(count <= 10, "prefix {prefix:?} matched {count} of 10 pages");
        }
    }

    // --- Nonsense and edges --------------------------------------------------

    #[test]
    fn a_query_that_matches_nothing_is_still_a_filter() {
        // So the panel can say so, rather than looking broken.
        let filter = PageFilter::parse("abc", 10);
        assert_eq!(filter, PageFilter::Only(Vec::new()));
        assert!(filter.is_narrowed());
        assert_eq!(filter.shown(10), 0);
    }

    #[test]
    fn page_zero_is_not_a_page() {
        // The rule the whole program follows, and the protocol refuses `{"page":0}` for
        // the same reason.
        assert_eq!(shown("0"), Vec::<usize>::new());
        assert_eq!(shown("0-0"), Vec::<usize>::new());
        assert_eq!(shown("0-2"), vec![0, 1]);
    }

    #[test]
    fn pages_past_the_end_are_dropped() {
        assert_eq!(shown("11"), Vec::<usize>::new());
        assert_eq!(shown("8-99"), vec![7, 8, 9]);
    }

    #[test]
    fn a_huge_range_does_not_hang() {
        // Clamped before the loop rather than inside it. Unclamped this would count to
        // eighteen quintillion on a keystroke.
        let filter = PageFilter::parse("1-99999999999999999999", 10);
        assert_eq!(filter.shown(10), 10);
        assert_eq!(
            PageFilter::parse(&format!("1-{}", usize::MAX), 10).shown(10),
            10
        );
    }

    #[test]
    fn stray_separators_are_harmless() {
        assert_eq!(shown(",,,"), Vec::<usize>::new());
        assert_eq!(shown("1,,3"), vec![0, 2]);
        assert_eq!(shown("-"), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn a_negative_number_is_read_as_a_range() {
        // "1-5" and "-5" are the only readings a dash has here; there are no negative
        // page numbers to confuse it with.
        assert_eq!(shown("-5"), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn an_empty_document_shows_nothing_and_does_not_panic() {
        for query in ["", "1", "1-9", "abc"] {
            let filter = PageFilter::parse(query, 0);
            assert_eq!(filter.shown(0), 0, "query {query:?}");
            assert_eq!(filter.position_at(0, 0), None);
        }
    }

    // --- Slots versus positions ----------------------------------------------

    #[test]
    fn unfiltered_slots_are_positions() {
        let filter = PageFilter::parse("", 10);
        assert_eq!(filter.position_at(0, 10), Some(0));
        assert_eq!(filter.position_at(9, 10), Some(9));
        assert_eq!(filter.position_at(10, 10), None, "past the end");
    }

    #[test]
    fn filtered_slots_map_to_the_pages_that_matched() {
        // The crossing this module exists for. Slot 0 of a filtered grid is page 4, and a
        // drag from it has to report position 3 — not 0, which would move the wrong page.
        let filter = PageFilter::parse("4-6", 10);
        assert_eq!(filter.position_at(0, 10), Some(3));
        assert_eq!(filter.position_at(1, 10), Some(4));
        assert_eq!(filter.position_at(2, 10), Some(5));
        assert_eq!(filter.position_at(3, 10), None, "only three pages matched");
    }
}
