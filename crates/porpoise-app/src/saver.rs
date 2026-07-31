//! Saving, kept off the frame loop.
//!
//! Measured before deciding this was necessary: reversing the 400-page, 126 MB drawing
//! set took **1.04 seconds** (`docs/goal-4-plan.md` §5a). Doing that inside `ui()`
//! would freeze the window for a second on every save, which is exactly the thing this
//! viewer has avoided everywhere else.
//!
//! So the write runs on a `std::thread` and the answer comes back over a channel,
//! polled once per frame — the same shape as the render pool and the file dialog. A
//! third use of one pattern rather than a third concurrency mechanism.
//!
//! One save at a time. A second request while one is in flight is refused rather than
//! queued: two writes to the same path racing each other is how a file gets corrupted,
//! and there is no sensible order to run them in anyway.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use porpoise_doc::{Overwrite, PageOrder, Source, save_reordered};

/// A save that has finished.
#[derive(Debug)]
pub(crate) struct Saved {
    /// Where the primary file was written.
    pub(crate) path: PathBuf,
    /// The page order that went into the file, in display order.
    ///
    /// Reported back rather than re-read from the document, because the order can move
    /// while the write runs — see [`Saver::start`]. This is what
    /// `PageOrder::mark_saved` needs to get "unsaved changes" right when somebody keeps
    /// editing during a save, and to record what the primary document's file now
    /// physically holds, so a second save over the same path still finds the right
    /// pages.
    pub(crate) written: Vec<Source>,
    /// What went wrong, if anything.
    pub(crate) error: Option<String>,
}

/// A save in flight, if there is one.
#[derive(Debug, Default)]
pub(crate) struct Saver {
    pending: Option<Receiver<Saved>>,
    /// Where the in-flight save is going, for reporting while it runs.
    destination: Option<PathBuf>,
}

impl Saver {
    /// Whether a save is already running.
    pub(crate) fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Where the in-flight save is going.
    pub(crate) fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    /// Starts a save, unless one is already running.
    ///
    /// `sources` is one path per document `order` refers to, index for index — see
    /// `OpenDocument::source_paths` for how the viewer builds it.
    ///
    /// Returns whether it started. The order is copied rather than borrowed so the
    /// caller can go on editing while the write runs — the saved file reflects the
    /// order as it was when the save began, which is the only interpretation that does
    /// not need a lock.
    pub(crate) fn start(
        &mut self,
        sources: &[PathBuf],
        order: &PageOrder,
        destination: &Path,
        overwrite: Overwrite,
    ) -> bool {
        if self.is_busy() {
            return false;
        }
        let (sender, receiver) = channel();
        let sources = sources.to_vec();
        let order = order.clone();
        let destination = destination.to_path_buf();
        let reported = destination.clone();
        // Taken here, on the UI thread, so it is unambiguously the order the write is
        // about — even if the pages move again a millisecond from now.
        let written = order.as_slice().to_vec();

        std::thread::spawn(move || {
            let error = save_reordered(&sources, &order, &destination, overwrite)
                .err()
                .map(|failure| failure.to_string());
            // A closed channel means the viewer exited mid-save. The file is already
            // written or already refused; there is nobody left to tell.
            let _ = sender.send(Saved {
                path: destination,
                written,
                error,
            });
        });

        self.pending = Some(receiver);
        self.destination = Some(reported);
        true
    }

    /// Takes the result, if the save has finished since the last poll.
    pub(crate) fn poll(&mut self) -> Option<Saved> {
        let receiver = self.pending.as_ref()?;
        match receiver.try_recv() {
            Ok(saved) => {
                self.pending = None;
                self.destination = None;
                Some(saved)
            }
            Err(TryRecvError::Empty) => None,
            // The thread died without reporting. Clearing the slot matters more than
            // the lost message: otherwise no further save could ever be started.
            Err(TryRecvError::Disconnected) => {
                let path = self.destination.take().unwrap_or_default();
                self.pending = None;
                Some(Saved {
                    path,
                    // Empty because nothing is known to have reached the file. It is
                    // never read on a failure, and an order invented here could mark a
                    // document saved that was not.
                    written: Vec::new(),
                    error: Some("the save did not report back".to_owned()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use porpoise_testkit::multi_page_pdf;

    fn scratch(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("porpoise-saver-{name}"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Blocks until the save reports, so the assertions are about the result rather
    /// than about timing.
    fn wait(saver: &mut Saver) -> Saved {
        for _ in 0..600 {
            if let Some(saved) = saver.poll() {
                return saved;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the save never reported");
    }

    #[test]
    fn a_fresh_saver_is_idle() {
        let mut saver = Saver::default();
        assert!(!saver.is_busy());
        assert!(saver.poll().is_none());
        assert!(saver.destination().is_none());
    }

    #[test]
    fn a_save_reports_where_it_went() {
        let source = scratch("source.pdf");
        std::fs::write(&source, multi_page_pdf(3, 200, 100)).expect("write fixture");
        let destination = scratch("out.pdf");

        let mut saver = Saver::default();
        let mut order = PageOrder::identity(3);
        assert!(order.move_page(0, 2));
        assert!(saver.start(std::slice::from_ref(&source), &order, &destination, Overwrite::Refuse));
        assert!(saver.is_busy(), "reported idle with a save running");
        assert_eq!(saver.destination(), Some(destination.as_path()));

        let saved = wait(&mut saver);
        assert_eq!(saved.error, None, "save failed: {saved:?}");
        assert_eq!(saved.path, destination);
        assert!(destination.exists());
        assert!(!saver.is_busy(), "still busy after reporting");
    }

    #[test]
    fn a_save_reports_the_order_it_wrote() {
        // Not the order at the time it finished. That is the whole reason this travels
        // back: the pages can be moved again during the second a big save takes, and
        // marking those moves as written would be a lie about where somebody's work is.
        let source = scratch("reported-source.pdf");
        std::fs::write(&source, multi_page_pdf(3, 200, 100)).expect("write fixture");
        let destination = scratch("reported-out.pdf");

        let mut saver = Saver::default();
        let mut order = PageOrder::identity(3);
        assert!(order.move_page(0, 2));
        let expected = order.as_slice().to_vec();
        assert!(saver.start(std::slice::from_ref(&source), &order, &destination, Overwrite::Refuse));

        // Kept editing while the write ran, exactly as a person would.
        assert!(order.move_page(0, 1));

        let saved = wait(&mut saver);
        assert_eq!(saved.error, None, "save failed: {saved:?}");
        assert_eq!(
            saved.written, expected,
            "reported an order the write never saw"
        );
    }

    #[test]
    fn a_failure_comes_back_as_a_message_rather_than_a_panic() {
        // A save that cannot happen has to be reportable. A panicking worker would
        // take the message with it and leave the saver wedged.
        let source = scratch("missing-source.pdf");
        let _ = std::fs::remove_file(&source);
        let destination = scratch("never-written.pdf");

        let mut saver = Saver::default();
        assert!(saver.start(
            std::slice::from_ref(&source),
            &PageOrder::identity(3),
            &destination,
            Overwrite::Refuse
        ));

        let saved = wait(&mut saver);
        assert!(saved.error.is_some(), "a missing source reported success");
        assert!(!destination.exists());
        assert!(!saver.is_busy());
    }

    #[test]
    fn a_second_save_is_refused_while_one_is_running() {
        // Two writes racing for one path is how a file gets corrupted, and there is no
        // sensible order to run them in.
        let source = scratch("busy-source.pdf");
        std::fs::write(&source, multi_page_pdf(3, 200, 100)).expect("write fixture");
        let first = scratch("busy-first.pdf");
        let second = scratch("busy-second.pdf");

        let mut saver = Saver::default();
        let order = PageOrder::identity(3);
        assert!(saver.start(std::slice::from_ref(&source), &order, &first, Overwrite::Refuse));
        assert!(
            !saver.start(std::slice::from_ref(&source), &order, &second, Overwrite::Refuse),
            "started a second save while one was running"
        );

        let saved = wait(&mut saver);
        assert_eq!(saved.path, first, "the refused save displaced the first");
        assert!(!second.exists(), "the refused save wrote anyway");
    }

    #[test]
    fn a_save_can_be_started_again_after_one_finishes() {
        let source = scratch("again-source.pdf");
        std::fs::write(&source, multi_page_pdf(3, 200, 100)).expect("write fixture");

        let mut saver = Saver::default();
        let order = PageOrder::identity(3);
        for round in 0..2 {
            let destination = scratch(&format!("again-{round}.pdf"));
            assert!(
                saver.start(std::slice::from_ref(&source), &order, &destination, Overwrite::Refuse),
                "round {round} was refused"
            );
            let saved = wait(&mut saver);
            assert_eq!(saved.error, None, "round {round}: {saved:?}");
        }
    }
}
