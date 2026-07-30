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
        Self {
            order: (0..page_count).collect(),
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

    /// Whether this still matches the document as opened.
    ///
    /// What "unsaved changes" means, and what makes saving over the source a no-op.
    #[must_use]
    pub fn is_unedited(&self) -> bool {
        self.order.len() == self.source_len && self.order.iter().enumerate().all(|(i, s)| i == *s)
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
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return false;
        }
        self.remember();
        let page = self.order.remove(from);
        self.order.insert(to, page);
        true
    }

    /// Removes the page at `position`.
    ///
    /// Refuses to remove the last one: a PDF with no pages is not a valid PDF, and
    /// producing one would be a worse outcome than declining.
    pub fn remove(&mut self, position: usize) -> bool {
        if position >= self.order.len() || self.order.len() == 1 {
            return false;
        }
        self.remember();
        self.order.remove(position);
        true
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
