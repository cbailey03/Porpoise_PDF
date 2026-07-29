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
//! headless `cargo test` does not report a failure it cannot help. CI sets it and
//! runs under `xvfb-run` on Linux.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use porpoise_testkit::multi_page_pdf;
use serde_json::Value;

/// How long to wait for any single expected message.
///
/// Generous: it covers window creation and GPU setup on a loaded CI runner, and a
/// test that waits too long is merely slow while one that waits too little is
/// flaky.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixture: enough pages that navigating to page 4 means something.
const PAGES: usize = 6;

/// The viewer's gap between pages, in points.
///
/// Duplicated from `viewer.rs` because this is an integration test and that
/// constant is crate-private. Only used to predict a scroll offset; if the two ever
/// disagree, the assertion that uses it fails loudly rather than drifting.
const PAGE_GAP_PT: f64 = 12.0;

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
    fn quit(mut self) {
        self.send_raw(r#"{"command":"quit"}"#);
        let status = self.child.wait().expect("should exit");
        assert!(status.success(), "exited with {status}");
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
    // why the offset is asserted too.
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
