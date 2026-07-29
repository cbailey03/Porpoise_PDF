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

use porpoise_view::ViewCommand;

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
    /// Close the window and exit.
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
            Self::Quit => "quit",
        }
    }

    /// Every shell command, plus every view command, as a reference list.
    ///
    /// Published by the control channel so an agent can ask what the program does
    /// instead of being told out of band. Built from [`ViewCommand::ALL`], so a new
    /// view command appears here without anyone remembering to add it.
    pub(crate) fn all_names() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = ViewCommand::ALL.iter().map(ViewCommand::name).collect();
        names.extend(
            [
                Self::Open {
                    path: PathBuf::new(),
                },
                Self::Close,
                Self::Capture {
                    path: PathBuf::new(),
                },
                Self::Quit,
            ]
            .iter()
            .map(Self::name),
        );
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
    fn the_reference_list_covers_view_and_shell_commands() {
        let names = Command::all_names();
        assert!(names.contains(&"next_page"), "missing a view command");
        assert!(names.contains(&"open"), "missing a shell command");
        assert!(names.contains(&"quit"));

        // Every view command appears, and the count is view + the four shell ones.
        assert_eq!(names.len(), ViewCommand::ALL.len() + 4);
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
