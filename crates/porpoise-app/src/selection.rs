//! Which pages are picked out in a grid, and what a click does to that.
//!
//! Two grids can each have one of these: [`crate::thumbnails`]'s reorganize mode, and
//! the merge tab's staging viewport (`docs/goal-5-plan.md` §10.4) — two independent
//! instances, since picking a page out in one says nothing about the other. The
//! decisions are here rather than in either grid because none of them need a window:
//! what ctrl+click does to a set is arithmetic, and arithmetic is the part of this
//! program that gets tested. The same split [`crate::edits`] made for the toolbar.
//!
//! # It holds source pages, not display positions
//!
//! The obvious choice is to remember the positions that are lit up — they are what is on
//! screen, and what a marquee produces. It is also wrong, because every edit moves them.
//! Select positions 1 and 2, drag them to the end, and a position-based selection is now
//! lighting up two pages nobody picked; delete a page and every selection after it is off
//! by one.
//!
//! Source pages have none of that. A reorder does not change which source page is which,
//! so the selection follows the pages it was pointing at with no reconciliation step to
//! forget — and an undo brings back both the pages and their selection, which is what
//! somebody who has just undone a delete would expect. The cost is that reading the
//! selection back out needs to know what is currently shown, so every method here takes
//! that as a plain `&[Source]` rather than a `PageOrder` specifically — a document staged
//! for the merge tab but not yet part of any order (`docs/goal-5-plan.md` §10.3) still has
//! pages to pick from, and its list is `(0..page_count).map(|page| Source { document,
//! page })`, not a `PageOrder::as_slice()`. There is no way to ask this module a question
//! about positions without saying which list of sources they are positions in.
//!
//! This is the same crossing `PageOrder::source_of` exists for, and the fourth time this
//! codebase has had two kinds of number in one shape — see `porpoise-doc`'s `order`
//! module docs for the other three.

use std::collections::BTreeSet;

use porpoise_doc::Source;

/// What a click on a thumbnail was asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    /// A plain click: this page and nothing else.
    Only,
    /// Ctrl or Cmd held: add this page, or take it back out.
    Toggle,
    /// Shift held: everything from the anchor to here.
    Range,
}

impl Pick {
    /// What a click with these modifiers means.
    ///
    /// Plain booleans rather than `egui::Modifiers`, so this module needs no GUI to test.
    /// The caller decides that ctrl and cmd both mean `toggle`, because which one is
    /// idiomatic is a platform question and not one about sets.
    ///
    /// Shift wins when both are held. Extending a range is the more specific request,
    /// and the other way round would silently throw the anchor away.
    pub(crate) fn of(toggle: bool, range: bool) -> Self {
        if range {
            Self::Range
        } else if toggle {
            Self::Toggle
        } else {
            Self::Only
        }
    }
}

/// The pages picked out in the grid.
///
/// Empty by default: opening the panel selects nothing, so the first thing a click can do
/// is start a selection rather than extend one somebody has forgotten about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Selected **sources**. See the module docs.
    chosen: BTreeSet<Source>,
    /// The source a shift-click measures from.
    ///
    /// Separate from the selection because it survives the selection shrinking: shift
    /// clicking twice should measure from the same place both times, which is what makes
    /// a range you got slightly wrong fixable by clicking again rather than starting over.
    anchor: Option<Source>,
}

impl Selection {
    /// How many of `shown` are selected.
    ///
    /// Takes what is currently shown because a selected page that has since been
    /// deleted, or a staged page that has since been un-staged, is not selected any
    /// more — it is only remembered in case of an undo.
    pub(crate) fn count(&self, shown: &[Source]) -> usize {
        shown
            .iter()
            .filter(|source| self.chosen.contains(source))
            .count()
    }

    /// Whether the page at `position` in `shown` is selected.
    pub(crate) fn contains_position(&self, shown: &[Source], position: usize) -> bool {
        shown
            .get(position)
            .is_some_and(|source| self.chosen.contains(source))
    }

    /// Selected positions in `shown`, ascending.
    ///
    /// Ascending because that is what the group edits in `PageOrder` want and what keeps
    /// a dragged block in the order it appeared in — clicking pages 5 then 2 and dragging
    /// them must not put 5 before 2.
    pub(crate) fn positions(&self, shown: &[Source]) -> Vec<usize> {
        (0..shown.len())
            .filter(|&position| self.contains_position(shown, position))
            .collect()
    }

    /// Applies a click on `position` in `shown`.
    pub(crate) fn pick(&mut self, shown: &[Source], position: usize, pick: Pick) {
        let Some(&source) = shown.get(position) else {
            return;
        };
        match pick {
            Pick::Only => {
                self.chosen.clear();
                self.chosen.insert(source);
                self.anchor = Some(source);
            }
            Pick::Toggle => {
                if !self.chosen.remove(&source) {
                    self.chosen.insert(source);
                }
                // The anchor moves even when the click removed the page, so a following
                // shift+click measures from where the pointer last was rather than from
                // whatever was clicked before that.
                self.anchor = Some(source);
            }
            Pick::Range => {
                // With nothing to measure from, a shift+click is just a click. Reachable
                // whenever the panel has only just opened.
                let Some(from) = self.anchor_position(shown) else {
                    self.pick(shown, position, Pick::Only);
                    return;
                };
                let (low, high) = if from <= position {
                    (from, position)
                } else {
                    (position, from)
                };
                self.chosen.clear();
                self.chosen
                    .extend((low..=high).filter_map(|p| shown.get(p).copied()));
                // The anchor deliberately stays where it was, so shift+clicking again
                // re-measures the range instead of growing it from the last click.
            }
        }
    }

    /// Replaces the selection with these positions in `shown`, as a marquee does.
    ///
    /// The anchor becomes the first of them, so a shift+click after dragging a box
    /// extends from the top of what was boxed.
    pub(crate) fn set_positions(&mut self, shown: &[Source], positions: &[usize]) {
        self.chosen.clear();
        self.chosen
            .extend(positions.iter().filter_map(|&p| shown.get(p).copied()));
        self.anchor = positions
            .iter()
            .min()
            .and_then(|&position| shown.get(position).copied());
    }

    /// Forgets everything, including the anchor.
    pub(crate) fn clear(&mut self) {
        self.chosen.clear();
        self.anchor = None;
    }

    /// Where the anchor currently sits in `shown`, if its page is still there.
    fn anchor_position(&self, shown: &[Source]) -> Option<usize> {
        let anchor = self.anchor?;
        shown.iter().position(|&source| source == anchor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use porpoise_doc::PageOrder;

    /// A six-page document, unedited.
    fn order() -> PageOrder {
        PageOrder::identity(6)
    }

    #[test]
    fn modifiers_map_to_picks() {
        assert_eq!(Pick::of(false, false), Pick::Only);
        assert_eq!(Pick::of(true, false), Pick::Toggle);
        assert_eq!(Pick::of(false, true), Pick::Range);
    }

    #[test]
    fn shift_wins_over_ctrl() {
        // Both held is ambiguous, and treating it as a toggle would discard the anchor —
        // losing information the person can only restore by starting the range again.
        assert_eq!(Pick::of(true, true), Pick::Range);
    }

    #[test]
    fn a_fresh_selection_is_empty() {
        let selection = Selection::default();
        assert_eq!(selection.count(order().as_slice()), 0);
        assert_eq!(selection.positions(order().as_slice()), Vec::<usize>::new());
    }

    #[test]
    fn a_plain_click_selects_only_that_page() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 3, Pick::Only);
        assert_eq!(selection.positions(order.as_slice()), vec![3]);

        selection.pick(order.as_slice(), 1, Pick::Only);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![1],
            "a plain click did not replace the selection"
        );
    }

    #[test]
    fn ctrl_click_adds_and_removes() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 1, Pick::Toggle);
        selection.pick(order.as_slice(), 3, Pick::Toggle);
        selection.pick(order.as_slice(), 5, Pick::Toggle);
        assert_eq!(selection.positions(order.as_slice()), vec![1, 3, 5]);

        selection.pick(order.as_slice(), 3, Pick::Toggle);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![1, 5],
            "ctrl+click on a selected page did not deselect it"
        );
    }

    #[test]
    fn shift_click_selects_the_range_between() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 1, Pick::Only);
        selection.pick(order.as_slice(), 4, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![1, 2, 3, 4]);
    }

    #[test]
    fn shift_click_works_backwards_too() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 4, Pick::Only);
        selection.pick(order.as_slice(), 1, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![1, 2, 3, 4]);
    }

    #[test]
    fn shift_clicking_twice_re_measures_from_the_same_anchor() {
        // What makes a range you got slightly wrong fixable with one more click, rather
        // than growing from wherever you last clicked.
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 1, Pick::Only);
        selection.pick(order.as_slice(), 5, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![1, 2, 3, 4, 5]);

        selection.pick(order.as_slice(), 3, Pick::Range);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![1, 2, 3],
            "the second shift+click measured from the first instead of the anchor"
        );
    }

    #[test]
    fn shift_click_with_no_anchor_is_a_plain_click() {
        // Reachable the moment the panel opens, so it must not select nothing or panic.
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 2, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![2]);
    }

    #[test]
    fn a_range_after_a_ctrl_click_measures_from_it() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 0, Pick::Only);
        selection.pick(order.as_slice(), 4, Pick::Toggle);
        selection.pick(order.as_slice(), 2, Pick::Range);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![2, 3, 4],
            "the range did not measure from the ctrl+clicked page"
        );
    }

    #[test]
    fn a_marquee_replaces_the_selection() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 0, Pick::Only);
        selection.set_positions(order.as_slice(), &[2, 3, 4]);
        assert_eq!(selection.positions(order.as_slice()), vec![2, 3, 4]);
    }

    #[test]
    fn a_shift_click_after_a_marquee_extends_from_its_top() {
        let order = order();
        let mut selection = Selection::default();
        selection.set_positions(order.as_slice(), &[3, 4]);
        selection.pick(order.as_slice(), 5, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![3, 4, 5]);
    }

    #[test]
    fn an_empty_marquee_clears_the_selection() {
        // Dragging a box over nothing is how you deselect, so it has to mean that.
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 1, Pick::Only);
        selection.set_positions(order.as_slice(), &[]);
        assert_eq!(selection.count(order.as_slice()), 0);
    }

    #[test]
    fn clicking_past_the_end_changes_nothing() {
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 2, Pick::Only);
        selection.pick(order.as_slice(), 99, Pick::Only);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![2],
            "a click outside the document disturbed the selection"
        );
    }

    // --- The reason it holds source pages ------------------------------------

    #[test]
    fn a_selection_follows_the_pages_when_they_move() {
        // The whole argument for storing source pages. A position-based selection would
        // now be lighting up two pages nobody picked.
        let mut order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 0, Pick::Only);
        selection.pick(order.as_slice(), 1, Pick::Toggle);
        assert_eq!(selection.positions(order.as_slice()), vec![0, 1]);

        assert!(order.move_pages(&[0, 1], 4));
        assert_eq!(
            order.as_slice(),
            [2, 3, 4, 5, 0, 1]
                .map(|page| Source { document: 0, page })
                .to_vec()
        );
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![4, 5],
            "the selection did not follow the pages it was pointing at"
        );
    }

    #[test]
    fn a_selection_is_unshifted_by_a_deletion_elsewhere() {
        let mut order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 4, Pick::Only);
        assert!(order.remove(0));
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![3],
            "deleting an earlier page left the selection off by one"
        );
    }

    #[test]
    fn deleting_the_selected_pages_leaves_nothing_selected() {
        let mut order = order();
        let mut selection = Selection::default();
        selection.set_positions(order.as_slice(), &[1, 2]);
        assert!(order.remove_pages(&selection.positions(order.as_slice())));
        assert_eq!(
            selection.count(order.as_slice()),
            0,
            "pages that are gone still counted as selected"
        );
    }

    #[test]
    fn undoing_a_delete_brings_the_selection_back() {
        // A consequence of holding source pages rather than a deliberate feature, but the
        // right behaviour: somebody who just undid a delete is looking at those pages.
        let mut order = order();
        let mut selection = Selection::default();
        selection.set_positions(order.as_slice(), &[1, 2]);
        order.remove_pages(&[1, 2]);
        assert_eq!(selection.count(order.as_slice()), 0);

        assert!(order.undo());
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![1, 2],
            "the selection did not come back with the pages"
        );
    }

    #[test]
    fn an_anchor_whose_page_was_deleted_falls_back_to_a_plain_click() {
        let mut order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 0, Pick::Only);
        assert!(order.remove(0));

        selection.pick(order.as_slice(), 3, Pick::Range);
        assert_eq!(
            selection.positions(order.as_slice()),
            vec![3],
            "a range measured from a page that no longer exists"
        );
    }

    #[test]
    fn clearing_forgets_the_anchor_too() {
        // Otherwise a shift+click after clearing would silently extend from a page the
        // person can no longer see is involved.
        let order = order();
        let mut selection = Selection::default();
        selection.pick(order.as_slice(), 1, Pick::Only);
        selection.clear();
        selection.pick(order.as_slice(), 4, Pick::Range);
        assert_eq!(selection.positions(order.as_slice()), vec![4]);
    }

    // --- Reused for a document staged but not yet in any order ---------------
    //
    // The merge tab's staging viewport (`docs/goal-5-plan.md` §10.4) shows a second
    // document's own pages before any of them are part of a `PageOrder` at all, so
    // there is no `PageOrder::as_slice()` to hand this module — only a plain list of
    // that document's `Source`s. These tests take the same shapes the tests above
    // already prove against `order()`, but build `shown` by hand instead, which is
    // what proves this module never needed a `PageOrder` specifically — only
    // whatever `&[Source]` names what is currently on screen.

    /// A staged document's own pages, exactly as the merge tab would build them:
    /// `(0..page_count).map(|page| Source { document, page })`.
    fn staged(document: usize, page_count: usize) -> Vec<Source> {
        (0..page_count)
            .map(|page| Source { document, page })
            .collect()
    }

    #[test]
    fn a_selection_picks_from_a_staged_documents_own_pages() {
        let shown = staged(1, 5);
        let mut selection = Selection::default();
        selection.pick(&shown, 2, Pick::Only);
        selection.pick(&shown, 4, Pick::Toggle);
        assert_eq!(selection.positions(&shown), vec![2, 4]);
        assert_eq!(selection.count(&shown), 2);
    }

    #[test]
    fn a_range_pick_works_against_a_staged_documents_pages() {
        let shown = staged(1, 6);
        let mut selection = Selection::default();
        selection.pick(&shown, 1, Pick::Only);
        selection.pick(&shown, 4, Pick::Range);
        assert_eq!(selection.positions(&shown), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_marquee_over_staged_pages_replaces_the_selection() {
        let shown = staged(1, 6);
        let mut selection = Selection::default();
        selection.pick(&shown, 0, Pick::Only);
        selection.set_positions(&shown, &[2, 3, 4]);
        assert_eq!(selection.positions(&shown), vec![2, 3, 4]);
    }

    #[test]
    fn two_independent_selections_do_not_share_state() {
        // The main document's selection and the staging pane's are two separate
        // instances, precisely so picking a page out in one says nothing about the
        // other — even when, as here, the two documents happen to have pages at the
        // same indices.
        let main = order();
        let staged_pages = staged(1, 4);
        let mut main_selection = Selection::default();
        let mut staged_selection = Selection::default();

        main_selection.pick(main.as_slice(), 1, Pick::Only);
        staged_selection.pick(&staged_pages, 1, Pick::Toggle);
        staged_selection.pick(&staged_pages, 2, Pick::Toggle);

        assert_eq!(main_selection.positions(main.as_slice()), vec![1]);
        assert_eq!(staged_selection.positions(&staged_pages), vec![1, 2]);
    }
}
