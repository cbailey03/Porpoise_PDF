//! The shell's command surface: everything [`ViewCommand`] cannot express.
//!
//! `porpoise-view` owns the commands that change what the view shows, and owns
//! them precisely because they need no document, no window, and no renderer — that
//! is what keeps them testable. The commands here cannot make that claim: opening
//! a file is I/O, capturing a window needs a window, and quitting needs an event
//! loop. So they live in the shell and wrap the pure ones rather than joining them.
//!
//! See `docs/goal-2-plan.md`, section 2.

use std::path::PathBuf;

use porpoise_view::{PageNumber, ViewCommand};

use crate::confirm::Answer;

/// Anything an operator — a person at the keyboard, or an agent — can ask of the
/// program.
///
/// Deliberately *not* `Deserialize`. The obvious derive would nest a view command
/// inside a `view` tag, giving `{"command":{"command":"next_page"}}` on the wire,
/// and `#[serde(untagged)]` would flatten it at the cost of collapsing every
/// decode failure into "data did not match any variant". An agent-facing protocol
/// needs to be told *which* command it got wrong and what the alternatives are, so
/// `crate::protocol` decodes by hand.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Command {
    /// Change what the view shows.
    View(ViewCommand),
    /// Open a PDF, replacing whatever is open.
    Open {
        /// Path to the file.
        path: PathBuf,
    },
    /// Close the open document, leaving an empty window.
    Close,
    /// Write the window's current contents to a PNG.
    ///
    /// Waits for the render pipeline to settle first, so the capture shows pages
    /// rather than placeholder tiles.
    Capture {
        /// Where to write the PNG.
        path: PathBuf,
    },
    /// Move a page to a different position in the document.
    ///
    /// Both are positions in the document *as currently shown*, counting from 1. The
    /// file on disk is untouched until a save.
    MovePage {
        /// The page to move.
        from: PageNumber,
        /// Where it should end up.
        to: PageNumber,
    },
    /// Remove a page from the document.
    ///
    /// Refused for the last remaining page: a PDF with no pages is not a PDF.
    DeletePage {
        /// The page to remove, counting from 1.
        page: PageNumber,
    },
    /// Undo the last page edit.
    Undo,
    /// Write the edited document over the file it came from.
    Save,
    /// Write the edited document to a new file, refusing to replace one.
    SaveAs {
        /// Where to write it.
        path: PathBuf,
    },
    /// Show or hide the page grid.
    ///
    /// A command rather than a click-only toggle because it changes what is on screen,
    /// and unlike the file dialog an agent that opens it can also close it — so there is
    /// no state it can enter and not leave. See `docs/goal-4-plan.md` §7.
    SetThumbnails {
        /// Whether the grid should be showing.
        visible: bool,
    },
    /// Answer the question raised when something would discard unsaved page changes.
    ///
    /// Does nothing when nothing is waiting on an answer. See [`crate::confirm`] for why
    /// this is a command rather than a click-only button: it is the way *out* of the
    /// question, and a question an agent cannot answer is one it can be stuck behind.
    Answer {
        /// Save first, go ahead anyway, or stay put.
        choice: Answer,
    },
    /// Close the window and exit.
    ///
    /// Refused with `needs_answer` while there are unsaved page changes, until an
    /// [`Self::Answer`] settles it.
    Quit,
}

impl Command {
    /// The wire name of this command.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::View(view) => view.name(),
            Self::Open { .. } => "open",
            Self::Close => "close",
            Self::Capture { .. } => "capture",
            Self::MovePage { .. } => "move_page",
            Self::DeletePage { .. } => "delete_page",
            Self::Undo => "undo",
            Self::Save => "save",
            Self::SaveAs { .. } => "save_as",
            Self::SetThumbnails { .. } => "set_thumbnails",
            Self::Answer { .. } => "answer",
            Self::Quit => "quit",
        }
    }

    /// One of each shell command, for building the reference list.
    ///
    /// The argument values are placeholders; only the variant matters. Kept exhaustive
    /// by `every_shell_command_is_in_the_reference_list`, which matches on every
    /// variant and so fails to compile when one is added — the same mechanism
    /// [`ViewCommand::ALL`] uses. Without it, a new command would simply be missing
    /// from what the control channel advertises, and an agent would have no way to
    /// discover it.
    fn shell_commands() -> Vec<Self> {
        let placeholder = || PathBuf::new();
        vec![
            Self::Open {
                path: placeholder(),
            },
            Self::Close,
            Self::Capture {
                path: placeholder(),
            },
            Self::MovePage {
                from: PageNumber::FIRST,
                to: PageNumber::FIRST,
            },
            Self::DeletePage {
                page: PageNumber::FIRST,
            },
            Self::Undo,
            Self::Save,
            Self::SaveAs {
                path: placeholder(),
            },
            Self::SetThumbnails { visible: true },
            Self::Answer {
                choice: Answer::Cancel,
            },
            Self::Quit,
        ]
    }

    /// Every shell command, plus every view command, as a reference list.
    ///
    /// Published by the control channel so an agent can ask what the program does
    /// instead of being told out of band. Built from [`ViewCommand::ALL`], so a new
    /// view command appears here without anyone remembering to add it.
    pub(crate) fn all_names() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = ViewCommand::ALL.iter().map(ViewCommand::name).collect();
        names.extend(Self::shell_commands().iter().map(Self::name));
        names
    }
}

impl From<ViewCommand> for Command {
    fn from(command: ViewCommand) -> Self {
        Self::View(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_commands_are_named() {
        assert_eq!(
            Command::Open {
                path: PathBuf::from("a.pdf")
            }
            .name(),
            "open"
        );
        assert_eq!(Command::Close.name(), "close");
        assert_eq!(Command::Quit.name(), "quit");
    }

    #[test]
    fn a_wrapped_view_command_keeps_its_own_name() {
        // The wrapper must be invisible on the wire: an agent sends `next_page`,
        // not `view` containing `next_page`.
        assert_eq!(Command::View(ViewCommand::NextPage).name(), "next_page");
    }

    #[test]
    fn every_shell_command_is_in_the_reference_list() {
        // This match is the enforcement. Adding a variant to `Command` without adding
        // it to `shell_commands` fails to compile *here*, which is the only reason the
        // list can be trusted as what the control channel advertises. `ViewCommand`
        // has had this since Goal 2; the shell list was built by hand and did not.
        let listed = Command::shell_commands();
        for command in &listed {
            match command {
                Command::View(_)
                | Command::Open { .. }
                | Command::Close
                | Command::Capture { .. }
                | Command::MovePage { .. }
                | Command::DeletePage { .. }
                | Command::Undo
                | Command::Save
                | Command::SaveAs { .. }
                | Command::SetThumbnails { .. }
                | Command::Answer { .. }
                | Command::Quit => {}
            }
        }

        // And the reverse: a variant matched above but forgotten in the list would
        // slip through, so count them.
        assert_eq!(
            listed.len(),
            11,
            "shell_commands has {} entries; update this count deliberately",
            listed.len()
        );
    }

    #[test]
    fn the_reference_list_covers_view_and_shell_commands() {
        let names = Command::all_names();
        assert!(names.contains(&"next_page"), "missing a view command");
        assert!(names.contains(&"open"), "missing a shell command");
        assert!(names.contains(&"move_page"), "missing an edit command");
        assert!(names.contains(&"save_as"));
        assert!(names.contains(&"quit"));

        assert_eq!(
            names.len(),
            ViewCommand::ALL.len() + Command::shell_commands().len()
        );
    }

    #[test]
    fn the_edit_commands_are_named() {
        assert_eq!(
            Command::MovePage {
                from: PageNumber::FIRST,
                to: PageNumber::FIRST
            }
            .name(),
            "move_page"
        );
        assert_eq!(
            Command::DeletePage {
                page: PageNumber::FIRST
            }
            .name(),
            "delete_page"
        );
        assert_eq!(Command::Undo.name(), "undo");
        assert_eq!(Command::Save.name(), "save");
        assert_eq!(
            Command::SaveAs {
                path: PathBuf::from("out.pdf")
            }
            .name(),
            "save_as"
        );
    }

    #[test]
    fn every_name_is_distinct() {
        let mut names = Command::all_names();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two commands share a wire name");
    }
}
