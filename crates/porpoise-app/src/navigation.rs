//! Where the reader is, and which moves from here are possible.
//!
//! The navigation half of what [`crate::edits`] does for the document: one answer,
//! computed once per frame from plain values, that the toolbar both reads its numbers
//! from and greys its buttons by.
//!
//! # Why this is a computed answer and not two numbers
//!
//! [`crate::chrome`] decides nothing; it is handed decisions and paints them. Giving the
//! toolbar `pages` and letting it work out `here < pages` for itself would put the first
//! real decision in the painting layer, and the whole point of that rule is that a lit
//! button cannot come to mean something the rest of the program disagrees with.
//!
//! The page number and count ride along because a control that needs them needs them
//! together with the moves. A "5 / 10" readout or a jump-to-page box is reading the same
//! situation these buttons are.
//!
//! # The keyboard does not read this
//!
//! Unlike [`crate::edits`], which exists precisely because the keyboard and the toolbar
//! had drifted. `command_for_key` is pure and knows nothing about the document, which is
//! what makes every binding testable without one, so the right arrow on the last page
//! still produces `NextPage` and the view resolves it as
//! [`porpoise_view::Outcome::Unchanged`].
//!
//! That is safe here for a reason that did not hold there. The Save drift was visible: a
//! disabled button next to a key press that put an error in the status bar. `Unchanged`
//! is not a failure and reaches nothing a person can see, so the grey button and the
//! inert key press agree about everything observable.

use porpoise_view::{PageNumber, ViewCommand};

use crate::command::Command;

/// The moves available from where the reader is. `None` means "not possible right now",
/// the same contract [`crate::edits::Edits`] uses.
///
/// `PartialEq` but not `Eq`, matching `Edits`: a `ViewCommand` can carry a float.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Navigation {
    /// The page on screen, counting from 1. `None` with no document.
    pub(crate) current: Option<PageNumber>,
    /// How many pages are shown. Zero means no document.
    pub(crate) pages: usize,
    /// Jump to the first page.
    pub(crate) first: Option<Command>,
    /// Go back one page.
    pub(crate) previous: Option<Command>,
    /// Go on one page.
    pub(crate) next: Option<Command>,
    /// Jump to the last page.
    pub(crate) last: Option<Command>,
}

impl Navigation {
    /// Works out where the reader can go.
    ///
    /// `current` is ignored when `pages` is zero, matching
    /// [`crate::edits::Edits::available`] — with nothing open there is no page to be on.
    pub(crate) fn available(current: PageNumber, pages: usize) -> Self {
        let here = current.get();
        // With no document there is no page to be on, and a page past the end is what a
        // delete leaves behind until the view catches up.
        let open = pages > 0 && here <= pages;

        Self {
            current: open.then_some(current),
            pages,
            first: (open && here > 1).then(|| ViewCommand::FirstPage.into()),
            previous: (open && here > 1).then(|| ViewCommand::PreviousPage.into()),
            next: (open && here < pages).then(|| ViewCommand::NextPage.into()),
            last: (open && here < pages).then(|| ViewCommand::LastPage.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(number: usize) -> PageNumber {
        PageNumber::new(number).expect("page numbers in tests start at 1")
    }

    fn go(command: ViewCommand) -> Option<Command> {
        Some(command.into())
    }

    #[test]
    fn every_move_is_offered_from_the_middle_of_a_document() {
        let navigation = Navigation::available(page(5), 10);

        assert_eq!(navigation.current, Some(page(5)));
        assert_eq!(navigation.pages, 10);
        assert_eq!(navigation.first, go(ViewCommand::FirstPage));
        assert_eq!(navigation.previous, go(ViewCommand::PreviousPage));
        assert_eq!(navigation.next, go(ViewCommand::NextPage));
        assert_eq!(navigation.last, go(ViewCommand::LastPage));
    }

    #[test]
    fn the_first_page_offers_no_way_back() {
        let navigation = Navigation::available(PageNumber::FIRST, 10);

        assert_eq!(navigation.first, None);
        assert_eq!(navigation.previous, None);
        assert_eq!(navigation.next, go(ViewCommand::NextPage));
        assert_eq!(navigation.last, go(ViewCommand::LastPage));
    }

    #[test]
    fn the_last_page_offers_no_way_on() {
        let navigation = Navigation::available(page(10), 10);

        assert_eq!(navigation.first, go(ViewCommand::FirstPage));
        assert_eq!(navigation.previous, go(ViewCommand::PreviousPage));
        assert_eq!(navigation.next, None);
        assert_eq!(navigation.last, None);
    }

    #[test]
    fn a_single_page_document_offers_nothing() {
        let navigation = Navigation::available(PageNumber::FIRST, 1);

        assert_eq!(navigation.current, Some(PageNumber::FIRST));
        assert_eq!(navigation.first, None);
        assert_eq!(navigation.previous, None);
        assert_eq!(navigation.next, None);
        assert_eq!(navigation.last, None);
    }

    #[test]
    fn with_no_document_there_is_nowhere_to_go_and_no_page_to_show() {
        let navigation = Navigation::available(PageNumber::FIRST, 0);

        assert_eq!(navigation.current, None);
        assert_eq!(navigation.pages, 0);
        assert_eq!(navigation.first, None);
        assert_eq!(navigation.previous, None);
        assert_eq!(navigation.next, None);
        assert_eq!(navigation.last, None);
    }

    #[test]
    fn a_page_past_the_end_offers_nothing() {
        // What a delete leaves behind for the rest of the frame: the view is still on
        // page 10 of a document that now has 9. `Edits` guards the same way.
        let navigation = Navigation::available(page(10), 9);

        assert_eq!(navigation.current, None);
        assert_eq!(navigation.first, None);
        assert_eq!(navigation.previous, None);
        assert_eq!(navigation.next, None);
        assert_eq!(navigation.last, None);
    }
}
