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

use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use porpoise_testkit::{multi_page_pdf, pdf_with_page_sizes};
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
    /// How many `page_failed` events have come past, counted on the way through.
    ///
    /// Every read path funnels through [`Self::next_message`], so counting there catches
    /// them whoever was waiting for what. A `Cell` because that method takes `&self`.
    failures_seen: Cell<usize>,
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
            failures_seen: Cell::new(0),
        }
    }

    /// Starts the binary with nothing open — no file argument on the command line.
    /// What an agent sees before the first `open`, and the shape every "refused
    /// with nothing open" test needs.
    fn start_empty() -> Self {
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

        Self {
            child,
            stdin,
            lines,
            next_id: 1,
            failures_seen: Cell::new(0),
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
        let message: Value = match self.lines.recv_timeout(REPLY_TIMEOUT) {
            Ok(line) => serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("unparsable output {line:?}: {error}")),
            Err(RecvTimeoutError::Timeout) => {
                panic!("no output within {REPLY_TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the program exited unexpectedly"),
        };
        if message.get("event").and_then(Value::as_str) == Some("page_failed") {
            self.failures_seen.set(self.failures_seen.get() + 1);
        }
        message
    }

    /// How many pages have been reported as failing to rasterize, cumulatively.
    ///
    /// A page the renderer refuses should be reported a bounded number of times and then
    /// left alone. This counter is how that is checked, because "stopped asking" is not
    /// visible in a snapshot — only in the absence of further events.
    fn failures_seen(&self) -> usize {
        self.failures_seen.get()
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
    fixture_of(name, PAGES)
}

/// A fixture of a chosen length, for the tests that care how long a document is.
fn fixture_of(name: &str, pages: usize) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    std::fs::write(&path, multi_page_pdf(pages, 200, 300)).expect("should write the fixture");
    path
}

/// How much document the window covers vertically, in points.
///
/// From the window and the zoom, which is the only derivation that holds in both scroll
/// modes: paged mode confines the scroll range to one page, so subtracting the scroll
/// limit from the content height measures a page and not a window.
fn window_height_pt(view: &Value) -> f64 {
    let pixels = view
        .get("viewport_height_px")
        .and_then(Value::as_f64)
        .expect("a viewport height");
    let zoom = view.get("zoom").and_then(Value::as_f64).expect("a zoom");
    pixels / zoom
}

/// The `view` field of `key`, as an integer.
fn page_field(view: &Value, key: &str) -> u64 {
    view.get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no {key} in {view}"))
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
    let mut serve = Serve::start_empty();

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
fn an_agent_can_insert_pages_from_another_document_and_save_it() {
    let Some(_window) = e2e("an_agent_can_insert_pages_from_another_document_and_save_it") else {
        return;
    };

    // Goal 5 through the running program rather than the library.
    // `porpoise-render`'s merge tests prove the *bytes* are right by comparing
    // pixels; this proves the command path reaches them — that `insert_file` grows
    // the page count by the second file's, that the pages it added can be moved
    // like any other, and that the saved file has the combined page count.
    let primary = fixture_of("e2e-insert-primary.pdf", 3);
    let inserted = fixture_of("e2e-insert-inserted.pdf", 2);
    let saved = scratch("e2e-insert-saved.pdf");

    let mut serve = Serve::start(&primary);
    serve.wait_for_event("idle");

    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(3)
    );
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(false)
    );

    let id = serve.send(
        "insert_file",
        &[("path", Value::from(inserted.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("outcome").and_then(Value::as_str),
        Some("edited"),
        "insert_file did not report an edit: {reply}"
    );
    // `pages_reordered`, the same event every page edit reports — see
    // `an_agent_can_reorder_a_document_and_save_it` for why `idle` is the wrong thing
    // to wait for here.
    let event = serve.wait_for_event("pages_reordered");
    assert_eq!(
        event.get("page_count").and_then(Value::as_u64),
        Some(5),
        "expected 3 + 2 pages after inserting"
    );

    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(true),
        "inserting pages was not counted as an unsaved change"
    );
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(5)
    );

    // The point of the whole design: an inserted page is an ordinary one. Move the
    // first page of the inserted document (position 4) to the front.
    let id = serve.send(
        "move_page",
        &[("from", Value::from(4)), ("to", Value::from(1))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited"),
        "the inserted page could not be moved like any other"
    );
    serve.wait_for_event("pages_reordered");

    // Save As, and wait for it — the reply only means it started.
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
            .is_some_and(|path| path.contains("e2e-insert-saved")),
        "unexpected save event: {event}"
    );

    // The merged file exists, opens, and has the combined page count.
    let bytes = std::fs::read(&saved).expect("the save should exist on disk");
    assert!(bytes.starts_with(b"%PDF"), "not a PDF");
    let reopened = Serve::start(&saved);
    reopened.wait_for_event("idle");
    let mut reopened = reopened;
    assert_eq!(
        reopened.view().get("page_count").and_then(Value::as_u64),
        Some(5),
        "the saved, merged document has the wrong number of pages"
    );
    reopened.quit();

    serve.quit();
}

#[test]
fn an_agent_can_stage_a_document_and_insert_its_pages_and_save_it() {
    // The merge tab's own command surface (`docs/goal-5-plan.md` §10.6), the same
    // evidentiary bar `an_agent_can_insert_pages_from_another_document_and_save_it`
    // sets for `insert_file`: this proves the command path reaches `PageOrder::
    // stage`/`insert_pages`, that staging is visible in the snapshot, that a page
    // can be dropped precisely mid-document rather than only at the end, that the
    // same staged document can be drawn from twice, and that clearing it leaves
    // already-placed pages untouched.
    let primary = fixture_of("e2e-stage-primary.pdf", 3);
    let staged = fixture_of("e2e-stage-staged.pdf", 3);
    let saved = scratch("e2e-stage-saved.pdf");

    let mut serve = Serve::start(&primary);
    serve.wait_for_event("idle");

    assert_eq!(
        serve.snapshot().get("staged"),
        None,
        "nothing was staged yet"
    );

    let id = serve.send(
        "stage_document",
        &[("path", Value::from(staged.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed"),
        "stage_document did not report a change"
    );
    let snapshot = serve.snapshot();
    assert!(
        snapshot
            .get("staged")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("e2e-stage-staged")),
        "the staged document's path was not reported: {snapshot}"
    );
    // Staging adds nothing to the document being edited.
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(3),
        "staging alone changed the open document's page count"
    );
    assert_eq!(
        snapshot.get("unsaved_changes").and_then(Value::as_bool),
        Some(false),
        "staging alone counted as an unsaved change"
    );

    // Page 1 of the staged document, landing between the primary's pages 1 and 2 —
    // not appended at the end, which is the whole point of this over `insert_file`.
    let id = serve.send(
        "insert_pages",
        &[("pages", Value::from(vec![1])), ("at", Value::from(2))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited"),
        "insert_pages did not report an edit"
    );
    let event = serve.wait_for_event("pages_reordered");
    assert_eq!(
        event.get("page_count").and_then(Value::as_u64),
        Some(4),
        "expected 3 + 1 pages after inserting"
    );

    // The same staged document, drawn from a second time — proof that staging
    // survives more than one drag, per `PageOrder::insert_pages`'s own contract.
    let id = serve.send(
        "insert_pages",
        &[("pages", Value::from(vec![2, 3])), ("at", Value::from(1))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited"),
        "a second insert_pages from the same staged document was refused"
    );
    let event = serve.wait_for_event("pages_reordered");
    assert_eq!(
        event.get("page_count").and_then(Value::as_u64),
        Some(6),
        "expected 4 + 2 pages after the second insert"
    );

    // Clearing staging does not touch pages already placed.
    let id = serve.send("clear_staging", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    let snapshot = serve.snapshot();
    assert_eq!(
        snapshot.get("staged"),
        None,
        "clear_staging did not clear the staged path"
    );
    assert_eq!(
        snapshot
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(6),
        "clearing staging discarded already-inserted pages"
    );

    // Clearing again is `unchanged`, the same convention every other toggle here
    // follows for "already true".
    let id = serve.send("clear_staging", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );

    let id = serve.send(
        "save_as",
        &[("path", Value::from(saved.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("saving")
    );
    serve.wait_for_event("saved");

    let bytes = std::fs::read(&saved).expect("the save should exist on disk");
    assert!(bytes.starts_with(b"%PDF"), "not a PDF");
    let reopened = Serve::start(&saved);
    reopened.wait_for_event("idle");
    let mut reopened = reopened;
    assert_eq!(
        reopened.view().get("page_count").and_then(Value::as_u64),
        Some(6),
        "the saved, merged document has the wrong number of pages"
    );
    reopened.quit();

    serve.quit();
}

#[test]
fn an_agent_can_select_all_of_the_staged_documents_pages() {
    // `set_staged_selection` (`docs/goal-5-plan.md` §10.6): the merge tab's
    // "Select All" button and an agent's equivalent request go through the
    // identical command. Named by `path` rather than left implicit the way
    // `insert_pages` is — see the command's own doc comment — so this also
    // proves a request naming the wrong document is refused rather than
    // silently picking out whatever happens to be staged.
    let primary = fixture_of("e2e-select-all-primary.pdf", 2);
    let staged = fixture_of("e2e-select-all-staged.pdf", 3);

    let mut serve = Serve::start(&primary);
    serve.wait_for_event("idle");

    let staged_selection = |serve: &mut Serve| -> Vec<u64> {
        serve
            .snapshot()
            .get("staged_selection")
            .and_then(Value::as_array)
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default()
    };

    let id = serve.send(
        "stage_document",
        &[("path", Value::from(staged.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert!(
        staged_selection(&mut serve).is_empty(),
        "something was picked out right after staging"
    );

    // Naming a document other than the one actually staged is refused, not
    // silently applied to whatever is staged instead.
    let id = serve.send(
        "set_staged_selection",
        &[
            ("path", Value::from(primary.to_string_lossy().as_ref())),
            ("pages", Value::from(vec![1])),
        ],
    );
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(false));
    assert!(
        reply
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("is staged")),
        "unexpected error: {reply}"
    );
    assert!(
        staged_selection(&mut serve).is_empty(),
        "the refused request still picked something out"
    );

    let id = serve.send(
        "set_staged_selection",
        &[
            ("path", Value::from(staged.to_string_lossy().as_ref())),
            ("pages", Value::from(vec![1, 2, 3])),
        ],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed"),
        "set_staged_selection did not report a change"
    );
    assert_eq!(
        staged_selection(&mut serve),
        vec![1, 2, 3],
        "every staged page should be picked out"
    );

    // Asking for the same selection again is `unchanged`, the same convention
    // every other toggle here follows for "already true".
    let id = serve.send(
        "set_staged_selection",
        &[
            ("path", Value::from(staged.to_string_lossy().as_ref())),
            ("pages", Value::from(vec![1, 2, 3])),
        ],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );

    // An empty list clears it, the same as `set_selection` does for the main grid.
    let id = serve.send(
        "set_staged_selection",
        &[
            ("path", Value::from(staged.to_string_lossy().as_ref())),
            ("pages", Value::from(Vec::<i64>::new())),
        ],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert!(staged_selection(&mut serve).is_empty());

    // Nothing staged at all is refused too, not just a mismatched path.
    let id = serve.send("clear_staging", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    let id = serve.send(
        "set_staged_selection",
        &[
            ("path", Value::from(staged.to_string_lossy().as_ref())),
            ("pages", Value::from(vec![1])),
        ],
    );
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(false));
    assert!(
        reply
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("nothing is staged")),
        "unexpected error: {reply}"
    );

    serve.quit();
}

#[test]
fn staging_a_document_with_nothing_open_is_refused() {
    let Some(_window) = e2e("staging_a_document_with_nothing_open_is_refused") else {
        return;
    };

    // The same empty-window shape `inserting_a_file_with_nothing_open_is_refused`
    // checks for `insert_file` — staging needs a document to merge into, same as
    // inserting does.
    let mut serve = Serve::start_empty();

    let id = serve.send(
        "stage_document",
        &[("path", Value::from("does-not-matter.pdf"))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(false),
        "staged a document into a window with nothing open: {reply}"
    );
    assert!(
        reply
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error.contains("nothing is open")),
        "unexpected error: {reply}"
    );

    serve.quit();
}

#[test]
fn inserting_a_file_with_nothing_open_is_refused() {
    let Some(_window) = e2e("inserting_a_file_with_nothing_open_is_refused") else {
        return;
    };

    // No file argument, the same empty-window shape
    // `an_agent_can_open_a_document_that_was_not_on_the_command_line` starts from —
    // there is nothing to insert pages into.
    let mut serve = Serve::start_empty();

    let inserted = fixture_of("e2e-insert-refused.pdf", 2);
    let id = serve.send(
        "insert_file",
        &[("path", Value::from(inserted.to_string_lossy().as_ref()))],
    );
    let reply = serve.reply_to(id);
    assert_eq!(
        reply.get("ok").and_then(Value::as_bool),
        Some(false),
        "inserted pages into a window with nothing open: {reply}"
    );

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
fn pages_can_be_picked_out_and_moved_as_a_group() {
    let Some(_window) = e2e("pages_can_be_picked_out_and_moved_as_a_group") else {
        return;
    };

    // The grid's ctrl+click, shift+click and marquee all come down to `set_selection`,
    // and dragging the result comes down to `move_pages`. Neither gesture can be
    // synthesized here, but both commands can — so what a gesture *authors* is covered
    // even though the pointer itself is not.
    let document = fixture("e2e-group-edit.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let selection = |serve: &mut Serve| -> Vec<u64> {
        serve
            .snapshot()
            .get("selection")
            .and_then(Value::as_array)
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default()
    };

    assert!(
        selection(&mut serve).is_empty(),
        "something was picked out before anything asked for it"
    );

    let id = serve.send("set_selection", &[("pages", Value::from(vec![2, 4, 5]))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(selection(&mut serve), vec![2, 4, 5]);

    // Scattered pages arrive contiguous, in the order they were shown in, starting where
    // they were asked to. Pages 2, 4 and 5 of 1..6 landing at 2 leaves 1, then them.
    let id = serve.send(
        "move_pages",
        &[("from", Value::from(vec![2, 4, 5])), ("to", Value::from(2))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );

    // And the selection followed the pages rather than staying on the positions: they are
    // now at 2, 3, 4.
    assert_eq!(
        selection(&mut serve),
        vec![2, 3, 4],
        "the selection did not follow the pages it was pointing at"
    );

    // One undo puts the whole group back, not one page of it.
    let id = serve.send("undo", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );
    assert_eq!(
        selection(&mut serve),
        vec![2, 4, 5],
        "undo left the group half moved"
    );

    // A group delete, also one step.
    let id = serve.send("delete_pages", &[("pages", Value::from(vec![2, 4, 5]))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("edited")
    );
    assert_eq!(
        serve
            .snapshot()
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(3),
        "a six-page document lost the wrong number of pages"
    );
    assert!(
        selection(&mut serve).is_empty(),
        "pages that are gone still counted as picked out"
    );

    // Deleting everything is refused rather than leaving an arbitrary page behind. It
    // comes back `unchanged` rather than as an error, which is the same convention
    // `delete_page` has always followed for the last remaining page — asking for
    // something that changes nothing is not a failure here. What matters is the
    // document, so that is what this checks.
    let id = serve.send("delete_pages", &[("pages", Value::from(vec![1, 2, 3]))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );
    assert_eq!(
        serve
            .snapshot()
            .get("view")
            .and_then(|view| view.get("page_count"))
            .and_then(Value::as_u64),
        Some(3),
        "emptying the document was accepted"
    );

    // `{"pages":[0]}` is refused for the reason `{"page":0}` is: page numbers start at 1.
    let id = serve.send("set_selection", &[("pages", Value::from(vec![0]))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(false),
        "page zero was accepted in a list"
    );

    // The group edits leave real unsaved changes, so quitting is guarded exactly as it is
    // after a single-page edit — worth pinning here too, since a group edit that skipped
    // the guard would lose work quietly.
    let id = serve.send("quit", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("needs_answer"),
        "a group edit did not count as unsaved changes"
    );
    let id = serve.send("answer", &[("choice", Value::from("discard"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("quitting")
    );
    serve.expect_exit();
}

#[test]
fn the_page_grid_can_be_narrowed_to_a_few_pages() {
    let Some(_window) = e2e("the_page_grid_can_be_narrowed_to_a_few_pages") else {
        return;
    };

    // The search box narrows the grid, and reports both what was typed and what it
    // resolved to — so a client need not reimplement the query parser to know what is on
    // screen. `PAGES` is 6.
    let document = fixture("e2e-page-filter.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let filtered = |serve: &mut Serve| -> Option<Vec<u64>> {
        serve
            .snapshot()
            .get("filtered_pages")
            .and_then(Value::as_array)
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect())
    };

    assert_eq!(
        filtered(&mut serve),
        None,
        "the grid was narrowed before anything asked"
    );

    let id = serve.send("set_page_filter", &[("query", Value::from("2-4"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(filtered(&mut serve), Some(vec![2, 3, 4]));
    assert_eq!(
        serve.snapshot().get("page_filter").and_then(Value::as_str),
        Some("2-4"),
        "the query itself was not reported back"
    );

    // Lists and ranges mix, and a page past the end is dropped rather than refused.
    let id = serve.send("set_page_filter", &[("query", Value::from("1,3-4,99"))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(filtered(&mut serve), Some(vec![1, 3, 4]));

    // A query that reads as nothing is still a filter — `Some([])`, not `None`. The panel
    // says "no pages match" rather than looking broken, and a client can tell the two
    // apart.
    let id = serve.send("set_page_filter", &[("query", Value::from("nonsense"))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true),
        "an unreadable query was refused; it should match nothing instead"
    );
    assert_eq!(filtered(&mut serve), Some(Vec::new()));

    // Asking again for the same query changes nothing, same convention as the rest.
    let id = serve.send("set_page_filter", &[("query", Value::from("nonsense"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );

    // Narrowing clears the selection, because Delete acts on it and pages behind a query
    // are pages nobody can see it is about to remove.
    serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    serve.send("set_page_filter", &[("query", Value::from(""))]);
    serve.send("set_selection", &[("pages", Value::from(vec![1, 2]))]);
    assert_eq!(
        serve
            .snapshot()
            .get("selection")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(2)
    );
    serve.send("set_page_filter", &[("query", Value::from("5"))]);
    assert_eq!(
        serve
            .snapshot()
            .get("selection")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(0),
        "narrowing kept a selection the query had hidden"
    );

    // An omitted query is the empty one, which clears the filter — the same reading the
    // box's ✕ has. Only a non-string is an error.
    let id = serve.send("set_page_filter", &[]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(filtered(&mut serve), None);

    let id = serve.send("set_page_filter", &[("query", Value::from(7))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(false),
        "a numeric query was accepted; it should ask for a string"
    );

    serve.quit();
}

#[test]
fn the_search_box_narrows_the_staging_viewport_independently() {
    // `docs/goal-5-plan.md` M30: one query, two documents, almost never the same
    // page count — so the staging viewport needs its *own* resolution of the same
    // text rather than reusing the primary's, which `Viewer::staged_filter`'s own
    // doc comment argues for. Proved here with a primary short enough that a query
    // clamps against *it* while still matching more of a longer staged document.
    let primary = fixture_of("e2e-staged-filter-primary.pdf", 3);
    let staged = fixture_of("e2e-staged-filter-staged.pdf", 10);

    let mut serve = Serve::start(&primary);
    serve.wait_for_event("idle");

    let staged_filtered = |serve: &mut Serve| -> Option<Vec<u64>> {
        serve
            .snapshot()
            .get("staged_filtered_pages")
            .and_then(Value::as_array)
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect())
    };

    assert_eq!(
        staged_filtered(&mut serve),
        None,
        "nothing is staged yet, so there is nothing to narrow"
    );

    let id = serve.send(
        "stage_document",
        &[("path", Value::from(staged.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    // Staged, but nothing typed yet: still no filter on either side.
    assert_eq!(staged_filtered(&mut serve), None);

    // "1-9" against the 3-page primary clamps to all three of its pages; against
    // the 10-page staged document it reaches every one of pages 1 through 9. Reusing
    // the primary's resolved `[1,2,3]` for the staging viewport would wrongly hide
    // pages 4 through 9, which is exactly the bug a shared result would cause.
    let id = serve.send("set_page_filter", &[("query", Value::from("1-9"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(
        serve
            .snapshot()
            .get("filtered_pages")
            .and_then(Value::as_array)
            .map(|pages| pages.iter().filter_map(Value::as_u64).collect::<Vec<_>>()),
        Some(vec![1, 2, 3]),
        "the 3-page primary should clamp to its own three pages"
    );
    assert_eq!(
        staged_filtered(&mut serve),
        Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
        "the 10-page staged document should show pages 1 through 9, not the primary's three"
    );

    // Clearing staging drops the staged half of the filter, even with the query
    // still typed — there is nothing left to narrow.
    let id = serve.send("clear_staging", &[]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(staged_filtered(&mut serve), None);

    serve.quit();
}

#[test]
fn leaving_reorganize_mode_forgets_the_selection() {
    let Some(_window) = e2e("leaving_reorganize_mode_forgets_the_selection") else {
        return;
    };

    // A selection nobody can see is a trap, because Delete acts on it. Cleared on the way
    // out rather than ignored while hidden, so the state and the snapshot agree.
    let document = fixture("e2e-selection-clear.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let picked = |serve: &mut Serve| -> usize {
        serve
            .snapshot()
            .get("selection")
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };

    serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    serve.send("set_selection", &[("pages", Value::from(vec![1, 2]))]);
    assert_eq!(picked(&mut serve), 2);

    // Navigation mode does not show a selection, so it must not keep one.
    serve.send("set_grid_mode", &[("mode", Value::from("navigate"))]);
    assert_eq!(
        picked(&mut serve),
        0,
        "switching to navigation kept a selection nothing was drawing"
    );

    // And the same when the panel closes entirely.
    serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    serve.send("set_selection", &[("pages", Value::from(vec![1, 2]))]);
    assert_eq!(picked(&mut serve), 2);
    serve.send("set_thumbnails", &[("visible", Value::from(false))]);
    assert_eq!(
        picked(&mut serve),
        0,
        "closing the grid kept a selection nothing was drawing"
    );

    serve.quit();
}

#[test]
fn the_page_grid_mode_can_be_set_by_command() {
    let Some(_window) = e2e("the_page_grid_mode_can_be_set_by_command") else {
        return;
    };

    // The mode decides what clicking a thumbnail does, so it is state a client has to be
    // able to read as well as set — otherwise a screenshot of the grid is ambiguous about
    // what a click in it would have done.
    let document = fixture("e2e-grid-mode.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    assert_eq!(
        serve.snapshot().get("grid_mode").and_then(Value::as_str),
        Some("navigate"),
        "the grid did not open in navigation mode"
    );

    let id = serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(
        serve.snapshot().get("grid_mode").and_then(Value::as_str),
        Some("reorganize")
    );

    // Same convention as every other command: already-true is `unchanged`, not an error.
    let id = serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );

    let id = serve.send("set_grid_mode", &[("mode", Value::from("navigate"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );

    // An unknown mode is refused, and the refusal has to name the alternatives — an
    // agent that guessed "reorder" has no other way to find out what to say.
    let id = serve.send("set_grid_mode", &[("mode", Value::from("reorder"))]);
    let reply = serve.reply_to(id);
    assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(false));
    let error = reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert!(
        error.contains("navigate") && error.contains("reorganize") && error.contains("merge"),
        "the refusal did not name the modes: {error}"
    );

    // And the mode survives the panel closing, so reopening it cannot silently put a
    // click back to meaning something else.
    let id = serve.send("set_grid_mode", &[("mode", Value::from("reorganize"))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    let id = serve.send("set_thumbnails", &[("visible", Value::from(false))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        serve.snapshot().get("grid_mode").and_then(Value::as_str),
        Some("reorganize"),
        "closing the grid forgot the mode"
    );

    serve.quit();
}

#[test]
fn the_merge_tabs_two_viewports_render_without_a_staged_document() {
    // The regression M20 already set the bar for: opening a new mode must not break
    // anything that already worked, and here there is not yet a way to stage a
    // second file (`docs/goal-5-plan.md` M28) — so the proof available *now* is that
    // switching to the tab and capturing the window produces a real image rather
    // than a panic or a blank one. The drag out of the staging viewport itself needs
    // a person's hands, same as every other pointer gesture this panel has shipped
    // — see `crate::thumbnails`'s own module docs.
    let Some(_window) = e2e("the_merge_tabs_two_viewports_render_without_a_staged_document") else {
        return;
    };

    let document = fixture("e2e-merge-tab.pdf");
    let capture = scratch("e2e-merge-tab-capture.png");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    assert_eq!(
        serve.reply_to(id).get("ok").and_then(Value::as_bool),
        Some(true)
    );
    let id = serve.send("set_grid_mode", &[("mode", Value::from("merge"))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("changed")
    );
    assert_eq!(
        serve.snapshot().get("grid_mode").and_then(Value::as_str),
        Some("merge")
    );

    let id = serve.send(
        "capture",
        &[("path", Value::from(capture.to_string_lossy().as_ref()))],
    );
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("capturing")
    );
    serve.wait_for_event("captured");

    let bytes = std::fs::read(&capture).expect("the capture should exist on disk");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder.read_info().expect("should be a readable PNG");
    let info = reader.info();
    assert!(
        info.width > 100 && info.height > 100,
        "captured a {}x{} image while the merge tab was open",
        info.width,
        info.height
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
    let mut serve = Serve::start_empty();

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

    // Measured from the window and the zoom rather than from the scroll limit. Paged mode
    // confines the scroll range to one page, so `content_height - max_scroll` no longer
    // names the window's height — it did when this test was written, which is why the
    // derivation is now spelled out.
    let viewport_pt = window_height_pt(&serve.view());
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

#[test]
fn paged_mode_shows_one_page_where_free_mode_shows_two() {
    let Some(_window) = e2e("paged_mode_shows_one_page_where_free_mode_shows_two") else {
        return;
    };

    // The complaint that produced the mode: "even when paged is selected, it still scrolls
    // like a free scroll". Paged mode changed what PageDown meant and nothing else, so the
    // next page still showed below the current one and the wheel rolled straight past it.
    let document = fixture("e2e-paged-single.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");

    // Zoom until the window is taller than a 300 pt page, so free mode has two pages on
    // screen and there is something for paged mode to be different from.
    let id = serve.send(
        "set_zoom",
        &[("target", serde_json::json!({ "fixed": 2.3 }))],
    );
    serve.reply_to(id);
    let view = serve.view();
    assert!(
        window_height_pt(&view) > 300.0,
        "the window is not taller than a page: {view}"
    );
    assert!(
        page_field(&view, "last_visible_page") > page_field(&view, "first_visible_page"),
        "free mode should have two pages on screen here: {view}"
    );

    let id = serve.send("set_scroll_mode", &[("mode", Value::from("paged"))]);
    serve.reply_to(id);
    let view = serve.view();
    assert_eq!(
        (
            page_field(&view, "first_visible_page"),
            page_field(&view, "last_visible_page")
        ),
        (1, 1),
        "paged mode is still showing more than one page: {view}"
    );
    // And there is nowhere to scroll, because the page is the whole scrollable range.
    assert_eq!(
        view.get("min_scroll_pt").and_then(Value::as_f64),
        view.get("max_scroll_pt").and_then(Value::as_f64),
        "paged mode left somewhere to scroll on a page that fits: {view}"
    );

    serve.quit();
}

#[test]
fn scrolling_in_paged_mode_turns_the_page() {
    let Some(_window) = e2e("scrolling_in_paged_mode_turns_the_page") else {
        return;
    };

    // What the wheel and the arrow keys do, over the wire. Scrolling off the end of the
    // page is the only gesture a single-page view has left, so it has to mean the next
    // page — and the same command an agent sends is the one the wheel produces.
    let document = fixture("e2e-paged-scroll.pdf");
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    let id = serve.send("set_scroll_mode", &[("mode", Value::from("paged"))]);
    serve.reply_to(id);

    // At fit-width these 200 pt sheets zoom to about 5x, which makes the window cover
    // barely half of one — so this is the case with page left to read before there is
    // anything to turn to, and scrolling has to do both jobs in the right order.
    let view = serve.view();
    let window = window_height_pt(&view);
    assert!(
        window < 300.0,
        "the window is not shorter than a page, so this tests the wrong case: {view}"
    );

    let mut screenfuls = 0;
    let mut inside_the_page = Vec::new();
    while page_field(&serve.view(), "current_page") == 1 {
        inside_the_page.push(serve.view().get("scroll_top_pt").cloned());
        let id = serve.send("scroll_by_viewports", &[("fraction", Value::from(1.0))]);
        serve.reply_to(id);
        screenfuls += 1;
        assert!(screenfuls < 10, "never left page 1: {:?}", inside_the_page);
    }
    assert!(
        screenfuls > 1,
        "the page turned before the rest of it had been shown: {inside_the_page:?}"
    );
    let view = serve.view();
    assert_eq!(page_field(&view, "current_page"), 2);
    // The fixture's pages are 300 pt with 12 pt gaps, so page 2 starts at 312 — the top of
    // the page, not a screenful past wherever the last one ended.
    assert_eq!(
        view.get("scroll_top_pt").and_then(Value::as_f64),
        Some(312.0)
    );

    // Back off the top of page 2 returns to the bottom of page 1, so that reading
    // backwards retraces the way it came rather than jumping to a page's top.
    let id = serve.send("scroll_by_viewports", &[("fraction", Value::from(-1.0))]);
    serve.reply_to(id);
    let view = serve.view();
    assert_eq!(
        page_field(&view, "current_page"),
        1,
        "scrolling back did not turn the page: {view}"
    );
    // Within a tolerance, because the position makes a round trip through the shell's
    // pixel offset, which is an `f32`. A point is 1/72 inch and this lands within a
    // hundredth of one; `SCROLL_EPSILON_PT` exists for the same reason.
    let landed = view.get("scroll_top_pt").and_then(Value::as_f64);
    let bottom = view.get("max_scroll_pt").and_then(Value::as_f64);
    assert!(
        landed
            .zip(bottom)
            .is_some_and(|(landed, bottom)| (landed - bottom).abs() < 0.01),
        "did not land on the bottom of the previous page: {view}"
    );

    // And off the top of the first page stops rather than wrapping.
    let id = serve.send("first_page", &[]);
    serve.reply_to(id);
    let id = serve.send("scroll_by_viewports", &[("fraction", Value::from(-1.0))]);
    assert_eq!(
        serve.reply_to(id).get("outcome").and_then(Value::as_str),
        Some("unchanged")
    );
    let view = serve.view();
    assert_eq!(page_field(&view, "current_page"), 1);
    assert_eq!(view.get("scroll_top_pt").and_then(Value::as_f64), Some(0.0));

    serve.quit();
}

#[test]
fn the_page_grid_does_not_fight_the_page_column_over_textures() {
    let Some(_window) = e2e("the_page_grid_does_not_fight_the_page_column_over_textures") else {
        return;
    };

    // Reported as "pages 10, 11 and 12 flicker". The texture cache has two consumers, and
    // eviction ran inside the page column with only its own window to go on — so the grid
    // asked for a thumbnail, the render landed, the column evicted it for being far from the
    // viewport, and the grid asked again. Forever.
    //
    // A settled viewer does no work. That is the whole assertion, and it is why this needed
    // a document longer than the column's retain margin: on a short one the window covers
    // everything and there is nothing to fight over.
    let document = fixture_of("e2e-grid-thrash.pdf", 20);
    let mut serve = Serve::start(&document);
    serve.wait_for_event("idle");
    // Paged mode narrows the column's window to one page, which is what made three pages
    // rather than one fall outside it.
    let id = serve.send("set_scroll_mode", &[("mode", Value::from("paged"))]);
    serve.reply_to(id);
    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    serve.reply_to(id);

    // Let the first renders land. With the bug present this never comes true, so the bound
    // is the failure rather than a timeout hidden inside a helper.
    let mut settled = None;
    for _ in 0..400 {
        let state = serve.snapshot();
        if state.get("renders_in_flight").and_then(Value::as_u64) == Some(0) {
            settled = Some(state);
            break;
        }
    }
    let settled = settled.expect("the render pipeline never went quiet with the grid open");
    assert_eq!(
        settled.get("failed_pages").and_then(Value::as_array),
        Some(&vec![]),
        "a failed render would look like quiet without being it: {settled}"
    );
    let cached = settled.get("pages_cached").and_then(Value::as_u64);

    // And it stays quiet. One reading proves nothing about a flicker; the count holding
    // still while no work is submitted does.
    for read in 0..40 {
        let state = serve.snapshot();
        assert_eq!(
            state.get("renders_in_flight").and_then(Value::as_u64),
            Some(0),
            "read {read}: the viewer went back to work with nothing on screen changing: {state}"
        );
        assert_eq!(
            state.get("pages_cached").and_then(Value::as_u64),
            cached,
            "read {read}: a texture was evicted and re-made while both panels wanted it: {state}"
        );
    }

    serve.quit();
}

#[test]
fn the_page_grid_stops_asking_for_a_page_the_renderer_refuses() {
    let Some(_window) = e2e("the_page_grid_stops_asking_for_a_page_the_renderer_refuses") else {
        return;
    };

    // The grid submitted renders without the retry budget the page column uses, so a page
    // that cannot be rasterized was re-requested from the grid on every frame it was on
    // screen — a worker permanently busy producing an answer already known.
    //
    // One page with an absurd aspect ratio, which is the only way found to fail
    // deterministically: at 100 x 4,000,000 pt it comes to 64 x 2,593,679 px even at the tiny
    // zoom thumbnails use, past the backend's 65,535 px per-axis limit — and a refused size
    // is not the kind of failure worth retrying. The other pages are ordinary and must go on
    // rendering.
    //
    // Note that only the *grid* asks for it. Page 6 is outside the page column's prefetch
    // window at the top of the document, so this isolates the path being tested.
    let mut sizes = vec![(200_u32, 300_u32); 12];
    sizes[5] = (100, 4_000_000);
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push("e2e-grid-hopeless.pdf");
    std::fs::write(&path, pdf_with_page_sizes(&sizes)).expect("should write the fixture");

    let mut serve = Serve::start(&path);
    serve.wait_for_event("idle");
    let id = serve.send("set_thumbnails", &[("visible", Value::from(true))]);
    serve.reply_to(id);

    // Let the grid draw and the refusals land. Snapshots drive frames as well as reading
    // state, and every message read is counted on the way past.
    for _ in 0..120 {
        serve.snapshot();
    }
    let state = serve.snapshot();
    let failed = state
        .get("failed_pages")
        .and_then(Value::as_array)
        .expect("a failed page list");
    assert_eq!(
        failed,
        &vec![Value::from(6)],
        "the fixture did not produce exactly one unrenderable page, so this proves nothing: \
         {state}"
    );

    // Everything after here is about *not* asking again.
    let settled = serve.failures_seen();
    assert!(
        settled > 0 && settled < 10,
        "expected a handful of refusals, not {settled} — the budget is bounded per rung"
    );
    for _ in 0..200 {
        serve.snapshot();
    }
    assert_eq!(
        serve.failures_seen(),
        settled,
        "the grid went back to asking for a page the renderer had refused"
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
