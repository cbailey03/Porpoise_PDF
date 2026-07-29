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

/// Whether the environment can open a window.
fn e2e_enabled() -> bool {
    std::env::var_os("PORPOISE_E2E").is_some()
}

/// Announces a skip rather than passing quietly.
///
/// A test that silently does nothing reads as coverage it is not providing.
fn skip(name: &str) {
    eprintln!("SKIPPED {name}: set PORPOISE_E2E=1 (needs a display) to run it");
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

    /// Asks for a snapshot and returns its `view` object.
    fn view(&mut self) -> Value {
        let id = self.send("snapshot", &[]);
        let reply = self.reply_to(id);
        assert_eq!(reply.get("ok").and_then(Value::as_bool), Some(true));
        reply
            .get("snapshot")
            .and_then(|snapshot| snapshot.get("view"))
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
    if !e2e_enabled() {
        skip("an_agent_can_open_navigate_and_capture");
        return;
    }

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
fn an_agent_can_open_a_document_that_was_not_on_the_command_line() {
    if !e2e_enabled() {
        skip("an_agent_can_open_a_document_that_was_not_on_the_command_line");
        return;
    }

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
fn a_malformed_message_is_refused_without_killing_the_session() {
    if !e2e_enabled() {
        skip("a_malformed_message_is_refused_without_killing_the_session");
        return;
    }

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
    if !e2e_enabled() {
        skip("a_refused_command_explains_itself_and_changes_nothing");
        return;
    }

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
        Some(0),
        "a refused command moved the view"
    );

    serve.quit();
}

#[test]
fn closing_stdin_shuts_the_program_down() {
    if !e2e_enabled() {
        skip("closing_stdin_shuts_the_program_down");
        return;
    }

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
