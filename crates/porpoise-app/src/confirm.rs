//! Asking before unsaved page changes are thrown away.
//!
//! Reorder twenty pages, click the X, and until this existed they were gone with no
//! prompt and no message. The program knew — the status bar said *unsaved changes* —
//! and nothing acted on it. Same for opening a different file, and for closing the
//! document.
//!
//! # It guards the command, not the gesture
//!
//! Everywhere else in this program a dialog is kept off the command surface, because a
//! box only a person can dismiss is a box an agent can get stuck behind
//! (`docs/goal-3-plan.md` §1). The obvious reading of that rule here would be to raise
//! the question from the X button, from `Ctrl+O`, and from a file drop — each producer
//! checking for itself.
//!
//! That reading is wrong twice over. It repeats the check three times, so a fourth
//! producer arrives unguarded. And it makes the *most* safety-critical behaviour in the
//! program the *only* one with no automated test, because no test can press an X.
//!
//! So the guard sits in front of dispatch and an agent gets the same protection a person
//! does. An agent that reorders pages and then opens another file loses work exactly as
//! a person would, so this is not a special case — it is the same rule. The way through
//! is [`Answer`], which is a real command, which is why the whole flow can be tested end
//! to end.
//!
//! The one thing that is *not* guarded is the control channel hanging up. There is
//! nobody left to ask, so the window closes and the edit is lost. Recorded rather than
//! papered over.

use std::path::PathBuf;

use crate::command::Command;

/// Something asked for that would throw away unsaved page changes.
///
/// Held until it is answered, then carried out or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Intent {
    /// Close the window and exit.
    Quit,
    /// Close the document, leaving an empty window.
    CloseDocument,
    /// Open a different document in its place.
    Open(PathBuf),
}

impl Intent {
    /// What this would do, for the question box and for the snapshot.
    ///
    /// The same string in both, so an agent reads exactly what a person is looking at.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Quit => "quit".to_owned(),
            Self::CloseDocument => "close this document".to_owned(),
            Self::Open(path) => format!("open {}", path.display()),
        }
    }
}

/// How a guarded request is waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Guard {
    /// Waiting on an answer.
    Asking(Intent),
    /// Answered with "save first"; waiting on the write to land.
    ///
    /// A separate state because a save is not instant — about a second on a 400-page
    /// document — and the intent must not be carried out until the file is really
    /// there. Collapsing this into `Asking` would mean quitting before the save
    /// finished, which loses the changes in the one case somebody explicitly asked to
    /// keep them.
    Saving(Intent),
}

/// What was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Answer {
    /// Write the changes, then go ahead.
    Save,
    /// Go ahead and lose the changes.
    Discard,
    /// Never mind; stay here.
    Cancel,
}

impl Answer {
    /// Every answer, for the protocol's error message.
    pub(crate) const ALL: [&'static str; 3] = ["save", "discard", "cancel"];
}

/// What this command would throw away, if anything.
///
/// The exhaustive match is the point. Adding a command that replaces or discards the
/// document without deciding about it here fails to compile, which is the only reason
/// this list can be trusted — the same mechanism `Command::shell_commands` uses to keep
/// the reference list honest.
pub(crate) fn intent_of(command: &Command) -> Option<Intent> {
    match command {
        Command::Quit => Some(Intent::Quit),
        Command::Close => Some(Intent::CloseDocument),
        Command::Open { path } => Some(Intent::Open(path.clone())),

        // Everything below either leaves the document alone, or is how you *keep* the
        // changes rather than lose them. `InsertFile`, `StageDocument`, `ClearStaging`
        // and `InsertPages` only ever add pages or touch the staging slot, so there is
        // nothing at risk to ask about — see `docs/goal-5-plan.md` §6 and §10.6.
        // `Answer` in particular must never be guarded: it is the way out of the
        // question, so guarding it would be a trap with no exit.
        Command::View(_)
        | Command::InsertFile { .. }
        | Command::StageDocument { .. }
        | Command::ClearStaging
        | Command::InsertPages { .. }
        | Command::Capture { .. }
        | Command::MovePage { .. }
        | Command::MovePages { .. }
        | Command::DeletePage { .. }
        | Command::DeletePages { .. }
        | Command::SetPageFilter { .. }
        | Command::SetSelection { .. }
        | Command::Undo
        | Command::Save
        | Command::SaveAs { .. }
        | Command::SetThumbnails { .. }
        | Command::SetGridMode { .. }
        | Command::Answer { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use porpoise_view::{PageNumber, ViewCommand};

    fn path() -> PathBuf {
        PathBuf::from("plans/sheet.pdf")
    }

    #[test]
    fn the_three_commands_that_lose_a_document_are_guarded() {
        assert_eq!(intent_of(&Command::Quit), Some(Intent::Quit));
        assert_eq!(intent_of(&Command::Close), Some(Intent::CloseDocument));
        assert_eq!(
            intent_of(&Command::Open { path: path() }),
            Some(Intent::Open(path()))
        );
    }

    #[test]
    fn editing_and_looking_around_are_not_guarded() {
        // Asking before every scroll would make the question meaningless.
        let unguarded = [
            Command::View(ViewCommand::NextPage),
            Command::MovePage {
                from: PageNumber::FIRST,
                to: PageNumber::FIRST,
            },
            Command::DeletePage {
                page: PageNumber::FIRST,
            },
            Command::Undo,
            Command::SetThumbnails { visible: true },
            Command::Capture { path: path() },
        ];
        for command in unguarded {
            assert_eq!(intent_of(&command), None, "{} was guarded", command.name());
        }
    }

    #[test]
    fn saving_is_never_guarded_because_it_is_the_way_to_keep_the_changes() {
        assert_eq!(intent_of(&Command::Save), None);
        assert_eq!(intent_of(&Command::SaveAs { path: path() }), None);
    }

    #[test]
    fn inserting_a_file_is_never_guarded_because_it_only_adds_pages() {
        assert_eq!(intent_of(&Command::InsertFile { path: path() }), None);
    }

    #[test]
    fn staging_and_inserting_pages_are_never_guarded() {
        // Staging adds nothing to the document, and inserting only adds to it — the
        // same reasoning `InsertFile` already gets. See `docs/goal-5-plan.md` §10.6.
        assert_eq!(intent_of(&Command::StageDocument { path: path() }), None);
        assert_eq!(intent_of(&Command::ClearStaging), None);
        assert_eq!(
            intent_of(&Command::InsertPages {
                pages: vec![PageNumber::FIRST],
                at: PageNumber::FIRST,
            }),
            None
        );
    }

    #[test]
    fn answering_is_never_guarded_or_the_question_would_have_no_exit() {
        // The one that would turn this from a safeguard into a trap: if the answer
        // needed confirming, nothing could ever get past the box.
        for answer in [Answer::Save, Answer::Discard, Answer::Cancel] {
            assert_eq!(
                intent_of(&Command::Answer { choice: answer }),
                None,
                "{answer:?} was guarded"
            );
        }
    }

    #[test]
    fn an_intent_says_what_it_would_do() {
        assert_eq!(Intent::Quit.describe(), "quit");
        assert!(Intent::CloseDocument.describe().contains("close"));
        let described = Intent::Open(path()).describe();
        assert!(described.starts_with("open "), "unhelpful: {described}");
        assert!(described.contains("sheet.pdf"), "unhelpful: {described}");
    }

    #[test]
    fn an_answer_decodes_from_its_wire_name() {
        for (wire, expected) in [
            ("\"save\"", Answer::Save),
            ("\"discard\"", Answer::Discard),
            ("\"cancel\"", Answer::Cancel),
        ] {
            let decoded: Answer = serde_json::from_str(wire).expect("should decode");
            assert_eq!(decoded, expected);
        }
        assert!(
            serde_json::from_str::<Answer>("\"maybe\"").is_err(),
            "accepted an answer that means nothing"
        );
    }

    #[test]
    fn every_answer_is_advertised() {
        // The protocol quotes `ALL` when it refuses a bad `choice`, so a fourth answer
        // that never reached this list would be undiscoverable.
        for answer in [Answer::Save, Answer::Discard, Answer::Cancel] {
            let wire = serde_json::to_value(match answer {
                Answer::Save => "save",
                Answer::Discard => "discard",
                Answer::Cancel => "cancel",
            })
            .expect("serialize");
            let name = wire.as_str().expect("a string");
            assert!(Answer::ALL.contains(&name), "{name} is not advertised");
        }
        assert_eq!(Answer::ALL.len(), 3);
    }
}
