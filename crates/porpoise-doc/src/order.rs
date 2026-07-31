//! What order the pages are in, and how to change it.
//!
//! An edit never touches a file. It is held as a list of **sources in display
//! order**, starting at one entry per page of the document the viewer was opened
//! with. Moving a page reorders the list; deleting one drops an entry; inserting
//! another document's pages appends entries naming it. The document on disk
//! changes only on save.
//!
//! Pure arithmetic over a `Vec<Source>` — no PDF, no `lopdf`, no window. Two useful
//! consequences: undo is a snapshot rather than an inverse operation, and a reorder
//! invalidates no rendered pages, because page textures stay keyed by source.
//!
//! # Display position is not source page — and now not source document either
//!
//! After any edit there are two page numbers in play and both are `usize`. This
//! codebase has been caught by that shape three times already — pixels versus PDF
//! points, zero-based indices versus one-based numbers, screen units versus document
//! units. Every crossing here goes through [`PageOrder::source_of`], and variables
//! are named `position` or `source`, never `page`. See `docs/goal-4-plan.md` §3.
//!
//! Merging pages from more than one document (`docs/goal-5-plan.md` §3) adds a third
//! axis — *which* document — so a variable naming one is called `document`, never
//! `page` or `source` on its own. [`Source`] bundles a document with a page
//! precisely so the two travel together and cannot be paired up wrong.
//!
//! # A source's identity outlives its file's layout
//!
//! `Source { document, page }` names a page once, at open or insert time, and never
//! again — `move_page` and `remove` only reorder or drop entries, and `append` only
//! ever hands out fresh ones. But saving physically rewrites a document's file,
//! compacted to hold only the retained pages and renumbered from zero, so a second
//! save over the same path would be reading a file whose page `n` is no longer the
//! `n` any `Source` still names. [`PageOrder::on_disk`] is the record that keeps the
//! two apart: what a `Source` means never changes, but what a document's *file*
//! currently holds does, and only [`PageOrder::mark_saved`] moves it. See
//! `docs/goal-5-plan.md` §9a, where saving twice over the same path was found to
//! either refuse a save that should succeed, or write the wrong pages without
//! complaint.

/// A page in one of the documents contributing to what is shown.
///
/// `document` is an index the caller hands in, not something this module invents —
/// this crate knows no PDF, no path and no `lopdf`, and a document index is exactly
/// the kind of thing that stays true of. Which path that index names, and what
/// parsed document backs it, is bookkeeping the viewer owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Source {
    /// Which contributing document, in the order it was added to the session. `0`
    /// is the document the viewer was opened with.
    pub document: usize,
    /// A page index within that document.
    pub page: usize,
}

impl Source {
    /// A page of the first — and, before any page is inserted from elsewhere, only
    /// — contributing document.
    fn primary(page: usize) -> Self {
        Self { document: 0, page }
    }
}

/// How many undo steps are remembered.
///
/// Each step is a copy of the order — 3.2 KB for a 400-page document — so this is
/// bounded to keep a long session from growing without limit rather than because the
/// copies are expensive.
const UNDO_DEPTH: usize = 64;

/// The order pages are shown in, and the history to undo it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOrder {
    /// Sources, in display order.
    order: Vec<Source>,
    /// The order as it stands in the file. Starts equal to [`Self::order`] and moves
    /// only when a save reports success.
    ///
    /// This is what makes "unsaved changes" mean it, rather than meaning "differs from
    /// the document as first opened". Without it a saved document goes on claiming
    /// changes forever — the status bar nags, the Save button stays lit, and anything
    /// built on top warns when there is nothing left to lose. A warning that fires when
    /// nothing is at risk is one people learn to click through.
    saved: Vec<Source>,
    /// Pages in each contributing document as it was added, indexed by
    /// [`Source::document`]. Needed to tell an edited order from a fresh one even
    /// after pages are deleted, and to bounds-check a source against the right
    /// document rather than any of them.
    ///
    /// This never changes after a document is opened or inserted, even once a save
    /// has rewritten its file — see [`Self::on_disk`] for the count that does.
    source_lens: Vec<usize>,
    /// The [`Source`]s each contributing document's file currently holds,
    /// physically, in the order they sit in that file — indexed by
    /// [`Source::document`], one list per document, each starting as
    /// `document`'s pages in order (`0, 1, 2, ...`).
    ///
    /// A [`Source`]'s identity never changes: `move_page` and `remove` only reorder
    /// or drop entries, and `append` only ever hands out fresh ones. But a save
    /// physically rewrites a document's file, compacted to hold only the retained
    /// pages and renumbered from zero — so the *file* no longer agrees that its
    /// physical page `n` is `Source`'s page `n`. [`Self::mark_saved`] is the only
    /// thing that changes this, recording what a document's file now holds so a
    /// later save can still translate a stable [`Source`] into the right physical
    /// page of that file. Without it, a second save over the same path either
    /// refuses a save that should succeed, or — if the two page counts happen to
    /// still agree — writes the wrong pages without complaint. See
    /// `docs/goal-5-plan.md` §9a, where this was found.
    ///
    /// Saving a merge into a document's own path folds every contributing file into
    /// that one file, so `on_disk[document]` after such a save can list `Source`s
    /// that named a *different* document before the save — accurate, since they are
    /// now physically inside `document`'s file too. This is never a problem: a
    /// lookup for one of those `Source`s still goes through *its own* document's
    /// record, which was never touched and still finds it in its own, untouched
    /// file.
    on_disk: Vec<Vec<Source>>,
    /// Previous orders, most recent last.
    history: Vec<Vec<Source>>,
}

impl PageOrder {
    /// The unedited order of a document with `page_count` pages.
    #[must_use]
    pub fn identity(page_count: usize) -> Self {
        let order: Vec<Source> = (0..page_count).map(Source::primary).collect();
        Self {
            saved: order.clone(),
            on_disk: vec![order.clone()],
            order,
            source_lens: vec![page_count],
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

    /// Pages in each contributing document as it was added, indexed by document.
    /// However many pages have since been deleted, this still names the document's
    /// full page count — what "opened with" or "inserted with" meant at the time.
    #[must_use]
    pub fn source_lens(&self) -> &[usize] {
        &self.source_lens
    }

    /// The [`Source`]s `document`'s file currently holds, physically, in the order
    /// they sit in that file. `None` if `document` is out of range for
    /// [`Self::document_count`].
    ///
    /// This is what a save reads instead of assuming a document's file still agrees
    /// with `source.page` — see the field docs on the private `on_disk` this
    /// exposes.
    #[must_use]
    pub fn on_disk(&self, document: usize) -> Option<&[Source]> {
        self.on_disk.get(document).map(Vec::as_slice)
    }

    /// How many documents contribute pages right now, including the one this was
    /// opened with. The index a fresh call to [`Self::append`] should use.
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.source_lens.len()
    }

    /// The source shown at `position`, or `None` if there is no such position.
    ///
    /// The one place a display position becomes a source. Everything that
    /// rasterizes, caches or looks up geometry goes through here.
    #[must_use]
    pub fn source_of(&self, position: usize) -> Option<Source> {
        self.order.get(position).copied()
    }

    /// Sources in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[Source] {
        &self.order
    }

    /// Appends every page of another contributing document to the end of the
    /// order, as one undo step.
    ///
    /// `document` is the index future sources should carry for this document —
    /// ordinarily [`Self::document_count`], since a document already known is never
    /// reused: the file on disk may have changed between two inserts of the "same"
    /// path, and silently treating it as unchanged would be exactly the kind of
    /// quiet incorrectness this project refuses to ship.
    ///
    /// A no-op, reporting so, when `page_count` is zero: there is nothing to add,
    /// and an empty document contributing nothing is not an edit.
    pub fn append(&mut self, document: usize, page_count: usize) -> bool {
        if page_count == 0 {
            return false;
        }
        self.remember();
        if document >= self.source_lens.len() {
            self.source_lens.resize(document + 1, 0);
            self.on_disk.resize(document + 1, Vec::new());
        }
        if let Some(len) = self.source_lens.get_mut(document) {
            *len = page_count;
        }
        let pages: Vec<Source> = (0..page_count).map(|page| Source { document, page }).collect();
        if let Some(slot) = self.on_disk.get_mut(document) {
            *slot = pages.clone();
        }
        self.order.extend(pages);
        true
    }

    /// Registers a document [`Self::insert_pages`] can later place pages from,
    /// without adding any of them to the display order yet.
    ///
    /// `document` is the index later calls should use for it — ordinarily
    /// [`Self::document_count`], for the same reason [`Self::append`] never reuses
    /// one: the file on disk may have changed since it was last read, and treating
    /// it as unchanged would be exactly the kind of quiet incorrectness this
    /// project refuses to ship. Replacing a staged document with a different one is
    /// done by staging the new one at a fresh index, not by re-staging over the
    /// old one — the old index is simply never referenced again. Calling this
    /// again on a document already contributing pages to the order is a caller
    /// error this module does not defend against, the same as calling `append`
    /// twice on the same index already was not.
    ///
    /// Not an edit: nothing about what is shown changes, so unlike [`Self::append`]
    /// this touches neither [`Self::as_slice`] nor the undo history — only
    /// [`Self::source_lens`] and [`Self::on_disk`], exactly the way `append`'s own
    /// bookkeeping half already does.
    ///
    /// A no-op, reporting so, when `page_count` is zero: there is nothing to ever
    /// place from an empty document.
    pub fn stage(&mut self, document: usize, page_count: usize) -> bool {
        if page_count == 0 {
            return false;
        }
        if document >= self.source_lens.len() {
            self.source_lens.resize(document + 1, 0);
            self.on_disk.resize(document + 1, Vec::new());
        }
        if let Some(len) = self.source_lens.get_mut(document) {
            *len = page_count;
        }
        let pages: Vec<Source> = (0..page_count).map(|page| Source { document, page }).collect();
        if let Some(slot) = self.on_disk.get_mut(document) {
            *slot = pages;
        }
        true
    }

    /// Inserts `pages` of `document` — already known via [`Self::stage`] or
    /// [`Self::append`] — as a contiguous block landing at `position`, in the order
    /// given, as one undo step.
    ///
    /// What [`Self::append`] cannot express: `append` always takes every page of a
    /// document and always lands at the end. This takes however many pages one
    /// selection carried, in whatever order they were given, and lands them
    /// wherever asked — mid-document or not, one page or several — without ever
    /// costing more than one undo. Calling it more than once against the same
    /// `document` is expected: a document can be staged once and drawn from by
    /// several separate drags.
    ///
    /// A no-op, reporting so, rather than inserting anything or touching history,
    /// if `pages` is empty, if `document` is not yet known to this order, if any
    /// page named is out of range for it, or if `position` is past the end of the
    /// order. Refusing the whole call rather than inserting whichever entries were
    /// valid is the same convention [`Self::move_pages`] and [`Self::remove_pages`]
    /// already use: a caller that asked for five pages and got three inserted would
    /// have no way to notice.
    ///
    /// Duplicate pages within `pages` are not rejected. Inserting the same page of
    /// `document` more than once is unusual, not invalid — nothing stops picking it
    /// out of the staging viewport a second time — and this module does not guess
    /// at an intention it was not asked about.
    pub fn insert_pages(&mut self, document: usize, pages: &[usize], position: usize) -> bool {
        if pages.is_empty() || position > self.order.len() {
            return false;
        }
        let Some(&len) = self.source_lens.get(document) else {
            return false;
        };
        if pages.iter().any(|&page| page >= len) {
            return false;
        }

        self.remember();
        let group = pages.iter().map(|&page| Source { document, page });
        self.order.splice(position..position, group);
        true
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

    /// Records that `document`'s file was just rewritten to hold exactly `written`,
    /// physically in that order — and that `written` is now what the whole order
    /// last matched on disk.
    ///
    /// Takes the order that was actually written rather than assuming it is the current
    /// one. A save runs off the UI thread and takes about a second on a 400-page
    /// document, so the pages may well have been moved again while it ran — and marking
    /// *those* moves as saved would tell somebody their work is on disk when it is not.
    /// Passing the written order through makes that case come out right on its own
    /// rather than needing to be noticed.
    ///
    /// `document` is whichever document's file `written` was saved to — in this
    /// project, always the one the viewer was opened with, since every save replaces
    /// that file (`docs/goal-5-plan.md` §9a). Recording it in [`Self::on_disk`] is
    /// what lets a later save over the same path find the right page a second time:
    /// without it, that file's physical layout has moved out from under the
    /// [`Source`]s naming it, and a second save either refuses one that should
    /// succeed or writes the wrong pages without complaint.
    pub fn mark_saved(&mut self, document: usize, written: &[Source]) {
        self.saved = written.to_vec();
        if let Some(slot) = self.on_disk.get_mut(document) {
            *slot = written.to_vec();
        }
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

        let group: Vec<Source> = taken
            .iter()
            .filter_map(|&position| self.order.get(position).copied())
            .collect();
        let mut rest: Vec<Source> = self
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

    /// A page of the primary document — shorthand for the document every test
    /// predating [`Source`] only ever needed.
    fn p(page: usize) -> Source {
        Source::primary(page)
    }

    /// Sources naming an arbitrary document, for building an expected order.
    fn from_document(document: usize, pages: impl IntoIterator<Item = usize>) -> Vec<Source> {
        pages.into_iter().map(|page| Source { document, page }).collect()
    }

    /// A run of primary-document sources, for comparing against `as_slice()`.
    fn primary(pages: impl IntoIterator<Item = usize>) -> Vec<Source> {
        from_document(0, pages)
    }

    #[test]
    fn a_fresh_order_is_the_document_as_opened() {
        let order = PageOrder::identity(4);
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3]));
        assert_eq!(order.len(), 4);
        assert_eq!(order.source_lens(), &[4]);
        assert_eq!(order.document_count(), 1);
        assert!(order.is_unedited());
        assert!(!order.can_undo());
    }

    #[test]
    fn a_position_maps_to_a_source_page() {
        let order = PageOrder::identity(3);
        assert_eq!(order.source_of(0), Some(p(0)));
        assert_eq!(order.source_of(2), Some(p(2)));
        assert_eq!(order.source_of(3), None, "past the end");
    }

    #[test]
    fn moving_a_page_forward_shifts_the_rest_back() {
        let mut order = PageOrder::identity(5);
        assert!(order.move_page(0, 2));
        assert_eq!(order.as_slice(), primary([1, 2, 0, 3, 4]));
        assert_eq!(
            order.source_of(2),
            Some(p(0)),
            "the moved page is now shown third"
        );
    }

    #[test]
    fn moving_a_page_backward_shifts_the_rest_forward() {
        let mut order = PageOrder::identity(5);
        assert!(order.move_page(3, 1));
        assert_eq!(order.as_slice(), primary([0, 3, 1, 2, 4]));
    }

    #[test]
    fn moving_the_last_page_to_the_front_reverses_nothing_else() {
        let mut order = PageOrder::identity(3);
        assert!(order.move_page(2, 0));
        assert_eq!(order.as_slice(), primary([2, 0, 1]));
    }

    #[test]
    fn a_move_that_changes_nothing_reports_so() {
        let mut order = PageOrder::identity(3);
        assert!(!order.move_page(1, 1), "moved a page onto itself");
        assert!(!order.move_page(0, 9), "moved past the end");
        assert!(!order.move_page(9, 0), "moved from past the end");
        assert_eq!(order.as_slice(), primary([0, 1, 2]));
        assert!(
            !order.can_undo(),
            "a move that did nothing still recorded history"
        );
    }

    #[test]
    fn deleting_a_page_drops_it_from_the_order() {
        let mut order = PageOrder::identity(4);
        assert!(order.remove(1));
        assert_eq!(order.as_slice(), primary([0, 2, 3]));
        assert_eq!(order.len(), 3);
        assert_eq!(
            order.source_lens(),
            &[4],
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
        assert_eq!(order.as_slice(), primary([0, 1, 2]));
    }

    // --- Group edits ---------------------------------------------------------

    #[test]
    fn a_group_moves_as_a_block_and_keeps_its_order() {
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[0, 1], 3));
        assert_eq!(order.as_slice(), primary([2, 3, 4, 0, 1, 5]));
        assert_eq!(
            order.source_of(3),
            Some(p(0)),
            "the group should start where it was asked to"
        );
    }

    #[test]
    fn a_group_moving_backward_lands_where_asked() {
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[3, 4], 1));
        assert_eq!(order.as_slice(), primary([0, 3, 4, 1, 2, 5]));
    }

    #[test]
    fn a_scattered_group_is_gathered_together() {
        // Picking pages out of a long document and dropping them side by side is the
        // point of selecting more than one, so they arrive contiguous.
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[0, 2, 4], 2));
        assert_eq!(order.as_slice(), primary([1, 3, 0, 2, 4, 5]));
    }

    #[test]
    fn a_group_dropped_past_the_end_lands_flush_against_it() {
        // Clamped rather than refused: dragging three pages at the bottom of the grid
        // means "put them last", and refusing would read as the drag not working.
        let mut order = PageOrder::identity(5);
        assert!(order.move_pages(&[0, 1, 2], 4));
        assert_eq!(order.as_slice(), primary([3, 4, 0, 1, 2]));
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
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3, 4]));
        assert!(
            !order.can_undo(),
            "a move that did nothing recorded history"
        );
    }

    #[test]
    fn moving_every_page_at_once_changes_nothing() {
        let mut order = PageOrder::identity(4);
        assert!(!order.move_pages(&[0, 1, 2, 3], 0));
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3]));
    }

    #[test]
    fn a_group_edit_is_one_undo_step() {
        // The reason these are not loops over the single-page versions. Dragging five
        // pages and pressing undo once has to put all five back, not one of them.
        let mut order = PageOrder::identity(6);
        order.move_pages(&[0, 1, 2], 3);
        assert_eq!(order.as_slice(), primary([3, 4, 5, 0, 1, 2]));
        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3, 4, 5]));
        assert!(!order.can_undo(), "one drag left more than one step behind");
    }

    #[test]
    fn a_group_delete_is_one_undo_step() {
        let mut order = PageOrder::identity(6);
        assert!(order.remove_pages(&[1, 3, 5]));
        assert_eq!(order.as_slice(), primary([0, 2, 4]));
        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3, 4, 5]));
        assert!(!order.can_undo());
    }

    #[test]
    fn duplicate_and_unordered_positions_are_accepted() {
        // The UI hands over a set in click order, which is neither sorted nor
        // necessarily unique once a range overlaps an earlier pick.
        let mut order = PageOrder::identity(6);
        assert!(order.move_pages(&[4, 0, 4, 0], 2));
        assert_eq!(order.as_slice(), primary([1, 2, 0, 4, 3, 5]));
    }

    #[test]
    fn deleting_every_page_is_refused() {
        // Refused, not "all but one": which page was not meant to go is not something
        // this can guess.
        let mut order = PageOrder::identity(3);
        assert!(!order.remove_pages(&[0, 1, 2]));
        assert_eq!(order.as_slice(), primary([0, 1, 2]));
        assert!(!order.can_undo());
    }

    #[test]
    fn one_bad_position_makes_the_whole_group_edit_a_no_op() {
        // Rather than silently acting on the rest. A caller that asked to move three
        // pages and got two moved has no way to notice.
        let mut order = PageOrder::identity(4);
        assert!(!order.move_pages(&[0, 1, 9], 2));
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3]));
        assert!(!order.remove_pages(&[0, 9]));
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3]));
    }

    #[test]
    fn an_empty_group_changes_nothing() {
        let mut order = PageOrder::identity(3);
        assert!(!order.move_pages(&[], 1));
        assert!(!order.remove_pages(&[]));
        assert_eq!(order.as_slice(), primary([0, 1, 2]));
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
            assert!(
                source.page < order.source_lens()[source.document],
                "invented page {source:?}"
            );
        }
    }

    #[test]
    fn undo_goes_back_one_edit_at_a_time() {
        let mut order = PageOrder::identity(4);
        order.move_page(0, 3); // [1,2,3,0]
        order.remove(0); // [2,3,0]
        assert_eq!(order.as_slice(), primary([2, 3, 0]));

        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([1, 2, 3, 0]), "one step back");

        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([0, 1, 2, 3]), "back to the start");
        assert!(order.is_unedited());
    }

    #[test]
    fn undo_past_the_beginning_is_a_no_op() {
        let mut order = PageOrder::identity(2);
        assert!(!order.undo());
        assert_eq!(order.as_slice(), primary([0, 1]));
    }

    // --- Merging in another document ------------------------------------------

    #[test]
    fn appending_another_documents_pages_extends_the_order() {
        let mut order = PageOrder::identity(3);
        assert!(order.append(order.document_count(), 2));

        let mut expected = primary([0, 1, 2]);
        expected.extend(from_document(1, [0, 1]));
        assert_eq!(order.as_slice(), expected);
        assert_eq!(order.len(), 5);
        assert_eq!(order.document_count(), 2);
        assert_eq!(order.source_lens(), &[3, 2]);
    }

    #[test]
    fn appending_zero_pages_changes_nothing() {
        // An empty document contributing nothing is not an edit.
        let mut order = PageOrder::identity(3);
        assert!(!order.append(1, 0));
        assert_eq!(
            order.document_count(),
            1,
            "an append that did nothing still counted a document"
        );
        assert!(!order.can_undo());
    }

    #[test]
    fn appending_is_its_own_undo_step() {
        let mut order = PageOrder::identity(3);
        assert!(order.append(1, 2));
        assert_eq!(order.len(), 5);

        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([0, 1, 2]));
        assert_eq!(order.len(), 3, "the undo did not remove the appended pages");
    }

    #[test]
    fn an_appended_document_makes_the_order_edited() {
        let mut order = PageOrder::identity(3);
        let written = order.as_slice().to_vec();
        order.mark_saved(0, &written);
        assert!(order.is_unedited());

        order.append(1, 2);
        assert!(!order.is_unedited(), "inserted pages are unsaved changes");
    }

    #[test]
    fn an_inserted_page_can_be_moved_and_deleted_like_any_other() {
        // The point of the whole model: once appended, an inserted page is an
        // ordinary entry, with no special casing anywhere else in this module.
        let mut order = PageOrder::identity(2);
        assert!(order.append(1, 2));

        assert!(order.move_page(2, 0));
        assert_eq!(
            order.source_of(0),
            Some(Source { document: 1, page: 0 }),
            "the first inserted page moved to the front like any other page would"
        );

        assert!(order.remove(0));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn a_third_and_later_document_gets_its_own_index() {
        let mut order = PageOrder::identity(1);
        order.append(order.document_count(), 1);
        order.append(order.document_count(), 1);
        assert_eq!(order.document_count(), 3);
        assert_eq!(
            order.as_slice(),
            vec![Source { document: 0, page: 0 }, Source { document: 1, page: 0 }, Source {
                document: 2,
                page: 0
            }]
        );
    }

    #[test]
    fn each_documents_bound_is_checked_against_its_own_length() {
        // A page from the smaller document must never be mistaken for a page that
        // only the larger one has.
        let mut order = PageOrder::identity(2);
        order.append(1, 5);
        assert_eq!(order.source_lens(), &[2, 5]);
        for &source in order.as_slice() {
            assert!(
                source.page < order.source_lens()[source.document],
                "{source:?} is out of range for its own document"
            );
        }
    }

    // --- Staging a document, then placing some of its pages -------------------

    #[test]
    fn staging_a_document_does_not_touch_the_order_or_history() {
        let mut order = PageOrder::identity(3);
        assert!(order.stage(order.document_count(), 5));

        assert_eq!(order.as_slice(), primary([0, 1, 2]), "staging is not an edit");
        assert_eq!(order.document_count(), 2, "the staged document is still known");
        assert_eq!(order.source_lens(), &[3, 5]);
        assert!(!order.can_undo(), "staging recorded an undo step");
        assert!(order.is_unedited(), "staging alone should not be an edit");
    }

    #[test]
    fn staging_zero_pages_changes_nothing() {
        let mut order = PageOrder::identity(3);
        assert!(!order.stage(1, 0));
        assert_eq!(
            order.document_count(),
            1,
            "a stage that did nothing still counted a document"
        );
    }

    #[test]
    fn a_staged_documents_pages_are_on_disk_before_any_are_placed() {
        // So a save reads exactly as many pages as the caller registered, even
        // though none of them are in the order yet.
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 3));
        assert_eq!(order.on_disk(1), Some(from_document(1, [0, 1, 2]).as_slice()));
    }

    #[test]
    fn inserting_staged_pages_lands_them_at_the_requested_position() {
        let mut order = PageOrder::identity(3); // [0,1,2]
        assert!(order.stage(1, 2));
        assert!(order.insert_pages(1, &[0], 1));

        let mut expected = primary([0]);
        expected.extend(from_document(1, [0]));
        expected.extend(primary([1, 2]));
        assert_eq!(order.as_slice(), expected);
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn inserting_several_pages_at_once_is_one_undo_step() {
        let mut order = PageOrder::identity(2); // [0,1]
        assert!(order.stage(1, 3));
        assert!(order.insert_pages(1, &[2, 0], 1));

        let mut expected = primary([0]);
        expected.extend(from_document(1, [2, 0]));
        expected.extend(primary([1]));
        assert_eq!(order.as_slice(), expected);

        assert!(order.undo());
        assert_eq!(order.as_slice(), primary([0, 1]), "one undo removed the whole group");
    }

    #[test]
    fn inserted_pages_keep_the_order_they_were_given_in() {
        // Not sorted, and not the order they sit in within their own document —
        // exactly the order the caller (a drag out of a multi-selection) handed in.
        let mut order = PageOrder::identity(1);
        assert!(order.stage(1, 4));
        assert!(order.insert_pages(1, &[3, 1, 2], 1));
        assert_eq!(
            order.as_slice(),
            [p(0)].into_iter().chain(from_document(1, [3, 1, 2])).collect::<Vec<_>>()
        );
    }

    #[test]
    fn inserting_at_the_very_end_is_allowed() {
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 1));
        assert!(order.insert_pages(1, &[0], order.len()));
        let mut expected = primary([0, 1]);
        expected.extend(from_document(1, [0]));
        assert_eq!(order.as_slice(), expected);
    }

    #[test]
    fn inserting_into_an_unstaged_document_is_refused() {
        let mut order = PageOrder::identity(2);
        assert!(!order.insert_pages(1, &[0], 1), "document 1 was never staged");
        assert_eq!(order.as_slice(), primary([0, 1]));
        assert!(!order.can_undo());
    }

    #[test]
    fn a_page_out_of_range_for_its_document_refuses_the_whole_insert() {
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 2));
        assert!(
            !order.insert_pages(1, &[0, 9], 1),
            "page 9 does not exist in a 2-page staged document"
        );
        assert_eq!(order.as_slice(), primary([0, 1]), "the whole call was refused");
        assert!(!order.can_undo(), "a refused insert should not be undoable");
    }

    #[test]
    fn inserting_past_the_end_of_the_order_is_refused() {
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 2));
        assert!(!order.insert_pages(1, &[0], 9));
        assert_eq!(order.as_slice(), primary([0, 1]));
    }

    #[test]
    fn inserting_zero_pages_changes_nothing() {
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 2));
        assert!(!order.insert_pages(1, &[], 0));
        assert_eq!(order.as_slice(), primary([0, 1]));
        assert!(!order.can_undo());
    }

    #[test]
    fn the_same_staged_page_can_be_inserted_more_than_once() {
        // Unusual, not invalid: nothing stops picking the same page out of the
        // staging viewport a second time, and this module does not guess at an
        // intention it was not asked about.
        let mut order = PageOrder::identity(1);
        assert!(order.stage(1, 1));
        assert!(order.insert_pages(1, &[0], 1));
        assert!(order.insert_pages(1, &[0], 2));
        assert_eq!(order.as_slice(), [p(0), Source { document: 1, page: 0 }, Source {
            document: 1,
            page: 0
        }]);
    }

    #[test]
    fn a_staged_document_can_be_drawn_from_by_more_than_one_insert() {
        // The point of splitting `stage` from `insert_pages`: one document, several
        // separate drags, each landing wherever it was dropped.
        let mut order = PageOrder::identity(1);
        assert!(order.stage(1, 3));
        assert!(order.insert_pages(1, &[0], 0));
        assert!(order.insert_pages(1, &[2], order.len()));
        assert_eq!(
            order.as_slice(),
            [Source { document: 1, page: 0 }, p(0), Source { document: 1, page: 2 }]
        );
    }

    #[test]
    fn an_inserted_page_is_an_ordinary_entry_afterward() {
        // The point of the whole model, restated for `insert_pages` the way
        // `an_inserted_page_can_be_moved_and_deleted_like_any_other` states it for
        // `append`: no special casing anywhere else in this module.
        let mut order = PageOrder::identity(2);
        assert!(order.stage(1, 2));
        assert!(order.insert_pages(1, &[0], 1));

        assert!(order.move_page(1, 0));
        assert_eq!(order.source_of(0), Some(Source { document: 1, page: 0 }));

        assert!(order.remove(0));
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn an_inserted_page_makes_the_order_edited() {
        let mut order = PageOrder::identity(2);
        let written = order.as_slice().to_vec();
        order.mark_saved(0, &written);
        assert!(order.is_unedited());

        assert!(order.stage(1, 1));
        assert!(order.insert_pages(1, &[0], 0));
        assert!(!order.is_unedited(), "an inserted page is an unsaved change");
    }

    #[test]
    fn inserted_pages_never_invent_or_duplicate_a_position() {
        let mut order = PageOrder::identity(4);
        assert!(order.stage(1, 3));
        assert!(order.insert_pages(1, &[2, 0], 2));
        assert!(order.move_page(0, 5));
        assert!(order.remove(1));

        let mut seen = order.as_slice().to_vec();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "a page appeared twice: {order:?}");
        for &source in order.as_slice() {
            assert!(
                source.page < order.source_lens()[source.document],
                "invented page {source:?}"
            );
        }
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
        order.mark_saved(0, &written);
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
        order.mark_saved(0, &written);

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
        order.mark_saved(0, &written);

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
        order.mark_saved(0, &written);
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
                source.page < order.source_lens()[source.document],
                "position {position} -> {source:?}"
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

    // --- What a save leaves on disk -------------------------------------------

    #[test]
    fn a_fresh_document_is_on_disk_as_opened() {
        let order = PageOrder::identity(3);
        assert_eq!(order.on_disk(0), Some(primary([0, 1, 2]).as_slice()));
    }

    #[test]
    fn an_appended_document_is_on_disk_as_identity_until_its_own_save() {
        let mut order = PageOrder::identity(2);
        assert!(order.append(1, 3));
        assert_eq!(order.on_disk(1), Some(from_document(1, [0, 1, 2]).as_slice()));
    }

    #[test]
    fn saving_records_what_the_document_now_physically_holds() {
        // [0,1,2,3] -> delete page 0 -> [1,2,3]. The file on disk now holds three
        // pages, and they are original pages 1, 2 and 3 — not fresh pages 0, 1, 2.
        let mut order = PageOrder::identity(4);
        assert!(order.remove(0));
        let written = order.as_slice().to_vec();
        order.mark_saved(0, &written);

        assert_eq!(
            order.on_disk(0),
            Some(primary([1, 2, 3]).as_slice()),
            "the file's new physical layout should be exactly what was written"
        );
        assert_eq!(
            order.source_lens(),
            &[4],
            "source_lens still names the document as it was first opened"
        );
    }

    #[test]
    fn a_document_out_of_range_has_no_on_disk_record() {
        let order = PageOrder::identity(3);
        assert_eq!(order.on_disk(1), None);
    }

    #[test]
    fn saving_twice_keeps_on_disk_in_step_with_each_write() {
        // The scenario `save_reordered` depends on this for: two saves to the same
        // path, with an edit in between. Each `mark_saved` has to describe the file
        // as it now stands, not as it stood after the first write.
        let mut order = PageOrder::identity(4);
        assert!(order.remove(0)); // [1,2,3]
        let first_write = order.as_slice().to_vec();
        order.mark_saved(0, &first_write);
        assert_eq!(order.on_disk(0), Some(primary([1, 2, 3]).as_slice()));

        assert!(order.move_page(0, 1)); // [2,1,3]
        let second_write = order.as_slice().to_vec();
        order.mark_saved(0, &second_write);
        assert_eq!(
            order.on_disk(0),
            Some(primary([2, 1, 3]).as_slice()),
            "the second save's layout should replace the first's, not extend it"
        );
    }

    #[test]
    fn a_secondary_documents_on_disk_record_survives_the_primarys_save() {
        // The primary's file was rewritten; the secondary's was not, so its record
        // should be untouched by a save that never touched its path.
        let mut order = PageOrder::identity(2);
        assert!(order.append(1, 2));
        assert!(order.remove(0));
        let written = order.as_slice().to_vec();
        order.mark_saved(0, &written);

        assert_eq!(
            order.on_disk(1),
            Some(from_document(1, [0, 1]).as_slice()),
            "a document whose file was not the save's destination keeps its record"
        );
    }
}
