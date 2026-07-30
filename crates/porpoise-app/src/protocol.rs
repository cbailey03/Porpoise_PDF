//! The control protocol: newline-delimited JSON, one object per line.
//!
//! See `docs/goal-2-plan.md`, sections 3 and 4. The shape is deliberately small:
//!
//! ```text
//! in   {"id":1,"command":"go_to_page","page":4}
//! out  {"id":1,"ok":true,"outcome":"changed"}
//! out  {"event":"page_rendered","page":4}
//! out  {"event":"idle"}
//! ```
//!
//! `id` is optional and echoed back, so a client can correlate replies without a
//! full JSON-RPC envelope. Events arrive unsolicited and carry no `id`.
//!
//! Every page number on this wire counts from 1, matching the CLI and what a person
//! reads in the status bar. They are [`PageNumber`], not `usize`, so `{"page":0}` is
//! refused by deserialization rather than by a check somebody has to remember.
//!
//! # Why this is decoded by hand
//!
//! A derived `Deserialize` on [`Command`] would nest the view commands one level
//! deep, and `#[serde(untagged)]` would flatten them at the cost of turning every
//! failure into "data did not match any variant". An agent needs to be told which
//! command it got wrong and what the valid ones are — a protocol whose errors are
//! unhelpful gets worked around by guessing, which is what we are trying to avoid.
//!
//! # This is an untrusted-input surface
//!
//! Everything arriving here is outside data, exactly like a PDF, and is treated the
//! same way: a malformed message produces an error, never a panic and never a hang.
//! `tests/control.rs` fuzzes this decoder for the same reason the PDF parser is
//! fuzzed. A viewer that survives 4,000 malformed PDFs and then dies on a stray
//! brace has moved the hole, not closed it.

use std::path::PathBuf;

use porpoise_view::{PageNumber, Rejection, ViewCommand, ViewSnapshot};
use serde::Serialize;

use crate::command::Command;
use crate::confirm::Answer;

/// Longest line we will accept, so a client cannot exhaust memory by never
/// sending a newline.
pub(crate) const MAX_LINE_BYTES: usize = 1 << 20;

/// A decoded message from the controlling process.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    /// Echoed back on the reply, if the client supplied one.
    pub(crate) id: Option<u64>,
    /// What was asked for.
    pub(crate) body: RequestBody,
}

/// What a request wants.
///
/// Commands change state; queries only read it. Keeping them apart means a client
/// can poll without any risk of moving the view.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RequestBody {
    /// Carry out a command.
    Command(Command),
    /// Report the current state.
    Snapshot,
    /// List every command this build understands.
    Commands,
}

/// Why a line could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("line exceeds the {MAX_LINE_BYTES} byte limit")]
    TooLong,
    #[error("not valid UTF-8: {detail}")]
    NotUtf8 {
        /// Where the encoding broke.
        detail: String,
    },
    #[error("not valid JSON: {detail}")]
    NotJson {
        /// serde_json's own complaint.
        detail: String,
    },
    #[error("expected a JSON object")]
    NotAnObject,
    #[error("missing the \"command\" field")]
    MissingCommand,
    #[error("\"command\" must be a string")]
    CommandNotAString,
    #[error("\"id\" must be a non-negative integer")]
    BadId,
    #[error("unknown command \"{name}\"; valid commands are: {valid}")]
    UnknownCommand {
        /// What the client sent.
        name: String,
        /// Every command this build accepts, comma-separated.
        valid: String,
    },
    #[error("\"{command}\" has bad arguments: {detail}")]
    BadArguments {
        /// The command that was recognised.
        command: String,
        /// What was wrong with its fields.
        detail: String,
    },
}

impl DecodeError {
    /// Attaches the request id this failure belongs to, if one was readable.
    fn at(self, id: Option<u64>) -> DecodeFailure {
        DecodeFailure { id, reason: self }
    }
}

/// A line that could not be decoded, and the id it was sent under.
///
/// The id matters more than it looks. A bad *argument* on a well-formed line —
/// `{"id":7,"command":"go_to_page","page":0}` — has a perfectly good id, and
/// discarding it hands the client an error it cannot match to any request. A client
/// waiting for a reply to id 7 then waits forever.
///
/// That is not hypothetical: the first end-to-end test to send a bad argument under
/// an id hung with the window open until it was closed by hand. So the id travels
/// with the failure whenever the line got far enough to have one, and is `None` only
/// when it genuinely could not be read — a line that is not JSON, not an object, or
/// whose `id` was itself malformed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub(crate) struct DecodeFailure {
    /// The request this failure answers, when the line got far enough to say.
    pub(crate) id: Option<u64>,
    /// What was wrong.
    pub(crate) reason: DecodeError,
}

/// Decodes one line.
///
/// Blank lines are not an error; they decode to `None` so a client can send them
/// harmlessly.
pub(crate) fn decode(line: &str) -> Result<Option<Request>, DecodeFailure> {
    if line.len() > MAX_LINE_BYTES {
        return Err(DecodeError::TooLong.at(None));
    }
    // A leading UTF-8 BOM is stripped rather than refused. JSON permits ignoring
    // it, and plenty of tooling — PowerShell's pipe among them — emits one without
    // being asked. Found the first time this protocol was driven by hand: the very
    // first command failed with "expected value at line 1 column 1", which is a
    // baffling error to hand somebody whose message was perfectly well formed.
    let trimmed = line.trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|error| {
        DecodeError::NotJson {
            detail: error.to_string(),
        }
        .at(None)
    })?;
    let object = value.as_object().ok_or(DecodeError::NotAnObject.at(None))?;

    // Read the id before anything that can fail with one, so every later failure can
    // be correlated by the client. See [`DecodeFailure`].
    let id = match object.get("id") {
        None | Some(serde_json::Value::Null) => None,
        Some(raw) => Some(raw.as_u64().ok_or(DecodeError::BadId.at(None))?),
    };

    let name = object
        .get("command")
        .ok_or(DecodeError::MissingCommand.at(id))?
        .as_str()
        .ok_or(DecodeError::CommandNotAString.at(id))?;

    let path_argument = |field: &str| -> Result<PathBuf, DecodeFailure> {
        object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| {
                DecodeError::BadArguments {
                    command: name.to_owned(),
                    detail: format!("expected a string \"{field}\""),
                }
                .at(id)
            })
    };

    // Page numbers count from 1, and `PageNumber` cannot hold zero, so `{"page":0}`
    // and `{"page":-1}` are refused here rather than by a bounds check further in.
    let page_argument = |field: &str| -> Result<PageNumber, DecodeFailure> {
        let complain = |detail: String| {
            DecodeError::BadArguments {
                command: name.to_owned(),
                detail,
            }
            .at(id)
        };
        let raw = object
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| complain(format!("expected a positive integer \"{field}\"")))?;
        let number =
            usize::try_from(raw).map_err(|_| complain(format!("\"{field}\" is too big")))?;
        PageNumber::new(number).ok_or_else(|| complain("page numbers start at 1".to_owned()))
    };

    let body = match name {
        "snapshot" => RequestBody::Snapshot,
        "commands" => RequestBody::Commands,
        "open" => RequestBody::Command(Command::Open {
            path: path_argument("path")?,
        }),
        "capture" => RequestBody::Command(Command::Capture {
            path: path_argument("path")?,
        }),
        "close" => RequestBody::Command(Command::Close),
        "move_page" => RequestBody::Command(Command::MovePage {
            from: page_argument("from")?,
            to: page_argument("to")?,
        }),
        "delete_page" => RequestBody::Command(Command::DeletePage {
            page: page_argument("page")?,
        }),
        "undo" => RequestBody::Command(Command::Undo),
        "save" => RequestBody::Command(Command::Save),
        "save_as" => RequestBody::Command(Command::SaveAs {
            path: path_argument("path")?,
        }),
        "set_thumbnails" => RequestBody::Command(Command::SetThumbnails {
            visible: object
                .get("visible")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| {
                    DecodeError::BadArguments {
                        command: name.to_owned(),
                        detail: "expected a boolean \"visible\"".to_owned(),
                    }
                    .at(id)
                })?,
        }),
        "answer" => RequestBody::Command(Command::Answer {
            choice: object
                .get("choice")
                .and_then(|raw| serde_json::from_value::<Answer>(raw.clone()).ok())
                .ok_or_else(|| {
                    DecodeError::BadArguments {
                        command: name.to_owned(),
                        // Names the alternatives, because an agent that guessed
                        // "yes" has no other way to find out what to say.
                        detail: format!(
                            "expected \"choice\" to be one of: {}",
                            Answer::ALL.join(", ")
                        ),
                    }
                    .at(id)
                })?,
        }),
        "quit" => RequestBody::Command(Command::Quit),
        other => {
            // A view command is internally tagged on the same `command` field, so
            // the whole object decodes directly. Only reached once the shell names
            // above are ruled out, which keeps their errors specific.
            if !ViewCommand::ALL.iter().any(|known| known.name() == other) {
                return Err(DecodeError::UnknownCommand {
                    name: other.to_owned(),
                    valid: Command::all_names().join(", "),
                }
                .at(id));
            }
            let view: ViewCommand = serde_json::from_value(value.clone()).map_err(|error| {
                DecodeError::BadArguments {
                    command: other.to_owned(),
                    detail: error.to_string(),
                }
                .at(id)
            })?;
            RequestBody::Command(Command::View(view))
        }
    };

    Ok(Some(Request { id, body }))
}

/// Everything readable about the program's state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Snapshot {
    /// Path of the open document, if any.
    pub(crate) document: Option<String>,
    /// The view itself.
    pub(crate) view: ViewSnapshot,
    /// Page textures held.
    pub(crate) pages_cached: usize,
    /// Bytes those textures occupy.
    pub(crate) cache_bytes: usize,
    /// Renders submitted and not yet returned.
    pub(crate) renders_in_flight: usize,
    /// Pages we have stopped trying to render, counting from 1.
    pub(crate) failed_pages: Vec<PageNumber>,
    /// Why the last command failed, or `None` if nothing has.
    ///
    /// The same string the status bar shows, so an agent reads what a person sees.
    /// Cleared by the next successful open or close.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    /// Whether the page grid is showing.
    pub(crate) thumbnails: bool,
    /// Whether the page order differs from the file on disk.
    ///
    /// False again once a save of that exact order reports success — so a document that
    /// has just been written does not go on claiming changes.
    pub(crate) unsaved_changes: bool,
    /// What is waiting on an `answer` command, because carrying it out would discard
    /// unsaved page changes. `None` when nothing is waiting.
    ///
    /// The same sentence a person is reading in the question box. A client that gets
    /// `needs_answer` back from `quit`, `close` or `open` finds out here what it is being
    /// asked about, then replies with `answer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) awaiting_answer: Option<String>,
    /// Whether there is a page edit to undo.
    pub(crate) can_undo: bool,
    /// Where a save in flight is going, if one is running.
    ///
    /// A save takes about a second on a 400-page document, so a client needs to be
    /// able to tell "not saved yet" from "saving right now".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) saving_to: Option<String>,
    /// Nothing queued, nothing in flight, everything visible is drawn.
    ///
    /// The most useful field here. An agent that scrolls and then captures without
    /// waiting for this gets placeholder tiles; without an idle signal its only
    /// option is to sleep and hope, which is the root of most flaky automation.
    pub(crate) idle: bool,
}

/// Something that happened, reported without being asked.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    /// A document was opened.
    DocumentOpened {
        /// Its path.
        path: String,
        /// How many pages it has.
        page_count: usize,
    },
    /// The open document was closed.
    DocumentClosed,
    /// The view changed. Coalesced to at most one per frame.
    ViewChanged {
        /// The new state.
        snapshot: Box<Snapshot>,
    },
    /// A page finished rasterizing and is on screen.
    PageRendered {
        /// The page, counting from 1.
        page: PageNumber,
    },
    /// A page could not be rasterized.
    PageFailed {
        /// The page, counting from 1.
        page: PageNumber,
        /// The renderer's message.
        reason: String,
        /// Whether another attempt is coming.
        will_retry: bool,
    },
    /// The page order changed.
    PagesReordered {
        /// How many pages the document now shows.
        page_count: usize,
    },
    /// The document was written out.
    Saved {
        /// Where it went.
        path: String,
    },
    /// A save could not be completed. The original is untouched.
    SaveFailed {
        /// Why.
        error: String,
    },
    /// A capture was written.
    Captured {
        /// Where it went.
        path: String,
    },
    /// A capture failed.
    CaptureFailed {
        /// Why.
        error: String,
    },
    /// The render pipeline has settled.
    Idle,
}

/// A reply to a request.
///
/// Absent fields are omitted rather than sent as null, so a successful command is
/// three keys and an agent does not have to filter noise.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Reply {
    /// Echoed from the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<u64>,
    /// Whether the request was carried out.
    pub(crate) ok: bool,
    /// `changed`, `unchanged`, `opened`, `closed`, `quitting`, `capturing`, `edited`,
    /// `saving`, `needs_answer`, or `cancelled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<&'static str>,
    /// Why it was refused, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// The answer to a `snapshot` query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot: Option<Box<Snapshot>>,
    /// The answer to a `commands` query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) commands: Option<Vec<&'static str>>,
}

impl Reply {
    /// A bare success.
    pub(crate) fn ok(id: Option<u64>, outcome: &'static str) -> Self {
        Self {
            id,
            ok: true,
            outcome: Some(outcome),
            error: None,
            snapshot: None,
            commands: None,
        }
    }

    /// A failure carrying a human-readable reason.
    pub(crate) fn failed(id: Option<u64>, error: impl std::fmt::Display) -> Self {
        Self {
            id,
            ok: false,
            outcome: None,
            error: Some(error.to_string()),
            snapshot: None,
            commands: None,
        }
    }

    /// A refusal from the view layer.
    pub(crate) fn rejected(id: Option<u64>, rejection: Rejection) -> Self {
        Self::failed(id, rejection)
    }

    /// The answer to a `snapshot` query.
    pub(crate) fn with_snapshot(id: Option<u64>, snapshot: Snapshot) -> Self {
        Self {
            id,
            ok: true,
            outcome: None,
            error: None,
            snapshot: Some(Box::new(snapshot)),
            commands: None,
        }
    }

    /// The answer to a `commands` query.
    pub(crate) fn with_commands(id: Option<u64>) -> Self {
        Self {
            id,
            ok: true,
            outcome: None,
            error: None,
            snapshot: None,
            commands: Some(Command::all_names()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use porpoise_view::{ScrollMode, ZoomTarget};

    fn decoded(line: &str) -> Request {
        decode(line)
            .unwrap_or_else(|error| panic!("{line} failed to decode: {error}"))
            .unwrap_or_else(|| panic!("{line} decoded to nothing"))
    }

    fn command(line: &str) -> Command {
        match decoded(line).body {
            RequestBody::Command(command) => command,
            other => panic!("{line} decoded to {other:?}, not a command"),
        }
    }

    // --- Well-formed input ---------------------------------------------------

    #[test]
    fn a_view_command_decodes_flat_without_a_wrapper() {
        // The wrapper must be invisible on the wire. If this ever nests, every
        // client breaks.
        assert_eq!(
            command(r#"{"command":"next_page"}"#),
            Command::View(ViewCommand::NextPage)
        );
        assert_eq!(
            command(r#"{"command":"go_to_page","page":4}"#),
            Command::View(ViewCommand::GoToPage {
                page: PageNumber::new(4).expect("page 4 exists")
            })
        );
    }

    #[test]
    fn page_zero_is_refused_without_a_hand_written_check() {
        // `PageNumber` cannot hold zero, so deserialization refuses it. Worth a test
        // because the guarantee lives in a type rather than in code anybody reading
        // `decode` would see.
        let failure =
            decode(r#"{"id":7,"command":"go_to_page","page":0}"#).expect_err("should be refused");
        match failure.reason {
            DecodeError::BadArguments { command, .. } => assert_eq!(command, "go_to_page"),
            other => panic!("expected BadArguments, got {other:?}"),
        }
        assert_eq!(
            failure.id,
            Some(7),
            "a bad argument must still be answerable, or the client waits forever"
        );
    }

    #[test]
    fn a_failure_keeps_the_id_whenever_the_line_had_one() {
        // The bug this pins down: every failure below used to reply with no id, so a
        // client waiting on its request never saw an answer. Found when an end-to-end
        // test sent `page: 0` under an id and hung with the window open.
        for line in [
            r#"{"id":3,"command":"go_to_page","page":0}"#,
            r#"{"id":3,"command":"teleport"}"#,
            r#"{"id":3,"command":"open"}"#,
            r#"{"id":3}"#,
            r#"{"id":3,"command":5}"#,
        ] {
            let failure = decode(line).expect_err("should be refused");
            assert_eq!(failure.id, Some(3), "{line} lost its id");
        }
    }

    #[test]
    fn a_failure_has_no_id_when_none_could_be_read() {
        // The honest half: claiming an id here would be inventing one.
        for line in [
            "not json at all",
            "[1,2,3]",
            r#"{"id":-1,"command":"next_page"}"#,
        ] {
            let failure = decode(line).expect_err("should be refused");
            assert_eq!(failure.id, None, "{line} reported an id it never had");
        }
    }

    #[test]
    fn a_negative_page_is_refused() {
        let failure =
            decode(r#"{"command":"go_to_page","page":-1}"#).expect_err("should be refused");
        assert!(
            matches!(failure.reason, DecodeError::BadArguments { .. }),
            "{failure:?}"
        );
    }

    #[test]
    fn every_published_view_command_can_be_decoded_from_its_own_wire_form() {
        // Closes the loop between the command reference and the decoder: anything
        // `commands` advertises can actually be sent.
        for known in ViewCommand::ALL {
            let json = serde_json::to_string(known).expect("should serialize");
            assert_eq!(
                command(&json),
                Command::View(*known),
                "{} did not survive its own wire form",
                known.name()
            );
        }
    }

    #[test]
    fn shell_commands_decode_with_their_arguments() {
        assert_eq!(
            command(r#"{"command":"open","path":"a/b.pdf"}"#),
            Command::Open {
                path: PathBuf::from("a/b.pdf")
            }
        );
        assert_eq!(
            command(r#"{"command":"capture","path":"out.png"}"#),
            Command::Capture {
                path: PathBuf::from("out.png")
            }
        );
        assert_eq!(command(r#"{"command":"close"}"#), Command::Close);
        assert_eq!(command(r#"{"command":"quit"}"#), Command::Quit);
    }

    #[test]
    fn an_answer_decodes_each_of_its_three_choices() {
        for (wire, expected) in [
            ("save", Answer::Save),
            ("discard", Answer::Discard),
            ("cancel", Answer::Cancel),
        ] {
            assert_eq!(
                command(&format!(r#"{{"command":"answer","choice":"{wire}"}}"#)),
                Command::Answer { choice: expected }
            );
        }
    }

    #[test]
    fn an_answer_that_means_nothing_lists_the_ones_that_do() {
        // An agent that guessed "yes" has no other way to find out what to say, and
        // this is the command that gets it *out* of a question — a bad error here is
        // the difference between recovering and being stuck.
        let error =
            decode(r#"{"id":2,"command":"answer","choice":"yes"}"#).expect_err("should be refused");
        assert_eq!(
            error.id,
            Some(2),
            "lost the id, so the client cannot match it"
        );
        let message = error.to_string();
        for choice in Answer::ALL {
            assert!(message.contains(choice), "{choice} missing from: {message}");
        }
    }

    #[test]
    fn an_answer_with_no_choice_is_refused() {
        for line in [
            r#"{"command":"answer"}"#,
            r#"{"command":"answer","choice":null}"#,
            r#"{"command":"answer","choice":7}"#,
        ] {
            assert!(decode(line).is_err(), "{line} was accepted");
        }
    }

    #[test]
    fn queries_are_not_commands() {
        assert_eq!(
            decoded(r#"{"command":"snapshot"}"#).body,
            RequestBody::Snapshot
        );
        assert_eq!(
            decoded(r#"{"command":"commands"}"#).body,
            RequestBody::Commands
        );
    }

    #[test]
    fn an_id_is_optional_and_preserved() {
        assert_eq!(decoded(r#"{"command":"next_page"}"#).id, None);
        assert_eq!(decoded(r#"{"id":7,"command":"next_page"}"#).id, Some(7));
        assert_eq!(decoded(r#"{"id":null,"command":"next_page"}"#).id, None);
    }

    #[test]
    fn nested_enum_arguments_decode() {
        assert_eq!(
            command(r#"{"command":"set_zoom","target":"fit_page"}"#),
            Command::View(ViewCommand::SetZoom {
                target: ZoomTarget::FitPage
            })
        );
        assert_eq!(
            command(r#"{"command":"set_zoom","target":{"fixed":1.5}}"#),
            Command::View(ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(1.5)
            })
        );
        assert_eq!(
            command(r#"{"command":"set_scroll_mode","mode":"paged"}"#),
            Command::View(ViewCommand::SetScrollMode {
                mode: ScrollMode::Paged
            })
        );
    }

    #[test]
    fn a_blank_line_is_ignored_rather_than_rejected() {
        for line in ["", "   ", "\t", "\n"] {
            assert_eq!(decode(line), Ok(None), "{line:?} should be ignored");
        }
    }

    #[test]
    fn a_leading_byte_order_mark_is_ignored() {
        // PowerShell's pipe adds one. Refusing it means the first command a Windows
        // client sends fails for a reason that has nothing to do with the command.
        assert_eq!(
            command("\u{feff}{\"command\":\"next_page\"}"),
            Command::View(ViewCommand::NextPage)
        );
        // And a BOM on its own is just an empty line.
        assert_eq!(decode("\u{feff}"), Ok(None));
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(
            command("  {\"command\":\"first_page\"}\t"),
            Command::View(ViewCommand::FirstPage)
        );
    }

    // --- Malformed input -----------------------------------------------------

    #[test]
    fn an_unknown_command_lists_the_valid_ones() {
        // An agent that guesses wrong should be able to correct itself from the
        // error rather than by reading our source.
        let error = decode(r#"{"command":"teleport"}"#)
            .expect_err("should be refused")
            .reason;
        match error {
            DecodeError::UnknownCommand { name, valid } => {
                assert_eq!(name, "teleport");
                assert!(valid.contains("next_page"), "valid list was: {valid}");
                assert!(valid.contains("open"), "valid list was: {valid}");
            }
            other => panic!("expected UnknownCommand, got {other:?}"),
        }
    }

    #[test]
    fn a_recognised_command_with_bad_arguments_says_which_command() {
        let error = decode(r#"{"command":"go_to_page","page":"four"}"#)
            .expect_err("should be refused")
            .reason;
        match error {
            DecodeError::BadArguments { command, .. } => assert_eq!(command, "go_to_page"),
            other => panic!("expected BadArguments, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_path_argument_names_the_field() {
        let error = decode(r#"{"command":"open"}"#).expect_err("should be refused");
        assert!(error.to_string().contains("path"), "unhelpful: {error}");
    }

    #[test]
    fn structurally_invalid_input_is_refused_without_panicking() {
        let cases = [
            (r"not json at all", "bare text"),
            (r"{", "truncated object"),
            (r"[1,2,3]", "array"),
            (r#""a string""#, "bare string"),
            (r"42", "bare number"),
            (r"null", "null"),
            (r"{}", "empty object"),
            (r#"{"command":5}"#, "non-string command"),
            (r#"{"id":-1,"command":"next_page"}"#, "negative id"),
            (r#"{"id":"x","command":"next_page"}"#, "non-numeric id"),
            (r#"{"id":1.5,"command":"next_page"}"#, "fractional id"),
        ];
        for (line, what) in cases {
            assert!(decode(line).is_err(), "{what} ({line}) was accepted");
        }
    }

    #[test]
    fn an_over_long_line_is_refused_before_being_parsed() {
        // The bound exists so a client cannot exhaust memory by never sending a
        // newline. Checked on length, so it costs nothing to enforce.
        let line = format!(
            r#"{{"command":"next_page","pad":"{}"}}"#,
            "x".repeat(MAX_LINE_BYTES)
        );
        assert_eq!(decode(&line), Err(DecodeError::TooLong.at(None)));
    }

    #[test]
    fn deeply_nested_json_does_not_blow_the_stack() {
        // serde_json has a recursion limit by default; this asserts we rely on it
        // rather than discovering it in production.
        let deep = format!("{}{}", "[".repeat(2000), "]".repeat(2000));
        assert!(decode(&deep).is_err());
    }

    #[test]
    fn a_non_ascii_path_survives_decoding() {
        match command(r#"{"command":"open","path":"café/naïve.pdf"}"#) {
            Command::Open { path } => {
                let shown = path.to_string_lossy();
                assert!(shown.contains("café"), "mangled: {shown}");
                assert!(shown.contains("naïve"), "mangled: {shown}");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn an_escaped_control_character_in_a_path_is_accepted() {
        // Legal JSON, and legal in a filename on most systems. We should not be
        // stricter than the format is.
        match command(r#"{"command":"open","path":"odd\tname.pdf"}"#) {
            Command::Open { path } => {
                assert!(path.to_string_lossy().contains('\t'), "the tab was dropped");
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn a_raw_control_byte_is_rejected_as_json_requires() {
        // Unescaped control characters are invalid JSON. Accepting them would mean
        // silently disagreeing with whatever the client used to build the message,
        // which is a worse outcome for it than a clear rejection.
        let line = format!("{{\"command\":\"open\",\"path\":\"a{}b.pdf\"}}", '\u{1}');
        assert!(matches!(
            decode(&line),
            Err(DecodeFailure {
                reason: DecodeError::NotJson { .. },
                ..
            })
        ));
    }

    // --- Replies -------------------------------------------------------------

    #[test]
    fn a_successful_reply_omits_the_fields_it_does_not_use() {
        let json = serde_json::to_string(&Reply::ok(Some(3), "changed")).expect("serialize");
        assert_eq!(json, r#"{"id":3,"ok":true,"outcome":"changed"}"#);
    }

    #[test]
    fn a_reply_without_an_id_omits_it_entirely() {
        let json = serde_json::to_string(&Reply::ok(None, "unchanged")).expect("serialize");
        assert_eq!(json, r#"{"ok":true,"outcome":"unchanged"}"#);
    }

    #[test]
    fn a_rejection_becomes_a_readable_error() {
        let reply = Reply::rejected(
            Some(1),
            Rejection::NoSuchPage {
                page: PageNumber::new(10).expect("nonzero"),
                page_count: 3,
            },
        );
        assert!(!reply.ok);
        let error = reply.error.expect("an error message");
        assert!(error.contains("10"), "unhelpful: {error}");
        assert!(error.contains('3'), "unhelpful: {error}");
    }

    #[test]
    fn the_commands_reply_advertises_every_command() {
        let reply = Reply::with_commands(None);
        let commands = reply.commands.expect("a command list");
        // Against `all_names` rather than a hardcoded count. The literal `+ 4` here
        // failed the moment Goal 4 added five commands, which is the right failure but
        // in the wrong place: `command.rs` already has the exhaustive-match check that
        // makes the list trustworthy, so this only needs to confirm the reply carries
        // the whole thing.
        assert_eq!(commands.len(), Command::all_names().len());
        assert!(commands.contains(&"go_to_page"), "missing a view command");
        assert!(commands.contains(&"move_page"), "missing an edit command");
        assert!(commands.contains(&"quit"));
    }

    #[test]
    fn events_serialize_with_a_flat_event_tag() {
        let json = serde_json::to_string(&Event::Idle).expect("serialize");
        assert_eq!(json, r#"{"event":"idle"}"#);

        // The wire carries the page number, so index 3 reports as page 4.
        let json = serde_json::to_string(&Event::PageRendered {
            page: PageNumber::from_index(3),
        })
        .expect("serialize");
        assert_eq!(json, r#"{"event":"page_rendered","page":4}"#);

        let json = serde_json::to_string(&Event::PageFailed {
            page: PageNumber::from_index(1),
            reason: "timed out".to_owned(),
            will_retry: true,
        })
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"event":"page_failed","page":2,"reason":"timed out","will_retry":true}"#
        );
    }

    #[test]
    fn no_reply_or_event_serializes_with_an_embedded_newline() {
        // The framing is one object per line, so an embedded newline would split a
        // message in two and desynchronize the client.
        let messages = [
            serde_json::to_string(&Event::Idle).expect("serialize"),
            serde_json::to_string(&Event::DocumentOpened {
                path: "a\nb.pdf".to_owned(),
                page_count: 1,
            })
            .expect("serialize"),
            serde_json::to_string(&Reply::failed(Some(1), "line one\nline two"))
                .expect("serialize"),
        ];
        for message in messages {
            assert!(
                !message.contains('\n'),
                "message contains a raw newline: {message}"
            );
        }
    }
}
