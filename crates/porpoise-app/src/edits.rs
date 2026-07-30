//! Which page edits are possible right now, and the command each one produces.
//!
//! One answer for every producer. The toolbar greys a button out when there is no
//! command here; a key press does nothing.
//!
//! # Why this is one function
//!
//! The keyboard and the toolbar used to work this out separately, and they had already
//! drifted. `Ctrl+S` produced a `Save` unconditionally while the Save button was disabled
//! during a save — so pressing the key twice on a large document put *"a save is already
//! running"* in the status bar, which the button could never do. Measured on the 400-page
//! drawing set: the second `save` comes back `ok:false`.
//!
//! Harmless in itself — the message clears when the save lands — but it was two answers
//! to one question, and that is how they drift further. Now there is one answer, and
//! because it takes plain values rather than a borrow of the viewer, it is the first part
//! of the toolbar's behaviour that has unit tests at all.

use porpoise_view::PageNumber;

use crate::command::Command;

/// Everything needed to decide which edits apply.
///
/// Plain values, so this module never learns what a `Viewer` is and can be tested
/// without a window, a document, or a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Situation {
    /// The page on screen, counting from 1. Ignored when `pages` is zero.
    pub(crate) current: PageNumber,
    /// How many pages are shown. Zero means no document.
    pub(crate) pages: usize,
    /// Whether there is a page edit to walk back.
    pub(crate) can_undo: bool,
    /// Whether the order differs from the file.
    pub(crate) unsaved_changes: bool,
    /// Whether a save is already running.
    pub(crate) saving: bool,
    /// Whether the page grid is showing.
    pub(crate) thumbnails: bool,
}

/// The commands available in this situation. `None` means "not possible right now".
///
/// `PartialEq` but not `Eq`, because a `ViewCommand` can carry a zoom factor and floats
/// are not `Eq`. Nothing here needs to be a hash key.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Edits {
    /// Move the page on screen one position earlier.
    pub(crate) move_earlier: Option<Command>,
    /// Move it one position later.
    pub(crate) move_later: Option<Command>,
    /// Remove it.
    pub(crate) delete: Option<Command>,
    /// Walk back the last edit.
    pub(crate) undo: Option<Command>,
    /// Write the changes over the file.
    pub(crate) save: Option<Command>,
    /// Show or hide the page grid. Always possible — it needs no document and cannot
    /// fail, which is also why an agent can never be stuck with it open.
    pub(crate) toggle_thumbnails: Command,
}

impl Edits {
    /// Works out what can be done.
    pub(crate) fn available(situation: Situation) -> Self {
        let Situation {
            current,
            pages,
            can_undo,
            unsaved_changes,
            saving,
            thumbnails,
        } = situation;

        let here = current.get();
        // With no document there is no page to move, and `current` means nothing.
        let open = pages > 0 && here <= pages;

        Self {
            move_earlier: (open && here > 1)
                .then(|| PageNumber::new(here - 1))
                .flatten()
                .map(|to| Command::MovePage { from: current, to }),
            move_later: (open && here < pages)
                .then(|| PageNumber::new(here + 1))
                .flatten()
                .map(|to| Command::MovePage { from: current, to }),
            // The last page cannot go: a PDF with no pages is not a PDF. `PageOrder`
            // refuses it too, so this only spares the person a button that does nothing.
            delete: (open && pages > 1).then_some(Command::DeletePage { page: current }),
            undo: can_undo.then_some(Command::Undo),
            // Not while one is running. This is the line the two producers disagreed on.
            save: (unsaved_changes && !saving).then_some(Command::Save),
            toggle_thumbnails: Command::SetThumbnails {
                visible: !thumbnails,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(number: usize) -> PageNumber {
        PageNumber::new(number).expect("page numbers in tests start at 1")
    }

    /// A ten-page document sitting on page 5, freshly opened.
    fn settled() -> Situation {
        Situation {
            current: page(5),
            pages: 10,
            can_undo: false,
            unsaved_changes: false,
            saving: false,
            thumbnails: false,
        }
    }

    #[test]
    fn a_page_in_the_middle_can_move_either_way() {
        let edits = Edits::available(settled());
        assert_eq!(
            edits.move_earlier,
            Some(Command::MovePage {
                from: page(5),
                to: page(4)
            })
        );
        assert_eq!(
            edits.move_later,
            Some(Command::MovePage {
                from: page(5),
                to: page(6)
            })
        );
    }

    #[test]
    fn the_first_page_cannot_move_earlier_and_the_last_cannot_move_later() {
        let first = Edits::available(Situation {
            current: page(1),
            ..settled()
        });
        assert_eq!(first.move_earlier, None);
        assert!(first.move_later.is_some());

        let last = Edits::available(Situation {
            current: page(10),
            ..settled()
        });
        assert!(last.move_earlier.is_some());
        assert_eq!(last.move_later, None);
    }

    #[test]
    fn nothing_can_be_edited_with_no_document() {
        let empty = Edits::available(Situation {
            pages: 0,
            ..settled()
        });
        assert_eq!(empty.move_earlier, None);
        assert_eq!(empty.move_later, None);
        assert_eq!(empty.delete, None);
        assert_eq!(empty.save, None);
    }

    #[test]
    fn a_current_page_past_the_end_is_not_trusted() {
        // Reachable for a frame after a delete shortens the document, because the view
        // reports where it was looking and the layout has only just changed. Producing
        // a `MovePage` from a page that no longer exists would be refused anyway; not
        // offering it is better than offering it.
        let stale = Edits::available(Situation {
            current: page(11),
            pages: 10,
            ..settled()
        });
        assert_eq!(stale.move_earlier, None);
        assert_eq!(stale.move_later, None);
        assert_eq!(stale.delete, None);
    }

    #[test]
    fn the_last_remaining_page_cannot_be_deleted() {
        let one = Edits::available(Situation {
            current: page(1),
            pages: 1,
            ..settled()
        });
        assert_eq!(one.delete, None);
        assert_eq!(one.move_earlier, None);
        assert_eq!(one.move_later, None);
    }

    #[test]
    fn undo_needs_something_to_undo() {
        assert_eq!(Edits::available(settled()).undo, None);
        let edited = Edits::available(Situation {
            can_undo: true,
            ..settled()
        });
        assert_eq!(edited.undo, Some(Command::Undo));
    }

    #[test]
    fn saving_needs_changes() {
        assert_eq!(Edits::available(settled()).save, None);
        let dirty = Edits::available(Situation {
            unsaved_changes: true,
            ..settled()
        });
        assert_eq!(dirty.save, Some(Command::Save));
    }

    #[test]
    fn saving_is_unavailable_while_a_save_is_running() {
        // The divergence this module exists to remove. The toolbar always knew this and
        // the keyboard did not, so `Ctrl+S` twice produced an error the button could not.
        let during = Edits::available(Situation {
            unsaved_changes: true,
            saving: true,
            ..settled()
        });
        assert_eq!(
            during.save, None,
            "offered a save while one was already running"
        );
    }

    #[test]
    fn the_page_grid_toggles_to_the_opposite_of_where_it_is() {
        assert_eq!(
            Edits::available(settled()).toggle_thumbnails,
            Command::SetThumbnails { visible: true }
        );
        assert_eq!(
            Edits::available(Situation {
                thumbnails: true,
                ..settled()
            })
            .toggle_thumbnails,
            Command::SetThumbnails { visible: false }
        );
    }

    #[test]
    fn the_grid_can_be_toggled_with_no_document() {
        // It is chrome, not an edit. Refusing it with an empty window would be a state
        // an agent could enter and not leave.
        let empty = Edits::available(Situation {
            pages: 0,
            thumbnails: true,
            ..settled()
        });
        assert_eq!(
            empty.toggle_thumbnails,
            Command::SetThumbnails { visible: false }
        );
    }
}
