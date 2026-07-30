//! End-to-end proof that an outside process can drive the whole program.
//!
//! This is Goal 2's M10, and it is deliberately not a unit test. Everything else
//! in the workspace tests a piece: this launches the real binary, talks to it over
//! a real pipe, and checks that a PNG appears on disk with the right page in it.
//! Without it, "an AI agent can drive the program" would be exactly the kind of
//! untested claim this project has refused everywhere else.
//!
//! # Needs a display
//!
//! `porpoise serve` opens a window, so these tests need a display server. They are
//! skipped — loudly, never silently — when `PORPOISE_E2E` is unset, so that a
//! headless `cargo test` does not report a failure it cannot help.
//!
//! Nothing sets it for you since CI was removed, so a plain `cargo test` reports these
//! as passing without running them. `PORPOISE_E2E=1 cargo test` is in the README's
//! *Checks* section for that reason.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use porpoise_testkit::multi_page_pdf;
use porpoise_view::PAGE_GAP_PT;
use serde_json::Value;

/// How long to wait for any single expected message.
///
/// Generous: it covers window creation and GPU setup on a loaded CI runner, and a
/// test that waits too long is merely slow while one that waits too little is
/// flaky.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixture: enough pages that navigating to page 4 means something.
const PAGES: usize = 6;

/// Serializes these tests against one another.
///
/// Each opens a real GPU window, and the control channel is serviced *inside the frame
/// loop* — so a window starved of frames is a window that has stopped answering.
/// Eight of them contending for one adapter starve each other, and whichever test is
/// waiting on a reply then times out after a minute.
///
/// Measured, not guessed: run alone the offending test passed three times out of
/// three, `--test-threads=1` passes all eight in 7.5 s, and the parallel run failed
/// intermittently. A lock rather than a note asking for `--test-threads=1`, so it
/// holds however cargo is invoked.
static ONE_WINDOW_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Skips loudly when there is no display, and otherwise takes the window lock.
///
/// One call rather than a skip check and a separate lock, so the lock cannot be
/// forgotten: there is no way past this line without holding it. A test that silently
/// does nothing reads as coverage it is not providing, hence the message.
fn e2e(name: &str) -> Option<MutexGuard<'static, ()>> {
    if std::env::var_os("PORPOISE_E2E").is_none() {
        eprintln!("SKIPPED {name}: set PORPOISE_E2E=1 (needs a display) to run it");
        return None;
    }
    // Poisoning is ignored on purpose. A panicking test has already reported itself,
    // and refusing the lock afterwards would turn one failure into eight.
    Some(
        ONE_WINDOW_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// A running `porpoise serve` and its two pipes.
struct Serve {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Serve {
    /// Starts the binary on `document`.
    fn start(document: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_porpoise"))
            .arg("serve")
            .arg(document)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so tracing warnings show up in the test output rather
            // than filling a pipe nobody drains.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("porpoise should launch");

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        // A reader thread, because the protocol is asynchronous: events arrive
        // between replies and a blocking read would deadlock against our writes.
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            child,
            stdin,
            lines,
            next_id: 1,
        }
    }

    /// Sends a raw line.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("should write to the control channel");
        self.stdin.flush().expect("should flush");
    }

    /// Sends a command with a fresh id and returns that id.
    fn send(&mut self, command: &str, arguments: &[(&str, Value)]) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let mut object = serde_json::Map::new();
        object.insert("id".to_owned(), Value::from(id));
        object.insert("command".to_owned(), Value::from(command));
        for (key, value) in arguments {
            object.insert((*key).to_owned(), value.clone());
        }
        self.send_raw(&Value::Object(object).to_string());
        id
    }

    /// Reads the next message, whatever it is.
    fn next_message(&self) -> Value {
        match self.lines.recv_timeout(REPLY_TIMEOUT) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("unparsable output {line:?}: {error}")),
            Err(RecvTimeoutError::Timeout) => {
                panic!("no output within {REPLY_TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the program exited unexpectedly"),
        }
    }

    /// Reads until the reply to `id` arrives, ignoring events along the way.
    fn reply_to(&self, id: u64) -> Value {
        loop {
            let message = self.next_message();
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    /// Reads until an event of the given name arrives.
    fn wait_for_event(&self, name: &str) -> Value {
        loop {
            let message = self.next_message();
            if message.get("event").and_then(Value::as_str) == Some(name) {
                return message;
            }
        }
    }

    /// Asks for a snapshot and returns the whole thing.
    fn snapshot(&mut self) -> Value {
        let id = self.send("snapshot", &[]);
        let reply = self.reply_to(id);
        assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true));
        reply.get("snapshot").cloned().expect("a snapshot")
    }

    /// Asks for a snapshot and returns its `view` object.
    fn view(&mut self) -> Value {
        self.snapshot()
            .get("view")
            .cloned()
            .expect("a snapshot with a view")
    }

    /// Sends `quit` and waits for the process to end.
    ///
    /// Fails rather than hanging if it does not, which is what happens when a session
    /// ends with unsaved page changes: `quit` then asks first and waits for an `answer`.
    /// A bare `child.wait()` there would block a test run forever with no clue why.
    fn quit(mut self) {
        self.send_raw(r#"{"command":"quit"}"#);
        self.expect_exit();
    }

    /// Waits a bounded time for the process to exit successfully.
    fn expect_exit(&mut self) {
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match self
                .child
                .try_wait()
                .expect("should be able to poll the child")
            {
                Some(status) => {
                    assert!(status.success(), "exited with {status}");
                    return;
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        panic!("still running {REPLY_TIMEOUT:?} after being told to quit");
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        // A panicking test must not leave a window on someone's desktop.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes the fixture into cargo's target directory.
fn fixture(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    std::fs::write(&path, multi_page_pdf(PAGES, 200, 300)).expect("should write the fixture");
    path
}

fn scratch(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn an_agent_can_open_navigate_and_capture() {
    let Some(_window) = e2e("an_agent_can_open_navigate_and_capture") else {
        return;
    };

    let document = fixture("e2e-navigate.pdf");
    let capture = scratch("e2e-capture.png");
    let mut serve = Serve::start(&document);

    // 1. Discovery: an agent should be able to ask what the program does.
    let id = serve.send("commands", &[]);
    let reply = serve.reply_to(id);
    let commands: Vec<String> = reply
        .get("commands")
        .and_then(Value::as_array)
        .expect("a command list")
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    assert!(commands.contains(&"go_to_page".to_owned()), "{commands:?}");
    assert!(commands.contains(&"capture".to_owned()), "{commands:?}");

    // 2. The document opened from the command line is already loaded.
    let view = serve.view();
    assert_eq!(
        view.get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64)
    );

    // 3. Navigate.
    let id = serve.send("go_to_page", &[("page", Value::from(3))]);
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("changed")
    );

    // 4. Wait for the pipeline to settle, then confirm we are actually there.
    //    This is what `idle` is for: without it the snapshot below could be read
    //    before the frame that honours the scroll request.
    serve.wait_for_event("idle");
    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(3),
        "navigation did not take effect: {view}"
    );
    assert_eq!(
        view.get("pending_scroll_pt"),
        Some(&Value::Null),
        "a scroll was still pending after idle: {view}"
    );
    // Page numbers are one-based, so page 3 is the *third* page: two pages and two
    // gaps above it. The round trip above holds under either convention, which is
    // why the offset is asserted too. `PAGE_GAP_PT` is imported from `porpoise-view`
    // rather than copied, so this cannot quietly disagree with the layout the viewer
    // actually builds.
    assert_eq!(
        view.get("scroll_top_pt").and_then(Value::as_f64),
        Some(2.0 * (300.0 + PAGE_GAP_PT)),
        "page 3 is not where one-based numbering puts it: {view}"
    );
    assert_eq!(
        view.get("first_visible_page").and_then(Value::as_u64),
        Some(3)
    );

    // 5. Capture, and wait for the file to actually exist.
    let id = serve.send(
        "capture",
        &[("path", Value::from(capture.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("capturing"),
        "a capture must not claim to be finished before the file exists"
    );

    let event = serve.wait_for_event("captured");
    assert!(
        event
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("e2e-capture")),
        "unexpected capture path: {event}"
    );

    // 6. The proof: a real PNG, of a real window, with real dimensions.
    let bytes = std::fs::read(&capture).expect("the capture should exist on disk");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("should be a readable PNG");
    let info = reader.info();
    assert!(
        info.width > 100 && info.height > 100,
        "captured a {}x{} image",
        info.width,
        info.height
    );

    serve.quit();
}

#[test]
fn a_capture_leaves_the_window_open_for_more_commands() {
    let Some(_window) = e2e("a_capture_leaves_the_window_open_for_more_commands") else {
        return;
    };

    // The regression this exists for: `capture` closed the window, because the
    // capture machinery was written for the one-shot CLI `--screenshot` flag, where
    // capture-then-exit is the whole point, and was then reused for the `capture`
    // command without revisiting that. Every other capture assertion in this file
    // captures *last*, so the program exiting afterwards looked like teardown.
    let document = fixture("e2e-capture-twice.pdf");
    let first = scratch("e2e-capture-first.png");
    let second = scratch("e2e-capture-second.png");
    let mut serve = Serve::start(&document);

    serve.wait_for_event("idle");

    let id = serve.send(
        "capture",
        &[("path", Value::from(first.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    serve.wait_for_event("captured");

    // The actual point: the program is still listening.
    let id = serve.send("go_to_page", &[("page", Value::from(4))]);
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(true),
        "the window did not survive a capture: {reply}"
    );
    serve.wait_for_event("idle");
    assert_eq!(
        serve.view().get("current_page").and_then(Value::as_u64),
        Some(4)
    );

    // And a second capture still works, which a one-shot state machine would fail.
    let id = serve.send(
        "capture",
        &[("path", Value::from(second.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    serve.wait_for_event("captured");

    let before = std::fs::read(&first).expect("the first capture should exist");
    let after = std::fs::read(&second).expect("the second capture should exist");
    assert_ne!(
        before, after,
        "both captures are byte-identical, so the second is a stale image \
         rather than a fresh one of page 4"
    );

    serve.quit();
}

#[test]
fn an_agent_can_open_a_document_that_was_not_on_the_command_line() {
    let Some(_window) = e2e("an_agent_can_open_a_document_that_was_not_on_the_command_line") else {
        return;
    };

    // No file argument: the window starts empty, which is the case that matters for
    // an agent choosing what to look at.
    let mut child = Command::new(env!("CARGO_BIN_EXE_porpoise"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("porpoise should launch");
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    let mut serve = Serve {
        child,
        stdin,
        lines,
        next_id: 1,
    };

    let view = serve.view();
    assert_eq!(
        view.get("page_count").and_then(Value::as_u64),
        Some(0),
        "expected an empty window"
    );

    let document = fixture("e2e-open.pdf");
    let id = serve.send(
        "open",
        &[("path", Value::from(document.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true));

    let event = serve.wait_for_event("document_opened");
    assert_eq!(
        event.get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64)
    );

    serve.wait_for_event("idle");
    let view = serve.view();
    assert_eq!(
        view.get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64)
    );

    serve.quit();
}

#[test]
fn an_agent_can_reorder_a_document_and_save_it() {
    let Some(_window) = e2e("an_agent_can_reorder_a_document_and_save_it") else {
        return;
    };

    // Goal 4 through the running program rather than the library. `porpoise-render`'s
    // reorder tests prove the *bytes* are right by comparing pixels; this proves the
    // command path reaches them — that a move changes what the window shows, that the
    // page count follows a delete, that undo walks back, and that the saved file has
    // the pages the session ended with.
    let document = fixture("e2e-reorder.pdf");
    let saved = scratch("e2e-reorder-saved.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(false),
        "a freshly opened document reported unsaved changes"
    );

    // Move page 1 to position 3. Nothing on disk changes.
    let id = serve.send(
        "move_page",
        &[("from", Value::from(1)), ("to", Value::from(3))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("edited"),
        "move_page did not report an edit: {reply}"
    );
    // `pages_reordered`, not `idle`. An edit takes effect in the frame that accepts it,
    // and `idle` is emitted on the *settling edge* — so an edit needing no new
    // rasterization produces no new `idle` at all, and waiting for one hangs. Found
    // exactly that way.
    let event = serve.wait_for_event("pages_reordered");
    assert_eq!(
        event.get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64)
    );
    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        snapshot.get("can_undo").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(PAGES as u64),
        "a move changed the page count"
    );

    // Delete a page: the count follows.
    let id = serve.send("delete_page", &[("page", Value::from(2))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );
    serve.wait_for_event("idle");
    assert_eq!(
        serve.view().get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64 - 1),
        "the deleted page is still counted"
    );

    // Undo walks back one edit, not all of them.
    let id = serve.send("undo", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );
    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(PAGES as u64),
        "undo did not restore the deleted page"
    );
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(true),
        "undo went back further than one edit"
    );

    // Save As to a new file, and wait for it — the reply only means it started.
    let id = serve.send(
        "save_as",
        &[("path", Value::from(saved.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("saving"),
        "a save must not claim to be finished before the file exists"
    );
    let event = serve.wait_for_event("saved");
    assert!(
        event
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("e2e-reorder-saved")),
        "unexpected save event: {event}"
    );

    // The file exists, opens, and has the pages the session ended with.
    let bytes = std::fs::read(&saved).expect("the save should exist on disk");
    assert!(bytes.starts_with(b"%PDF"), "not a PDF");
    let reopened = Serve::start(&saved);
    reopened.wait_for_event("idle");
    let mut reopened = reopened;
    assert_eq!(
        reopened.view().get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64),
        "the saved document has the wrong number of pages"
    );
    reopened.quit();

    serve.quit();
}

#[test]
fn the_page_grid_can_be_opened_and_closed_by_command() {
    let Some(_window) = e2e("the_page_grid_can_be_opened_and_closed_by_command") else {
        return;
    };

    // The grid is chrome, not a document edit, so it gets a command rather than being
    // click-only. Unlike the file dialog there is no argument for leaving it out: it
    // changes what is on screen, and an agent that opens it can also close it, so it
    // cannot become a state something gets stuck in.
    let document = fixture("e2e-grid.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    assert_eq!(
        serve.snapshot().get("thumbnails").and_then(Value::as_bool),
        Some(false),
        "the grid was showing before it was asked for"
    );

    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(
        serve.snapshot().get("thumbnails").and_then(Value::as_bool),
        Some(true)
    );

    // Asking for what is already true is `unchanged`, not an error — the same
    // convention every other command follows.
    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );

    let id = serve.send("set_thumbnails", &[("visible", Value::from(false))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(
        serve.snapshot().get("thumbnails").and_then(Value::as_bool),
        Some(false)
    );

    // A missing or non-boolean argument is refused rather than guessed at.
    let id = serve.send("set_thumbnails", &[]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(false),
        "set_thumbnails with no argument was accepted"
    );

    serve.quit();
}

#[test]
fn saving_an_unedited_document_over_itself_is_refused() {
    let Some(_window) = e2e("saving_an_unedited_document_over_itself_is_refused") else {
        return;
    };

    // Rewriting the file would not even be byte-identical — the writer makes its own
    // encoding choices — so an unedited save is a no-op rather than a harmless one.
    let document = fixture("e2e-no-op-save.pdf");
    let before = std::fs::read(&document).expect("should read");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let id = serve.send("save", &[]);
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("unchanged"),
        "an unedited save was not refused: {reply}"
    );
    assert_eq!(
        std::fs::read(&document).expect("should read"),
        before,
        "the file was rewritten anyway"
    );

    serve.quit();
}

#[test]
fn an_empty_window_can_still_be_captured() {
    let Some(_window) = e2e("an_empty_window_can_still_be_captured") else {
        return;
    };

    // The regression: `capture` waited on `open.settled() && !cache.is_empty()`, which
    // with no document is false forever — so the request was never sent and the
    // attempt burned its whole frame budget before reporting "no screenshot arrived".
    // Harmless while a path was mandatory. Since Goal 3 an empty window is how the
    // program starts, so it is the first thing anyone would try to capture.
    let capture = scratch("e2e-empty-window.png");
    let mut child = Command::new(env!("CARGO_BIN_EXE_porpoise"))
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("porpoise should launch");
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    let mut serve = Serve {
        child,
        stdin,
        lines,
        next_id: 1,
    };

    let id = serve.send(
        "capture",
        &[("path", Value::from(capture.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    let event = serve.wait_for_event("captured");
    assert!(
        event.get("path").and_then(Value::as_str).is_some(),
        "unexpected capture event: {event}"
    );

    let bytes = std::fs::read(&capture).expect("the capture should exist on disk");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("should be a readable PNG");
    assert!(reader.info().width > 100, "captured a sliver of a window");

    serve.quit();
}

#[test]
fn idle_is_never_reported_while_a_move_is_still_pending() {
    let Some(_window) = e2e("idle_is_never_reported_while_a_move_is_still_pending") else {
        return;
    };

    // `idle` is the field every client is told to wait on, so it must not be true while
    // the view still has somewhere to go. It used to count only render work, and a
    // scroll command records a *request* that the shell carries out while painting — so
    // for a frame after every command, `idle` was true and the view had not moved.
    //
    // A minimised window turns that one-frame window into an indefinite one, because
    // painting stops entirely. Driving one by hand, `go_to_page 7` replied "changed",
    // reported `idle: true`, and left `current_page` at 3 until the window came back.
    //
    // It reads the *event stream* rather than polling snapshots, and that distinction is
    // the whole test. On a visible window the bad frame is about 16 ms wide, and a
    // snapshot round-trip through the pipe is slower than that — so a polling version of
    // this test passed against the unfixed code, which is how it was found to be
    // worthless. `view_changed` is emitted inside the same frame that accepted the
    // command, before the frame's painting consumes the request, so the offending state
    // appears there and nowhere else.
    let document = fixture("e2e-idle-honesty.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    for page in [4, 2, 6, 1] {
        let id = serve.send("go_to_page", &[("page", Value::from(page))]);
        assert_eq!(
            serve.reply_to(id).get("ok").and_then(Value::as_bool),
            Some(true)
        );

        // Every message until this move settles, checking each reported state.
        loop {
            let message = serve.next_message();
            if message.get("event").and_then(Value::as_str) == Some("idle") {
                break;
            }
            let Some(snapshot) = message.get("snapshot") else {
                continue;
            };
            let Some(view) = snapshot.get("view") else {
                continue;
            };
            if snapshot.get("idle").and_then(Value::as_bool) == Some(true) {
                assert_eq!(
                    view.get("pending_scroll_pt"),
                    Some(&Value::Null),
                    "claimed idle with a move still pending: {view}"
                );
                assert_eq!(
                    view.get("pending_scroll_left_pt"),
                    Some(&Value::Null),
                    "claimed idle with a pan still pending: {view}"
                );
            }
        }

        let view = serve.view();
        assert_eq!(
            view.get("current_page").and_then(Value::as_u64),
            Some(page),
            "settled, but not where we asked to be: {view}"
        );
    }

    serve.quit();
}

#[test]
fn a_file_that_will_not_open_reports_a_visible_reason() {
    let Some(_window) = e2e("a_file_that_will_not_open_reports_a_visible_reason") else {
        return;
    };

    // Goal 3's third part. Until the file picker existed, a bad path was a startup
    // error printed to a terminal; now it is something a person does at runtime, so
    // the reason has to be readable rather than going only to `tracing::warn!` — which
    // for a windowed app means nowhere. The status bar and this field show the same
    // string, so testing one covers what the other displays.
    let document = fixture("e2e-open-failure.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    assert_eq!(
        serve.snapshot().get("last_error"),
        None,
        "a healthy session reported an error"
    );

    let missing = scratch("e2e-does-not-exist.pdf");
    let id = serve.send(
        "open",
        &[("path", Value::from(missing.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(false),
        "opening a missing file claimed to succeed: {reply}"
    );

    let snapshot = serve.snapshot();
    let reported = snapshot
        .get("last_error")
        .and_then(Value::as_str)
        .expect("a failed open should leave a readable reason");
    assert!(
        reported.contains("e2e-does-not-exist"),
        "the reason does not name the file: {reported}"
    );

    // The document that was already open stays open: a failed open must not also
    // close what someone was reading.
    assert_eq!(
        serve.view().get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64),
        "a failed open discarded the working document"
    );

    // And a successful open clears it, so a stale message cannot outlive its cause.
    let id = serve.send(
        "open",
        &[("path", Value::from(document.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        serve.snapshot().get("last_error"),
        None,
        "the error survived a successful open"
    );

    serve.quit();
}

#[test]
fn a_malformed_message_is_refused_without_killing_the_session() {
    let Some(_window) = e2e("a_malformed_message_is_refused_without_killing_the_session") else {
        return;
    };

    let document = fixture("e2e-malformed.pdf");
    let mut serve = Serve::start(&document);

    // Drain the opening chatter so the assertions below are unambiguous.
    serve.wait_for_event("idle");

    // Every one of these is something an agent will send eventually.
    for bad in [
        "not json",
        "{",
        "[]",
        r#"{"command":"teleport"}"#,
        r#"{"command":"go_to_page","page":"three"}"#,
        r#"{"command":"open"}"#,
        r#"{"id":-1,"command":"next_page"}"#,
    ] {
        serve.send_raw(bad);
    }

    // The session must still be alive and correct afterwards.
    let id = serve.send("go_to_page", &[("page", Value::from(2))]);
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(true),
        "the session did not survive malformed input: {reply}"
    );

    serve.wait_for_event("idle");
    let view = serve.view();
    assert_eq!(view.get("current_page").and_then(Value::as_u64), Some(2));

    serve.quit();
}

#[test]
fn a_refused_command_explains_itself_and_changes_nothing() {
    let Some(_window) = e2e("a_refused_command_explains_itself_and_changes_nothing") else {
        return;
    };

    let document = fixture("e2e-refused.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let id = serve.send("go_to_page", &[("page", Value::from(999))]);
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(false));
    let error = reply
        .get("error")
        .and_then(Value::as_str)
        .expect("a reason")
        .to_owned();
    assert!(error.contains("999"), "unhelpful: {error}");
    assert!(
        error.contains(&PAGES.to_string()),
        "should name the real page count: {error}"
    );

    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(1),
        "a refused command moved the view"
    );

    // Page zero does not exist under one-based numbering. Unlike page 999 above it
    // is refused by the wire *type* rather than by a bounds check, so it fails during
    // decoding rather than in `apply` — a different path, and one that used to throw
    // the request id away. `reply_to` only returns on a matching id, so this call
    // hanging is exactly the failure a real client would see.
    let id = serve.send("go_to_page", &[("page", Value::from(0))]);
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(false),
        "page 0 was accepted: {reply}"
    );

    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(1),
        "a refused command moved the view"
    );

    serve.quit();
}

#[test]
fn closing_stdin_shuts_the_program_down() {
    let Some(_window) = e2e("closing_stdin_shuts_the_program_down") else {
        return;
    };

    // The convention every stdio protocol follows: when the controller goes away,
    // so do we. Without it an abandoned window survives its agent.
    let document = fixture("e2e-hangup.pdf");
    let mut child = Command::new(env!("CARGO_BIN_EXE_porpoise"))
        .arg("serve")
        .arg(&document)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("porpoise should launch");

    // Dropping stdin closes the pipe.
    drop(child.stdin.take());

    let status = child.wait().expect("should exit on its own");
    assert!(status.success(), "exited with {status}");
}

#[test]
fn paged_navigation_reaches_the_last_page_in_a_window_taller_than_a_page() {
    let Some(_window) =
        e2e("paged_navigation_reaches_the_last_page_in_a_window_taller_than_a_page")
    else {
        return;
    };

    // A regression a person found before any test did. The last page's top sits at
    // `content_height - page_height` and scrolling stops at `content_height -
    // viewport_height`, so once the window is TALLER than a page, the last page's top is
    // past the end of the scroll range: it can fill the window and still never be the page
    // you are "on". The counter stuck at 5 of 6 and PageDown did nothing.
    //
    // The fixture's pages are 300 pt, so this zooms until the window covers slightly more
    // than one of them. Every other test here runs at fit-width, where the window is
    // *shorter* than a page — which is exactly why none of them caught it.
    let document = fixture("e2e-paged-end.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let id = serve.send("set_scroll_mode", &[("mode", Value::from("paged"))]);
    serve.reply_to(id);
    let id = serve.send(
        "set_zoom",
        &[("target", serde_json::json!({ "fixed": 2.3 }))],
    );
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );

    let view = serve.view();
    let content = view
        .get("content_height_pt")
        .and_then(Value::as_f64)
        .expect("a content height");
    let max_scroll = view
        .get("max_scroll_pt")
        .and_then(Value::as_f64)
        .expect("a scroll limit");
    let viewport_pt = content - max_scroll;
    assert!(
        viewport_pt > 300.0,
        "the window is not taller than a page, so this cannot reproduce: {viewport_pt} pt"
    );

    // Step forward past the end. Paged mode advances one page at a time, so more steps
    // than pages guarantees arrival.
    for _ in 0..PAGES + 2 {
        let id = serve.send("next_page", &[]);
        serve.reply_to(id);
    }
    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(PAGES as u64),
        "paged navigation stalled short of the last page: {view}"
    );

    // And the direct jump agrees with the walk.
    let id = serve.send("first_page", &[]);
    serve.reply_to(id);
    let id = serve.send("last_page", &[]);
    serve.reply_to(id);
    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(PAGES as u64),
        "last_page did not land on the last page: {view}"
    );

    // Going back from the end moves exactly one page, rather than skipping one because the
    // end of the document reported the page before it.
    let id = serve.send("previous_page", &[]);
    serve.reply_to(id);
    let view = serve.view();
    assert_eq!(
        view.get("current_page").and_then(Value::as_u64),
        Some(PAGES as u64 - 1),
        "going back from the end skipped a page: {view}"
    );

    serve.quit();
}

// --- Unsaved page changes ---------------------------------------------------
//
// The guard sits in front of dispatch rather than on the X button, and these tests are
// the reason. A gesture-level check would have left the most safety-critical behaviour
// in the program as the only one with no automated test, because nothing can press an X.
// See `crates/porpoise-app/src/confirm.rs`.

/// Reorders a page so the session has something to lose.
fn make_it_dirty(serve: &mut Serve) {
    let id = serve.send(
        "move_page",
        &[("from", Value::from(1)), ("to", Value::from(3))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("edited"),
        "the fixture did not become dirty: {reply}"
    );
    assert_eq!(
        serve
            .snapshot()
            .get("unsaved_changes")
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn quitting_with_unsaved_changes_asks_before_losing_them() {
    let Some(_window) = e2e("quitting_with_unsaved_changes_asks_before_losing_them") else {
        return;
    };

    let document = fixture("e2e-guard-quit.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    make_it_dirty(&mut serve);

    // 1. Quit is held back rather than carried out.
    let id = serve.send("quit", &[]);
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("needs_answer"),
        "quit went ahead and lost the reordering: {reply}"
    );

    // 2. The program is still running and says what it is asking about. Being able to
    //    read this is what lets a client answer without guessing.
    let snapshot = serve.snapshot();
    let asking = snapshot
        .get("awaiting_answer")
        .and_then(Value::as_str)
        .expect("the snapshot should say what is waiting");
    assert!(asking.contains("quit"), "unhelpful: {asking}");
    assert_eq!(
        snapshot.get("idle").and_then(Value::as_bool),
        Some(false),
        "reported idle with a question nobody had answered: {snapshot}"
    );

    // 3. Cancel puts it back the way it was: still open, still dirty, no question.
    let id = serve.send("answer", &[("choice", Value::from("cancel"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("cancelled")
    );
    let snapshot = serve.snapshot();
    assert_eq!(snapshot.get("awaiting_answer"), None);
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(true),
        "cancelling threw the changes away: {snapshot}"
    );

    // 4. Asked again and answered with discard, it really does quit.
    let id = serve.send("quit", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("needs_answer")
    );
    let id = serve.send("answer", &[("choice", Value::from("discard"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("quitting")
    );
    serve.expect_exit();
}

#[test]
fn opening_another_document_with_unsaved_changes_asks_first() {
    let Some(_window) = e2e("opening_another_document_with_unsaved_changes_asks_first") else {
        return;
    };

    // `open` replaces the document, so it loses an edit exactly as quitting does. Two
    // commands, one guard — which is the point of guarding dispatch rather than each
    // producer.
    let document = fixture("e2e-guard-open-a.pdf");
    let other = fixture("e2e-guard-open-b.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    make_it_dirty(&mut serve);

    let id = serve.send(
        "open",
        &[("path", Value::from(other.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("needs_answer"),
        "opening another file discarded the reordering without asking"
    );
    let snapshot = serve.snapshot();
    let asking = snapshot
        .get("awaiting_answer")
        .and_then(Value::as_str)
        .expect("should say what is waiting");
    assert!(
        asking.contains("e2e-guard-open-b"),
        "did not say which file: {asking}"
    );
    // Still the first document, untouched.
    assert!(
        snapshot
            .get("document")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("e2e-guard-open-a")),
        "the document was replaced before the question was answered: {snapshot}"
    );

    // Discard, and the open finally happens — with the edit gone, as asked.
    let id = serve.send("answer", &[("choice", Value::from("discard"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("opened")
    );
    let snapshot = serve.snapshot();
    assert!(
        snapshot
            .get("document")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("e2e-guard-open-b")),
        "{snapshot}"
    );
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(false)
    );

    serve.quit();
}

#[test]
fn answering_save_writes_the_file_before_going_ahead() {
    let Some(_window) = e2e("answering_save_writes_the_file_before_going_ahead") else {
        return;
    };

    // The ordering that is easy to get wrong: a save takes about a second and runs off
    // the UI thread, so firing both and hoping would close the document before the file
    // existed. `close` rather than `quit` as the intent, so the session survives to prove
    // the bytes landed.
    let document = fixture("e2e-guard-save.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let id = serve.send("delete_page", &[("page", Value::from(2))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );

    let id = serve.send("close", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("needs_answer")
    );

    let id = serve.send("answer", &[("choice", Value::from("save"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("saving"),
        "answering save must not claim the file is written yet"
    );

    // The write lands first, and only then is the document closed.
    serve.wait_for_event("saved");
    serve.wait_for_event("document_closed");
    let snapshot = serve.snapshot();
    // `document` is serialized as null when nothing is open rather than being omitted,
    // so this asks whether there is a path, not whether the key is there.
    assert!(
        snapshot.get("document").and_then(Value::as_str).is_none(),
        "still open: {snapshot}"
    );
    assert_eq!(snapshot.get("awaiting_answer"), None);

    // And the file really has the shorter document, so what was saved was the edit
    // rather than whatever was on disk before.
    let mut reopened = Serve::start(&document);
    reopened.wait_for_event("idle");
    assert_eq!(
        reopened.view().get("page_count").and_then(Value::as_u64),
        Some(PAGES as u64 - 1),
        "the file does not contain the edit that was saved for us"
    );
    reopened.quit();

    serve.quit();
}

#[test]
fn a_saved_document_stops_claiming_unsaved_changes() {
    let Some(_window) = e2e("a_saved_document_stops_claiming_unsaved_changes") else {
        return;
    };

    // Before this, `unsaved_changes` meant "differs from the document as first opened"
    // and nothing ever cleared it — so a saved file went on claiming changes forever.
    // Survivable while it only lit a status bar; not survivable once a warning is built
    // on it, because a warning that fires when nothing is at risk is one people learn to
    // click straight through.
    let document = fixture("e2e-guard-clean.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    make_it_dirty(&mut serve);

    let id = serve.send("save", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("saving")
    );
    serve.wait_for_event("saved");

    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(false),
        "still claiming unsaved changes after writing them: {snapshot}"
    );

    // Which means quitting is not questioned at all — and `quit` fails rather than
    // hanging if it ever is, so this line is the assertion.
    serve.quit();
}

#[test]
fn answering_save_with_nothing_left_to_save_still_goes_ahead() {
    let Some(_window) = e2e("answering_save_with_nothing_left_to_save_still_goes_ahead") else {
        return;
    };

    // Reachable because the question box does not block the control channel: the order
    // can be put back while it is up. "Save, then continue" with nothing to save is just
    // "continue" — leaving the question standing would strand it with no answer that
    // works, since saving an unedited document over itself is refused by design.
    let document = fixture("e2e-guard-nothing-to-save.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    make_it_dirty(&mut serve);

    let id = serve.send("close", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("needs_answer")
    );

    // Undo, so the document matches the file again while the question waits.
    let id = serve.send("undo", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );
    assert_eq!(
        serve
            .snapshot()
            .get("unsaved_changes")
            .and_then(Value::as_bool),
        Some(false)
    );

    let id = serve.send("answer", &[("choice", Value::from("save"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("closed"),
        "stranded the question with nothing to save"
    );

    serve.quit();
}

#[test]
fn answering_when_nothing_was_asked_does_nothing() {
    let Some(_window) = e2e("answering_when_nothing_was_asked_does_nothing") else {
        return;
    };

    // A stray `answer` must not be a way to close the document by accident. Worth
    // pinning because `discard` is the destructive choice and this is the path a
    // confused client takes.
    let document = fixture("e2e-guard-stray.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    for choice in ["discard", "save", "cancel"] {
        let id = serve.send("answer", &[("choice", Value::from(choice))]);
        let reply = serve.reply_to(id);
        assert_eq!(
            reply.get("outcome").and_then(Value::as_str),
            Some("unchanged"),
            "a stray {choice} did something: {reply}"
        );
    }

    let snapshot = serve.snapshot();
    assert!(
        snapshot.get("document").and_then(Value::as_str).is_some(),
        "a stray answer closed the document: {snapshot}"
    );

    serve.quit();
}
