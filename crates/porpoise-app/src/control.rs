//! The control channel: commands in on stdin, replies and events out on stdout.
//!
//! # Why stdio and not a socket
//!
//! This is a security decision, not a convenience one. A bound socket is reachable
//! by every process on the machine and would need an authentication story we have
//! no reason to write. stdio is a pipe handed to us by a parent that already holds
//! our privileges — a different risk class entirely.
//!
//! It is worth being plain about what this channel grants, since the whole point of
//! Goal 2 is to grant it: the controlling process can open any file the user can
//! read, see it rendered, and write a PNG anywhere the user can write. That is a
//! file-read and screen-read capability. It is not privilege escalation — the
//! controller already runs as the user — but it is meaningfully more than "a
//! viewer", which is why it exists only when `porpoise serve` is invoked and never
//! by default.
//!
//! See `docs/goal-2-plan.md`, section 5.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use serde::Serialize;

use crate::protocol::{DecodeError, MAX_LINE_BYTES, Request, decode};

/// Requests handled per frame.
///
/// A client that floods the channel must not be able to stall a frame, so the
/// backlog is drained at a bounded rate rather than exhaustively.
const MAX_REQUESTS_PER_FRAME: usize = 64;

/// One line's worth of input: a request, or why it could not be read.
type Incoming = Result<Request, DecodeError>;

/// The reader half plus the writer, owned by the viewer.
pub(crate) struct Control {
    incoming: Receiver<Incoming>,
    out: Box<dyn Write + Send>,
    /// Set once stdin closes, meaning the controlling process has gone.
    hung_up: bool,
}

impl Control {
    /// Reads commands from stdin and writes replies to stdout.
    pub(crate) fn stdio() -> Self {
        Self::new(
            Box::new(BufReader::new(std::io::stdin())),
            Box::new(std::io::stdout()),
        )
    }

    /// The general form, so tests can drive the loop over pipes in memory.
    pub(crate) fn new(input: Box<dyn BufRead + Send>, out: Box<dyn Write + Send>) -> Self {
        let (sender, incoming) = mpsc::channel();

        // A dedicated thread because reading stdin blocks, and the UI thread cannot
        // afford to. Ends when stdin closes, which drops the sender and is how the
        // viewer learns the controller is gone.
        let spawned = std::thread::Builder::new()
            .name("porpoise-control".to_owned())
            .spawn(move || read_lines(input, &sender));
        if let Err(error) = spawned {
            tracing::error!(%error, "could not start the control reader");
        }

        Self {
            incoming,
            out,
            hung_up: false,
        }
    }

    /// Takes up to [`MAX_REQUESTS_PER_FRAME`] pending lines. Never blocks.
    pub(crate) fn poll(&mut self) -> Vec<Incoming> {
        let mut batch = Vec::new();
        for _ in 0..MAX_REQUESTS_PER_FRAME {
            match self.incoming.try_recv() {
                Ok(incoming) => batch.push(incoming),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.hung_up = true;
                    break;
                }
            }
        }
        batch
    }

    /// Whether stdin has closed. The viewer treats this as a request to exit, the
    /// way every other stdio protocol does.
    pub(crate) fn hung_up(&self) -> bool {
        self.hung_up
    }

    /// Writes one message, followed by a newline, and flushes.
    ///
    /// Flushed per message on purpose: a client blocked waiting for an `idle` event
    /// that is sitting in our buffer would deadlock.
    pub(crate) fn send(&mut self, message: &impl Serialize) {
        match serde_json::to_string(message) {
            Ok(line) => {
                // A write failure means the client closed the pipe. Nothing useful
                // to do about it, and `hung_up` will notice on the read side.
                if let Err(error) = writeln!(self.out, "{line}").and_then(|()| self.out.flush()) {
                    tracing::debug!(%error, "control channel write failed");
                }
            }
            // Serializing our own types cannot fail in practice, but panicking on
            // the UI thread over a diagnostic would be a poor trade.
            Err(error) => tracing::error!(%error, "could not serialize a control message"),
        }
    }
}

/// What one capped read produced.
enum Line {
    /// A complete line, in the buffer.
    Read,
    /// The line exceeded [`MAX_LINE_BYTES`] and has been skipped to the next
    /// newline, so the stream stays usable.
    TooLong,
    /// The stream ended.
    Eof,
}

/// Reads one line, refusing to buffer more than [`MAX_LINE_BYTES`].
///
/// `BufRead::read_line` would grow without bound on a client that never sends a
/// newline, which is an allocation attack on a channel whose input is not ours to
/// trust. `Read::take` cannot help here — it requires `Sized` and this is a trait
/// object — so the cap is applied by hand over `fill_buf`.
///
/// An over-long line is *skipped* rather than fatal. One bad message should cost
/// the client that message, not the whole session.
fn read_capped(input: &mut dyn BufRead, out: &mut Vec<u8>) -> std::io::Result<Line> {
    out.clear();
    let mut overflowed = false;

    loop {
        let available = match input.fill_buf() {
            Ok(buf) => buf,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };

        if available.is_empty() {
            return Ok(if overflowed {
                Line::TooLong
            } else if out.is_empty() {
                Line::Eof
            } else {
                // A final line with no trailing newline is still a line.
                Line::Read
            });
        }

        if let Some(index) = available.iter().position(|byte| *byte == b'\n') {
            if !overflowed {
                out.extend_from_slice(available.get(..index).unwrap_or_default());
            }
            input.consume(index + 1);
            return Ok(if overflowed {
                Line::TooLong
            } else {
                Line::Read
            });
        }

        let consumed = available.len();
        if !overflowed {
            out.extend_from_slice(available);
            if out.len() > MAX_LINE_BYTES {
                // Stop accumulating, but keep reading to find the newline so the
                // next message is not parsed as this one's tail.
                overflowed = true;
                out.clear();
            }
        }
        input.consume(consumed);
    }
}

/// Reads lines until stdin closes, decoding each one.
fn read_lines(mut input: Box<dyn BufRead + Send>, sender: &mpsc::Sender<Incoming>) {
    let mut buffer = Vec::new();
    loop {
        let outcome = match read_capped(&mut *input, &mut buffer) {
            Ok(outcome) => outcome,
            Err(error) => {
                // A broken pipe. There is no way to resynchronize a stream we
                // cannot read, so stop; dropping the sender reports the hangup.
                tracing::warn!(%error, "control channel read failed");
                return;
            }
        };

        let incoming = match outcome {
            // EOF: the controller has gone. Dropping the sender tells the viewer.
            Line::Eof => return,
            Line::TooLong => Err(DecodeError::TooLong),
            Line::Read => match std::str::from_utf8(&buffer) {
                Ok(line) => match decode(line) {
                    // A blank line is not a request and needs no reply.
                    Ok(None) => continue,
                    Ok(Some(request)) => Ok(request),
                    Err(error) => Err(error),
                },
                Err(error) => Err(DecodeError::NotUtf8 {
                    detail: error.to_string(),
                }),
            },
        };

        if sender.send(incoming).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::protocol::RequestBody;
    use porpoise_view::ViewCommand;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().map_or(Ok(0), |mut guard| {
                guard.extend_from_slice(buf);
                Ok(buf.len())
            })
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            self.0
                .lock()
                .map(|guard| String::from_utf8_lossy(&guard).into_owned())
                .unwrap_or_default()
        }
    }

    fn control(input: &str) -> (Control, Shared) {
        let out = Shared::default();
        let control = Control::new(
            Box::new(Cursor::new(input.to_owned().into_bytes())),
            Box::new(out.clone()),
        );
        (control, out)
    }

    /// Polls until `count` items arrive or the deadline passes.
    fn collect(control: &mut Control, count: usize) -> Vec<Incoming> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut all = Vec::new();
        while all.len() < count && Instant::now() < deadline {
            all.extend(control.poll());
            if all.len() < count {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        all
    }

    #[test]
    fn several_commands_arrive_in_order() {
        let (mut control, _out) = control(
            "{\"id\":1,\"command\":\"next_page\"}\n\
             {\"id\":2,\"command\":\"first_page\"}\n",
        );
        let batch = collect(&mut control, 2);
        assert_eq!(batch.len(), 2);

        let ids: Vec<Option<u64>> = batch
            .iter()
            .map(|item| item.as_ref().map_or(None, |request| request.id))
            .collect();
        assert_eq!(ids, vec![Some(1), Some(2)], "requests arrived out of order");
    }

    #[test]
    fn a_malformed_line_is_reported_without_stopping_the_stream() {
        // The important property: one bad message must not desynchronize the
        // channel or kill the reader. An agent will send bad JSON eventually.
        let (mut control, _out) = control(
            "{\"command\":\"nonsense\"}\n\
             not json at all\n\
             {\"id\":9,\"command\":\"last_page\"}\n",
        );
        let batch = collect(&mut control, 3);
        assert_eq!(batch.len(), 3);
        assert!(batch[0].is_err(), "unknown command was accepted");
        assert!(batch[1].is_err(), "bare text was accepted");

        let recovered = batch[2].as_ref().expect("the stream should recover");
        assert_eq!(recovered.id, Some(9));
        assert_eq!(
            recovered.body,
            RequestBody::Command(Command::View(ViewCommand::LastPage))
        );
    }

    #[test]
    fn blank_lines_produce_nothing_at_all() {
        let (mut control, _out) = control("\n\n   \n{\"command\":\"quit\"}\n");
        let batch = collect(&mut control, 1);
        assert_eq!(batch.len(), 1, "blank lines produced messages");
        assert_eq!(
            batch[0].as_ref().expect("a request").body,
            RequestBody::Command(Command::Quit)
        );
    }

    #[test]
    fn closing_stdin_is_reported_as_a_hangup() {
        let (mut control, _out) = control("{\"command\":\"next_page\"}\n");
        collect(&mut control, 1);

        // Poll until the reader thread has finished and dropped its sender.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !control.hung_up() && Instant::now() < deadline {
            control.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(control.hung_up(), "EOF was not reported");
    }

    #[test]
    fn a_line_with_no_newline_terminator_is_still_read() {
        // A client that writes its last command without a trailing newline and then
        // closes should still have it acted on.
        let (mut control, _out) = control("{\"command\":\"quit\"}");
        let batch = collect(&mut control, 1);
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn an_unterminated_flood_cannot_buffer_without_bound() {
        // No newline, ever. The read cap is what stops this becoming an allocation
        // attack; without it `read_line` would grow until memory ran out.
        let flood = "x".repeat(MAX_LINE_BYTES * 4);
        let (mut control, _out) = control(&flood);
        let batch = collect(&mut control, 1);
        assert_eq!(batch.len(), 1);
        assert!(batch[0].is_err(), "a flood decoded as a valid request");
    }

    #[test]
    fn messages_are_written_one_per_line_and_flushed() {
        let (mut control, out) = control("");
        control.send(&crate::protocol::Event::Idle);
        control.send(&crate::protocol::Event::PageRendered { page: 3 });

        let text = out.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "framing is wrong: {text:?}");
        assert_eq!(lines[0], r#"{"event":"idle"}"#);
        assert_eq!(lines[1], r#"{"event":"page_rendered","page":3}"#);
    }

    #[test]
    fn polling_is_bounded_so_a_flood_cannot_stall_a_frame() {
        let mut input = String::new();
        for id in 0..(MAX_REQUESTS_PER_FRAME * 3) {
            input.push_str(&format!("{{\"id\":{id},\"command\":\"next_page\"}}\n"));
        }
        let (mut control, _out) = control(&input);

        // Give the reader time to queue more than one frame's worth.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut first = Vec::new();
        while first.is_empty() && Instant::now() < deadline {
            first = control.poll();
        }
        assert!(
            first.len() <= MAX_REQUESTS_PER_FRAME,
            "one poll returned {} requests",
            first.len()
        );
    }
}
