//! The system file dialog, kept off the frame loop.
//!
//! See `docs/goal-3-plan.md`. Two things shape this module.
//!
//! **It must not block.** `rfd::FileDialog::pick_file` blocks until the person
//! chooses, and calling that from `App::ui` freezes rendering — which breaks the one
//! property this viewer has been careful about throughout. `rfd` offers an async
//! dialog instead, but that wants an executor and this program has no async runtime;
//! adopting one for a file dialog would be a poor trade. So the blocking call runs on
//! a `std::thread` and the answer comes back over a channel, polled once per frame.
//! That is the same shape as the render pool, so it adds no new concurrency concept.
//!
//! **It is not a command.** The dialog does not change the document; it chooses the
//! argument to [`crate::command::Command::Open`]. So the picker joins the keyboard and
//! the toolbar as a *producer* of that command, and no `pick_file` exists on the
//! control protocol. An agent already has `open` with a path, which is strictly more
//! capable — and a `pick_file` without a programmatic cancel would let an agent enter
//! a modal only a human could dismiss. See `docs/goal-3-plan.md` section 1.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// A pending request for a path.
///
/// The seam for testing: everything except the `rfd` call itself goes through
/// [`Self::poll`], which is exercised against a channel the tests fill by hand. A
/// native modal cannot be driven headlessly, so the alternative would be no coverage
/// of the surrounding logic at all.
#[derive(Debug, Default)]
pub(crate) struct FilePicker {
    /// `Some` while a dialog is open and unanswered.
    pending: Option<Receiver<Option<PathBuf>>>,
}

impl FilePicker {
    /// Whether a dialog is already open.
    ///
    /// Guards against a second `Ctrl+O` stacking dialogs, which on some platforms
    /// leaves a modal nobody can reach behind another one.
    pub(crate) fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// Opens the dialog, unless one is already open.
    pub(crate) fn open(&mut self) {
        if self.is_open() {
            return;
        }
        let (sender, receiver) = channel();
        // Detached on purpose. If the viewer exits while a dialog is up, the thread
        // finds a closed channel and drops the answer, which is the correct outcome —
        // there is nothing left to open the file into.
        std::thread::spawn(move || {
            let chosen = rfd::FileDialog::new()
                .add_filter("PDF", &["pdf"])
                .set_title("Open a PDF")
                .pick_file();
            let _ = sender.send(chosen);
        });
        self.pending = Some(receiver);
    }

    /// Takes the chosen path, if the person has answered since the last poll.
    ///
    /// `None` covers three cases that are all "nothing happened": no answer yet, the
    /// dialog was cancelled, and the dialog could not be shown at all. Cancelling is
    /// deliberately not distinguished from failing — neither should produce a message,
    /// because a person who cancels has not made a mistake.
    pub(crate) fn poll(&mut self) -> Option<PathBuf> {
        let receiver = self.pending.as_ref()?;
        match receiver.try_recv() {
            Ok(chosen) => {
                self.pending = None;
                chosen
            }
            Err(TryRecvError::Empty) => None,
            // The thread died without sending. Clear the flag or the dialog could
            // never be opened again.
            Err(TryRecvError::Disconnected) => {
                self.pending = None;
                None
            }
        }
    }

    /// Installs a channel as if a dialog had been opened. Tests only.
    #[cfg(test)]
    fn with_pending(receiver: Receiver<Option<PathBuf>>) -> Self {
        Self {
            pending: Some(receiver),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_picker_has_nothing_pending() {
        let mut picker = FilePicker::default();
        assert!(!picker.is_open());
        assert_eq!(picker.poll(), None);
    }

    #[test]
    fn a_chosen_path_is_returned_once() {
        let (sender, receiver) = channel();
        let mut picker = FilePicker::with_pending(receiver);
        assert!(picker.is_open());

        sender
            .send(Some(PathBuf::from("chosen.pdf")))
            .expect("the picker should still be listening");
        assert_eq!(picker.poll(), Some(PathBuf::from("chosen.pdf")));

        // And the dialog is no longer open, so the path cannot be delivered twice and
        // re-open the same document every frame.
        assert!(!picker.is_open());
        assert_eq!(picker.poll(), None);
    }

    #[test]
    fn polling_before_an_answer_leaves_the_dialog_open() {
        let (_sender, receiver) = channel();
        let mut picker = FilePicker::with_pending(receiver);
        assert_eq!(picker.poll(), None);
        assert!(picker.is_open(), "gave up on a dialog still being answered");
    }

    #[test]
    fn cancelling_closes_the_dialog_without_a_path() {
        let (sender, receiver) = channel();
        let mut picker = FilePicker::with_pending(receiver);
        sender.send(None).expect("still listening");
        assert_eq!(picker.poll(), None);
        assert!(!picker.is_open());
    }

    #[test]
    fn a_thread_that_dies_without_answering_does_not_wedge_the_picker() {
        // Dropping the sender is what a panicking dialog thread looks like from here.
        // Without clearing `pending`, `is_open` would stay true forever and Ctrl+O
        // would never work again.
        let (sender, receiver) = channel();
        let mut picker = FilePicker::with_pending(receiver);
        drop(sender);

        assert_eq!(picker.poll(), None);
        assert!(!picker.is_open(), "a dead dialog thread wedged the picker");
    }

    #[test]
    fn opening_twice_does_not_replace_a_live_dialog() {
        // The second request has to be dropped rather than stacking a dialog behind
        // the first. Checked through the real `open`, which spawns a thread; the
        // thread's own dialog call is what cannot be tested here.
        let (sender, receiver) = channel();
        let mut picker = FilePicker::with_pending(receiver);
        picker.open();

        // Still the channel we installed, not a replacement: sending on it arrives.
        sender
            .send(Some(PathBuf::from("first.pdf")))
            .expect("the original channel should still be the live one");
        assert_eq!(picker.poll(), Some(PathBuf::from("first.pdf")));
    }
}
