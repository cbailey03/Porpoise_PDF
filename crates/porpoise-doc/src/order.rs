//! What order the pages are in, and how to change it.
//!
//! An edit never touches the file. It is held as a list of **source page indices in
//! display order**, starting at `[0, 1, 2, …]`. Moving a page reorders the list;
//! deleting one drops an entry. The document on disk changes only on save.
//!
//! Pure arithmetic over a `Vec<usize>` — no PDF, no `lopdf`, no window. Two useful
//! consequences: undo is a snapshot rather than an inverse operation, and a reorder
//! invalidates no rendered pages, because page textures stay keyed by source page.
//!
//! # Display position is not source page
//!
//! After any edit there are two page numbers in play and both are `usize`. This
//! codebase has been caught by that shape three times already — pixels versus PDF
//! points, zero-based indices versus one-based numbers, screen units versus document
//! units. Every crossing here goes through [`PageOrder::source_of`], and variables
//! are named `position` or `source`, never `page`. See `docs/goal-4-plan.md` §3.

/// How many undo steps are remembered.
///
/// Each step is a copy of the order — 3.2 KB for a 400-page document — so this is
/// bounded to keep a long session from growing without limit rather than because the
/// copies are expensive.
const UNDO_DEPTH: usize = 64;

/// The order pages are shown in, and the history to undo it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOrder {
    /// Source page indices, in display order.
    order: Vec<usize>,
    /// The order as it stands in the file. Starts equal to [`Self::order`] and moves
    /// only when a save reports success.
    ///
    /// This is what makes "unsaved changes" mean it, rather than meaning "differs from
    /// the document as first opened". Without it a saved document goes on claiming
    /// changes forever — the status bar nags, the Save button stays lit, and anything
    /// built on top warns when there is nothing left to lose. A warning that fires when
    /// nothing is at risk is one people learn to click through.
    saved: Vec<usize>,
    /// Pages in the document as opened. Needed to tell an edited order from a fresh
    /// one even after pages are deleted.
    source_len: usize,
    /// Previous orders, most recent last.
    history: Vec<Vec<usize>>,
}

impl PageOrder {
    /// The unedited order of a document with `page_count` pages.
    #[must_use]
    pub fn identity(page_count: usize) -> Self {
        let order: Vec<usize> = (0..page_count).collect();
        Self {
            saved: order.clone(),
            order,
            source_len: page_count,
            history: Vec::new(),
        }
    }

    /// How many pages are shown.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether no pages are left.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Pages in the document as opened, however many have since been deleted.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source_len
    }

    /// The source page shown at `position`, or `None` if there is no such position.
    ///
    /// The one place a display position becomes a source page. Everything that
    /// rasterizes, caches or looks up geometry goes through here.
    #[must_use]
    pub fn source_of(&self, position: usize) -> Option<usize> {
        self.order.get(position).copied()
    }

    /// Source pages in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[usize] {
        &self.order
    }

    /// Whether this matches what is on disk.
    ///
    /// What "unsaved changes" means, and what makes saving over the file a no-op. True
    /// for a freshly opened document, false after any edit, and true again once a save
    /// of that exact order has reported success.
    #[must_use]
    pub fn is_unedited(&self) -> bool {
        self.order == self.saved
    }

    /// Records that `written` is now what the file contains.
    ///
    /// Takes the order that was actually written rather than assuming it is the current
    /// one. A save runs off the UI thread and takes about a second on a 400-page
    /// document, so the pages may well have been moved again while it ran — and marking
    /// *those* moves as saved would tell somebody their work is on disk when it is not.
    /// Passing the written order through makes that case come out right on its own
    /// rather than needing to be noticed.
    pub fn mark_saved(&mut self, written: &[usize]) {
        self.saved = written.to_vec();
    }

    /// Whether there is anything to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.history.is_empty()
    }

    /// Moves the page at `from` so that it sits at `to`.
    ///
    /// Returns whether anything changed. A move to where the page already is, or with
    /// either position out of range, changes nothing and is not an error — the same
    /// convention the view commands use, where being already where you asked to go is
    /// `Unchanged` rather than a rejection.
    pub fn move_page(&mut self, from: usize, to: usize) -> bool {
        self.move_pages(&[from], to)
    }

    /// Moves every page in `positions` so the group starts at `to`, keeping their
    /// relative order.
    ///
    /// `to` is where the group ends up **after** the move, which is the contract
    /// [`Self::move_page`] has always had — not "insert before whatever is at `to`
    /// now". The two spellings disagree whenever the group moves forward, so the choice
    /// matters: `move_pages(&[0], 2)` on `[0,1,2,3]` gives `[1,2,0,3]`, with the moved
    /// page third as asked.
    ///
    /// One undo step for the whole group, which is the reason this is not a loop over
    /// [`Self::move_page`] at the call site: dragging five pages and pressing undo once
    /// has to put all five back.
    ///
    /// Duplicate and unordered positions are fine. Out of range in either argument
    /// changes nothing, as above.
    pub fn move_pages(&mut self, positions: &[usize], to: usize) -> bool {
        let Some(taken) = self.normalize(positions) else {
            return false;
        };
        if to >= self.order.len() {
            return false;
        }

        // Clamp so the group lands wholly inside the document. `to` is already known to
        // be a valid position, so for a single page this can never bite — which is what
        // keeps `move_page`'s existing behaviour exactly as it was.
        let landing = to.min(self.order.len() - taken.len());

        let group: Vec<usize> = taken
            .iter()
            .filter_map(|&position| self.order.get(position).copied())
            .collect();
        let mut rest: Vec<usize> = self
            .order
            .iter()
            .enumerate()
            .filter(|(position, _)| !taken.contains(position))
            .map(|(_, &page)| page)
            .collect();
        rest.splice(landing..landing, group);

        if rest == self.order {
            return false;
        }
        self.remember();
        self.order = rest;
        true
    }

    /// Removes the page at `position`.
    ///
    /// Refuses to remove the last one: a PDF with no pages is not a valid PDF, and
    /// producing one would be a worse outcome than declining.
    pub fn remove(&mut self, position: usize) -> bool {
        self.remove_pages(&[position])
    }

    /// Removes every page in `positions`, as one undo step.
    ///
    /// Refuses to remove them *all*, for the reason [`Self::remove`] refuses the last
    /// one. Note that it refuses rather than deleting all but one: which page a person
    /// did not mean to delete is not something this can guess, and leaving the selection
    /// intact lets them narrow it.
    pub fn remove_pages(&mut self, positions: &[usize]) -> bool {
        let Some(taken) = self.normalize(positions) else {
            return false;
        };
        if taken.len() == self.order.len() {
            return false;
        }
        self.remember();
        self.order = self
            .order
            .iter()
            .enumerate()
            .filter(|(position, _)| !taken.contains(position))
            .map(|(_, &page)| page)
            .collect();
        true
    }

    /// Sorted, deduplicated, in-range positions — or `None` if there is nothing to act
    /// on.
    ///
    /// Shared by both group edits so "which positions did you mean" is answered once.
    /// A single out-of-range position makes the whole call a no-op rather than being
    /// dropped silently: a caller that asked to move five pages and got four moved
    /// would have no way to tell.
    fn normalize(&self, positions: &[usize]) -> Option<Vec<usize>> {
        if positions.is_empty() || positions.iter().any(|&p| p >= self.order.len()) {
            return None;
        }
        let mut taken = positions.to_vec();
        taken.sort_unstable();
        taken.dedup();
        Some(taken)
    }

    /// Goes back one step. Returns whether there was a step to go back to.
    pub fn undo(&mut self) -> bool {
        match self.history.pop() {
            Some(previous) => {
                self.order = previous;
                true
            }
            None => false,
        }
    }

    /// Records the current order so [`Self::undo`] can return to it.
    fn remember(&mut self) {
        if self.history.len() == UNDO_DEPTH {
            // Drop the oldest. A session that has made 64 edits cares far more about
            // the last few than the first.
            self.history.remove(0);
        }
        self.history.push(self.order.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_order_is_the_document_as_opened() {
        let order = PageOrder::identity(4);
        assert_eq!(order.as_slice(), &[0, 1, 2, 3]);
        assert_eq!(order.len(), 4);
        assert_eq!(order.source_len(), 4);
        assert!(order.is_unedited());
        assert!(!order.can_undo());
    }

    #[test]
    fn a_position_maps_to_a_source_page() {
        let order = PageOrder::identity(3);
        assert_eq!(order.source_of(0), Some(0));
        assert_eq!(order.source_of(2), Some(2));
        assert_eq!(order.source_of(3), None, "past the end");
    }

    #[test]
    fn moving_a_page_forward_shifts_the_rest_back() {
        let mut order = PageOrder::identity(5);
        assert!(order.move_page(0, 2));
        assert_eq!(order.as_slice(), &[1, 2, 0, 3, 4]);
        assert_eq!(
            order.source_of(2),
            Some(0),
            "the moved page is now shown third"
        );
    }

    #[test]
    fn moving_a_page_backward_shifts_the_rest_forward() {
        let mut order = PageOrder::identity(5);
        assert!(order.move_page(3, 1));
        assert_eq!(order.as_slice(), &[0, 3, 1, 2, 4]);
    }

    #[test]
    fn moving_the_last_page_to_the_front_reverses_nothing_else() {
        let mut order = PageOrder::identity(3);
        assert!(order.move_page(2, 0));
        assert_eq!(order.as_slice(), &[2, 0, 1]);
    }

    #[test]
    fn a_move_that_changes_nothing_reports_so() {
        let mut order = PageOrder::identity(3);
        assert!(!order.move_page(1, 1), "moved a page onto itself");
        assert!(!order.move_page(0, 9), "moved past the end");
        assert!(!order.move_page(9, 0), "moved from past the end");
        assert_eq!(order.as_slice(), &[0, 1, 2]);
        assert!(
            !order.can_undo(),
            "a move that did nothing still recorded history"
        );
    }

    #[test]
    fn deleting_a_page_drops_it_from_the_order() {
        let mut order = PageOrder::identity(4);
        assert!(order.remove(1));
        assert_eq!(order.as_slice(), &[0, 2, 3]);
        assert_eq!(order.len(), 3);
        assert_eq!(
            order.source_len(),
            4,
            "the source document still has four pages"
        );
    }

    #[test]
    fn the_last_page_cannot_be_deleted() {
        // A PDF with no pages is not a valid PDF, so producing one would be worse
        // than declining.
        let mut order = PageOrder::identity(1);
        assert!(!order.remove(0));
        assert_eq!(order.len(), 1);
    }

    #[test]
    fn deleting_past_the_end_changes_nothing() {
        let mut order = PageOrder::identity(3);
        assert!(!order.remove(7));
        assert_eq!(order.as_slice(), &[0, 1, 2]);
    }

    // --- Group edits ---------------------------------------------------------

    #[test]
    fn a_group_moves_as_a_block_and_keeps_its_order() {
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[0, 1], 3));
        assert_eq!(order.as_slice(), &[2, 3, 4, 0, 1, 5]);
        assert_eq!(
            order.source_of(3),
            Some(0),
            "the group should start where it was asked to"
        );
    }

    #[test]
    fn a_group_moving_backward_lands_where_asked() {
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[3, 4], 1));
        assert_eq!(order.as_slice(), &[0, 3, 4, 1, 2, 5]);
    }

    #[test]
    fn a_scattered_group_is_gathered_together() {
        // Picking pages out of a long document and dropping them side by side is the
        // point of selecting more than one, so they arrive contiguous.
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[0, 2, 4], 2));
        assert_eq!(order.as_slice(), &[1, 3, 0, 2, 4, 5]);
    }

    #[test]
    fn a_group_dropped_past_the_end_lands_flush_against_it() {
        // Clamped rather than refused: dragging three pages at the bottom of the grid
        // means "put them last", and refusing would read as the drag not working.
        let mut order = PageOrder::identity(5);
        assert!(order.move_pages(&[0, 1, 2], 4));
        assert_eq!(order.as_slice(), &[3, 4, 0, 1, 2]);
    }

    #[test]
    fn a_single_page_group_moves_exactly_like_move_page() {
        // The two share one implementation, and this is what pins that they agree —
        // `move_page`'s contract predates groups and its callers depend on it.
        for from in 0..5 {
            for to in 0..5 {
                let mut one = PageOrder::identity(5);
                let mut group = PageOrder::identity(5);
                assert_eq!(
                    one.move_page(from, to),
                    group.move_pages(&[from], to),
                    "{from} -> {to} disagreed about whether anything changed"
                );
                assert_eq!(one.as_slice(), group.as_slice(), "{from} -> {to}");
            }
        }
    }

    #[test]
    fn moving_a_group_onto_itself_changes_nothing() {
        let mut order = PageOrder::identity(5);
        assert!(!order.move_pages(&[1, 2], 1));
        assert_eq!(order.as_slice(), &[0, 1, 2, 3, 4]);
        assert!(
            !order.can_undo(),
            "a move that did nothing recorded history"
        );
    }

    #[test]
    fn moving_every_page_at_once_changes_nothing() {
        let mut order = PageOrder::identity(4);
        assert!(!order.move_pages(&[0, 1, 2, 3], 0));
        assert_eq!(order.as_slice(), &[0, 1, 2, 3]);
    }

    #[test]
    fn a_group_edit_is_one_undo_step() {
        // The reason these are not loops over the single-page versions. Dragging five
        // pages and pressing undo once has to put all five back, not one of them.
        let mut order = PageOrder::identity(6);
        order.move_pages(&[0, 1, 2], 3);
        assert_eq!(order.as_slice(), &[3, 4, 5, 0, 1, 2]);
        assert!(order.undo());
        assert_eq!(order.as_slice(), &[0, 1, 2, 3, 4, 5]);
        assert!(!order.can_undo(), "one drag left more than one step behind");
    }

    #[test]
    fn a_group_delete_is_one_undo_step() {
        let mut order = PageOrder::identity(6);
        assert!(order.remove_pages(&[1, 3, 5]));
        assert_eq!(order.as_slice(), &[0, 2, 4]);
        assert!(order.undo());
        assert_eq!(order.as_slice(), &[0, 1, 2, 3, 4, 5]);
        assert!(!order.can_undo());
    }

    #[test]
    fn duplicate_and_unordered_positions_are_accepted() {
        // The UI hands over a set in click order, which is neither sorted nor
        // necessarily unique once a range overlaps an earlier pick.
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[4, 0, 4, 0], 2));
        assert_eq!(order.as_slice(), &[1, 2, 0, 4, 3, 5]);
    }

    #[test]
    fn deleting_every_page_is_refused() {
        // Refused, not "all but one": which page was not meant to go is not something
        // this can guess.
        let mut order = PageOrder::identity(3);
        assert!(!order.remove_pages(&[0, 1, 2]));
        assert_eq!(order.as_slice(), &[0, 1, 2]);
        assert!(!order.can_undo());
    }

    #[test]
    fn one_bad_position_makes_the_whole_group_edit_a_no_op() {
        // Rather than silently acting on the rest. A caller that asked to move three
        // pages and got two moved has no way to notice.
        let mut order = PageOrder::identity(4);
        assert!(!order.move_pages(&[0, 1, 9], 2));
        assert_eq!(order.as_slice(), &[0, 1, 2, 3]);
        assert!(!order.remove_pages(&[0, 9]));
        assert_eq!(order.as_slice(), &[0, 1, 2, 3]);
    }

    #[test]
    fn an_empty_group_changes_nothing() {
        let mut order = PageOrder::identity(3);
        assert!(!order.move_pages(&[], 1));
        assert!(!order.remove_pages(&[]));
        assert_eq!(order.as_slice(), &[0, 1, 2]);
    }

    #[test]
    fn a_group_edit_never_loses_or_duplicates_a_page() {
        // The invariant the render path depends on, exercised over the group edits the
        // way the single-page ones already are below.
        let mut order = PageOrder::identity(8);
        order.move_pages(&[0, 3, 7], 2);
        order.remove_pages(&[1, 4]);
        order.move_pages(&[0, 1], 4);

        let mut seen = order.as_slice().to_vec();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "a page appeared twice: {order:?}");
        for &source in order.as_slice() {
            assert!(source < order.source_len(), "invented page {source}");
        }
    }

    #[test]
    fn undo_goes_back_one_edit_at_a_time() {
        let mut order = PageOrder::identity(4);
        order.move_page(0, 3); // [1,2,3,0]
        order.remove(0); // [2,3,0]
        assert_eq!(order.as_slice(), &[2, 3, 0]);

        assert!(order.undo());
        assert_eq!(order.as_slice(), &[1, 2, 3, 0], "one step back");

        assert!(order.undo());
        assert_eq!(order.as_slice(), &[0, 1, 2, 3], "back to the start");
        assert!(order.is_unedited());
    }

    #[test]
    fn undo_past_the_beginning_is_a_no_op() {
        let mut order = PageOrder::identity(2);
        assert!(!order.undo());
        assert_eq!(order.as_slice(), &[0, 1]);
    }

    // --- What is on disk -----------------------------------------------------

    #[test]
    fn saving_makes_the_order_unedited_again() {
        // The whole point. Before this, a saved document went on claiming unsaved
        // changes forever: the status bar nagged and the Save button stayed lit even
        // though the file matched exactly.
        let mut order = PageOrder::identity(4);
        order.move_page(0, 3);
        assert!(!order.is_unedited());

        let written = order.as_slice().to_vec();
        order.mark_saved(&written);
        assert!(
            order.is_unedited(),
            "still dirty after saving that exact order"
        );
    }

    #[test]
    fn editing_after_a_save_is_unsaved_again() {
        let mut order = PageOrder::identity(4);
        order.move_page(0, 3);
        let written = order.as_slice().to_vec();
        order.mark_saved(&written);

        order.move_page(1, 2);
        assert!(!order.is_unedited(), "an edit after a save reported clean");
    }

    #[test]
    fn an_edit_made_while_the_save_was_running_is_still_unsaved() {
        // The race the `written` argument exists for. A save takes about a second on a
        // 400-page document, so there is a real window in which pages get moved again.
        // Marking those as saved would tell somebody their work is on disk when only
        // the earlier version is.
        let mut order = PageOrder::identity(5);
        order.move_page(0, 4);
        let written = order.as_slice().to_vec(); // what the save thread took

        // ... and while it was writing, another move.
        order.move_page(1, 2);
        order.mark_saved(&written);

        assert!(
            !order.is_unedited(),
            "claimed the later move was written when the save never saw it"
        );

        // Undoing that later move lands back on exactly what was written.
        assert!(order.undo());
        assert!(
            order.is_unedited(),
            "back to the written order but reported dirty"
        );
    }

    #[test]
    fn undoing_back_to_the_saved_order_counts_as_unedited() {
        let mut order = PageOrder::identity(3);
        order.move_page(0, 2);
        let written = order.as_slice().to_vec();
        order.mark_saved(&written);
        order.remove(1);
        assert!(!order.is_unedited());

        assert!(order.undo());
        assert!(
            order.is_unedited(),
            "undo back to the file's order reported dirty"
        );
    }

    #[test]
    fn a_fresh_document_matches_its_file() {
        // Nothing has been written, but nothing has been changed either, so there is
        // nothing to lose — and saving it over itself would be pointless.
        assert!(PageOrder::identity(7).is_unedited());
        assert!(PageOrder::identity(0).is_unedited());
    }

    #[test]
    fn an_order_that_has_been_edited_and_put_back_counts_as_unedited() {
        // What "unsaved changes" is asked of. Moving a page and moving it back leaves
        // nothing to write, and claiming otherwise would nag about saving a document
        // that matches the file on disk.
        let mut order = PageOrder::identity(3);
        order.move_page(0, 2);
        assert!(!order.is_unedited());
        order.move_page(2, 0);
        assert!(order.is_unedited());
    }

    #[test]
    fn deleting_a_page_always_counts_as_edited() {
        let mut order = PageOrder::identity(3);
        order.remove(2);
        assert!(
            !order.is_unedited(),
            "a shorter document than the source is edited by definition"
        );
    }

    #[test]
    fn history_is_bounded() {
        // 64 copies of a 400-page order is about 200 KB; unbounded, a long session
        // would grow without limit.
        let mut order = PageOrder::identity(3);
        for _ in 0..(UNDO_DEPTH * 2) {
            order.move_page(0, 1);
            order.move_page(1, 0);
        }
        assert_eq!(order.history.len(), UNDO_DEPTH);

        // And it still unwinds cleanly rather than getting stuck.
        let mut steps = 0;
        while order.undo() {
            steps += 1;
        }
        assert_eq!(steps, UNDO_DEPTH);
    }

    #[test]
    fn every_position_maps_to_a_real_source_page_after_any_edit() {
        // The invariant the render path depends on: whatever the order, asking for the
        // page at a valid position gives a page the source document actually has.
        let mut order = PageOrder::identity(6);
        order.move_page(5, 0);
        order.remove(3);
        order.move_page(0, 4);
        order.remove(0);

        for position in 0..order.len() {
            let source = order.source_of(position).expect("a source page");
            assert!(
                source < order.source_len(),
                "position {position} -> {source}"
            );
        }
        // And no page appears twice, which a careless move-then-insert would cause.
        let mut seen = order.as_slice().to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), order.len(), "a page appeared twice: {order:?}");
    }

    #[test]
    fn an_empty_document_has_an_empty_order() {
        let order = PageOrder::identity(0);
        assert!(order.is_empty());
        assert_eq!(order.source_of(0), None);
        assert!(order.is_unedited());
    }
}
