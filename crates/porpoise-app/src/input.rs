//! Translating raw input into commands.
//!
//! Pure: no viewer state, no window, no side effects. That is what makes every
//! binding testable, and it is also the shape the command model wants — input is a
//! *producer* of commands like any other, so the translation is a function rather
//! than a method reaching into the app.

use std::path::{Path, PathBuf};

use eframe::egui;
use porpoise_view::{ScrollMode, ViewCommand, ZoomTarget};

use crate::command::Command;
use crate::label::file_label;

/// Fraction of the viewport a page-down moves in free-scroll mode.
///
/// Slightly less than a full screen so a line or two carries over, which makes it
/// obvious nothing was skipped.
pub(crate) const VIEWPORT_STEP_FRACTION: f64 = 0.9;

/// How far an arrow key scrolls or pans, in PDF points.
pub(crate) const ARROW_STEP_PT: f64 = 48.0;

/// A page edit asked for by a key press.
///
/// Separate from [`command_for_key`] because these need to know *which* page is on
/// screen, and this module deliberately knows nothing about the document. The caller
/// supplies the current page; this only decides what was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditKey {
    /// Move the current page one position earlier.
    MoveEarlier,
    /// Move the current page one position later.
    MoveLater,
    /// Undo the last page edit.
    Undo,
    /// Write the changes over the original.
    Save,
    /// Show or hide the page grid.
    ToggleThumbnails,
}

/// Which page edit, if any, this key press asks for.
///
/// All under Ctrl, because they change the document rather than the view and a bare
/// arrow key already means "scroll".
pub(crate) fn edit_for_key(key: egui::Key, modifiers: egui::Modifiers) -> Option<EditKey> {
    if !(modifiers.command || modifiers.ctrl) {
        return None;
    }
    match key {
        egui::Key::ArrowUp => Some(EditKey::MoveEarlier),
        egui::Key::ArrowDown => Some(EditKey::MoveLater),
        egui::Key::Z => Some(EditKey::Undo),
        egui::Key::S => Some(EditKey::Save),
        egui::Key::T => Some(EditKey::ToggleThumbnails),
        _ => None,
    }
}

/// Whether this key press asks for the file dialog.
///
/// Separate from [`command_for_key`] because the dialog is not a command — see
/// [`crate::picker`]. Pure, so the binding is testable without a window.
pub(crate) fn opens_the_picker(key: egui::Key, modifiers: egui::Modifiers) -> bool {
    (modifiers.command || modifiers.ctrl) && key == egui::Key::O
}

/// What dropping these files on the window would do.
///
/// Like the file dialog, a drop is a **producer** of [`Command::Open`] rather than a
/// command of its own — an agent already has `open` with a path, which is strictly more
/// capable than a gesture. See `docs/goal-3-plan.md` §1.
///
/// One decision serves two callers: the hint painted while the drag is still in the air
/// and the open that happens when the button is released. Computing those separately
/// would let the window promise one thing and do another, which is worse than having no
/// hint at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropAction {
    /// Open this document, ignoring `ignored` other dropped files.
    Open {
        path: PathBuf,
        /// How many other paths came with it. Zero for the ordinary single drop.
        ignored: usize,
    },
    /// Nothing dropped can be opened. Written for a person to read.
    Refuse { reason: String },
}

impl DropAction {
    /// The sentence to show over the window.
    ///
    /// `unsaved_changes` says an open will be interrupted by a question, since opening a
    /// document replaces the one on screen. Worth saying while the mouse button is still
    /// down, so the drag can be abandoned rather than answered.
    ///
    /// This used to read *"will be lost"*, which was true when the drop hint was the
    /// only warning there was. It is not true any more — [`crate::confirm`] asks first —
    /// and a warning that overstates what is at stake is one people stop believing.
    pub(crate) fn hint(&self, unsaved_changes: bool) -> String {
        match self {
            Self::Open { path, ignored } => {
                let mut parts = vec![format!("Open {}", file_label(path))];
                if *ignored > 0 {
                    parts.push(format!("ignoring {ignored} other file(s)"));
                }
                if unsaved_changes {
                    parts.push("you will be asked about your unsaved page changes".to_owned());
                }
                parts.join(" — ")
            }
            Self::Refuse { reason } => reason.clone(),
        }
    }
}

/// Decides what a set of dropped or hovered paths means.
///
/// `None` means nothing is there — no drop, or a drop egui gave us no paths for.
pub(crate) fn drop_action(paths: &[PathBuf]) -> Option<DropAction> {
    if paths.is_empty() {
        return None;
    }
    // The first PDF, not the first path: dropping a folder of drawings alongside the
    // one PDF in it should open the PDF rather than refuse the lot.
    match paths.iter().find(|path| is_pdf(path)) {
        Some(path) => Some(DropAction::Open {
            path: path.clone(),
            ignored: paths.len() - 1,
        }),
        None => Some(DropAction::Refuse {
            reason: match paths {
                [only] => format!("{} is not a PDF", file_label(only)),
                many => format!("none of those {} files is a PDF", many.len()),
            },
        }),
    }
}

/// Whether a path names a PDF, judged by its extension.
///
/// Extension only. Reading the first bytes would mean touching the disk while the
/// pointer is still moving, and guessing wrong here is cheap — the open itself reports
/// the real parse failure. A directory has no `.pdf` extension, so this is also what
/// keeps a dropped folder out.
fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// Translates a key press into a command.
///
/// Pure, and mode-aware on purpose. `PageDown` means "next page" in paged mode and
/// "next screenful" in free mode — so the *key handler* decides which command that
/// is. Putting the mode dependence inside a command would mean an agent could
/// never be sure what `NextPage` was going to do.
pub(crate) fn command_for_key(
    key: egui::Key,
    modifiers: egui::Modifiers,
    mode: ScrollMode,
) -> Option<Command> {
    if modifiers.command || modifiers.ctrl {
        let command = match key {
            egui::Key::Plus | egui::Key::Equals => ViewCommand::StepZoom { rungs: 1 },
            egui::Key::Minus => ViewCommand::StepZoom { rungs: -1 },
            egui::Key::Num0 => ViewCommand::SetZoom {
                target: ZoomTarget::FitWidth,
            },
            egui::Key::Num1 => ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(1.0),
            },
            egui::Key::Num2 => ViewCommand::SetZoom {
                target: ZoomTarget::FitPage,
            },
            _ => return None,
        };
        return Some(command.into());
    }

    // One screenful or one page, depending on the mode.
    let advance = |forward: bool| -> ViewCommand {
        let sign = if forward { 1.0 } else { -1.0 };
        match mode {
            ScrollMode::Paged if forward => ViewCommand::NextPage,
            ScrollMode::Paged => ViewCommand::PreviousPage,
            ScrollMode::Free => ViewCommand::ScrollByViewports {
                fraction: VIEWPORT_STEP_FRACTION * sign,
            },
        }
    };

    let command = match key {
        egui::Key::PageDown => advance(true),
        egui::Key::PageUp => advance(false),
        // Space is the reader's page-down; shift reverses it.
        egui::Key::Space => advance(!modifiers.shift),
        egui::Key::Home => ViewCommand::FirstPage,
        egui::Key::End => ViewCommand::LastPage,
        egui::Key::ArrowDown => ViewCommand::ScrollBy {
            points: ARROW_STEP_PT,
        },
        egui::Key::ArrowUp => ViewCommand::ScrollBy {
            points: -ARROW_STEP_PT,
        },
        // Rejected as `Unchanged` when the document fits the window, so these are
        // harmless at fit-width and useful the moment anyone zooms in.
        egui::Key::ArrowRight => ViewCommand::PanBy {
            points: ARROW_STEP_PT,
        },
        egui::Key::ArrowLeft => ViewCommand::PanBy {
            points: -ARROW_STEP_PT,
        },
        _ => return None,
    };
    Some(command.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> egui::Modifiers {
        egui::Modifiers::NONE
    }

    fn ctrl() -> egui::Modifiers {
        egui::Modifiers::CTRL
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers, mode: ScrollMode) -> Option<Command> {
        command_for_key(key, modifiers, mode)
    }

    #[test]
    fn ctrl_o_asks_for_the_file_dialog_and_is_not_a_command() {
        assert!(opens_the_picker(egui::Key::O, ctrl()));
        assert!(
            key(egui::Key::O, ctrl(), ScrollMode::Free).is_none(),
            "the dialog is not a command; `command_for_key` must not claim this key"
        );
    }

    #[test]
    fn a_bare_o_does_not_open_the_dialog() {
        // Guards against the dialog appearing while someone is typing.
        assert!(!opens_the_picker(egui::Key::O, none()));
    }

    #[test]
    fn ctrl_with_another_key_does_not_open_the_dialog() {
        assert!(!opens_the_picker(egui::Key::P, ctrl()));
    }

    // --- File drops ----------------------------------------------------------

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_dropped_pdf_asks_to_open_it() {
        assert_eq!(
            drop_action(&paths(&["plans/sheet.pdf"])),
            Some(DropAction::Open {
                path: PathBuf::from("plans/sheet.pdf"),
                ignored: 0,
            })
        );
    }

    #[test]
    fn an_uppercase_extension_is_still_a_pdf() {
        // Windows hands back whatever case is on disk, and `.PDF` is common on files
        // that came off a plotter or an email attachment.
        for name in ["sheet.PDF", "sheet.Pdf", "sheet.pDf"] {
            assert!(
                matches!(drop_action(&paths(&[name])), Some(DropAction::Open { .. })),
                "{name} was not recognised as a PDF"
            );
        }
    }

    #[test]
    fn nothing_dropped_asks_for_nothing() {
        assert_eq!(drop_action(&[]), None);
    }

    #[test]
    fn a_dropped_file_that_is_not_a_pdf_is_refused_by_name() {
        // Named, because "nothing happened" is indistinguishable from the window
        // being broken.
        let Some(DropAction::Refuse { reason }) = drop_action(&paths(&["notes/minutes.docx"]))
        else {
            panic!("a .docx should be refused");
        };
        assert!(reason.contains("minutes.docx"), "unhelpful: {reason}");
        assert!(reason.contains("not a PDF"), "unhelpful: {reason}");
    }

    #[test]
    fn a_dropped_folder_is_refused() {
        // A directory has no `.pdf` extension, which is all that keeps it out — worth
        // pinning, because opening one would hand a path to `Document::open`.
        assert!(matches!(
            drop_action(&paths(&["C:/plans/gdot"])),
            Some(DropAction::Refuse { .. })
        ));
    }

    #[test]
    fn the_first_pdf_wins_rather_than_the_first_path() {
        // Dropping a folder's worth of files that happens to contain one PDF should
        // open the PDF, not refuse everything because a README came first.
        assert_eq!(
            drop_action(&paths(&["readme.txt", "sheet.pdf", "logo.png"])),
            Some(DropAction::Open {
                path: PathBuf::from("sheet.pdf"),
                ignored: 2,
            })
        );
    }

    #[test]
    fn refusing_several_files_says_how_many() {
        let Some(DropAction::Refuse { reason }) = drop_action(&paths(&["a.txt", "b.png"])) else {
            panic!("neither is a PDF");
        };
        assert!(reason.contains('2'), "unhelpful: {reason}");
    }

    #[test]
    fn the_hint_names_the_file_that_would_open() {
        let action = drop_action(&paths(&["plans/ROLT14.pdf"])).expect("a PDF was dropped");
        let hint = action.hint(false);
        assert!(hint.contains("ROLT14.pdf"), "unhelpful: {hint}");
        // Just the file name: a full path of a hundred characters would run off both
        // edges of the window.
        assert!(!hint.contains("plans"), "showed the whole path: {hint}");
    }

    #[test]
    fn the_hint_says_when_other_files_will_be_ignored() {
        let action = drop_action(&paths(&["sheet.pdf", "other.pdf"])).expect("a PDF was dropped");
        let hint = action.hint(false);
        assert!(hint.contains("ignoring 1"), "unhelpful: {hint}");
    }

    #[test]
    fn the_hint_mentions_unsaved_page_changes() {
        // So the drag can be abandoned rather than answered. It must not claim they
        // *will be lost* — `crate::confirm` asks first, and a warning that overstates
        // the stakes is one people stop believing.
        let action = drop_action(&paths(&["sheet.pdf"])).expect("a PDF was dropped");
        assert!(!action.hint(false).contains("unsaved"));
        let warned = action.hint(true);
        assert!(warned.contains("unsaved"), "unhelpful: {warned}");
        assert!(!warned.contains("lost"), "overstates the stakes: {warned}");
    }

    #[test]
    fn the_hint_and_the_drop_never_disagree() {
        // The whole point of one `drop_action` serving both. If the hint were computed
        // separately, the window could invite a drop it then refuses — a worse outcome
        // than showing nothing at all.
        let cases = [
            vec![],
            paths(&["sheet.pdf"]),
            paths(&["notes.txt"]),
            paths(&["notes.txt", "sheet.pdf"]),
            paths(&["a.txt", "b.png", "c"]),
        ];
        for case in cases {
            // Nothing there means no hint is drawn, which cannot disagree with anything.
            let Some(action) = drop_action(&case) else {
                continue;
            };
            let hint = action.hint(false);
            match &action {
                DropAction::Open { path, .. } => assert!(
                    hint.starts_with("Open ") && hint.contains(&file_label(path)),
                    "{case:?} would open {} but the hint said {hint:?}",
                    path.display()
                ),
                DropAction::Refuse { reason } => assert_eq!(
                    &hint, reason,
                    "{case:?} refuses but the hint said something else"
                ),
            }
        }
    }

    // --- Keys ----------------------------------------------------------------

    #[test]
    fn page_down_means_a_page_in_paged_mode_and_a_screenful_in_free_mode() {
        // This is the mode-dependence the command model deliberately keeps in the
        // key handler rather than inside `NextPage`.
        assert_eq!(
            key(egui::Key::PageDown, none(), ScrollMode::Paged),
            Some(ViewCommand::NextPage.into())
        );
        assert_eq!(
            key(egui::Key::PageDown, none(), ScrollMode::Free),
            Some(
                ViewCommand::ScrollByViewports {
                    fraction: VIEWPORT_STEP_FRACTION
                }
                .into()
            )
        );
    }

    #[test]
    fn page_up_reverses_the_direction_in_both_modes() {
        assert_eq!(
            key(egui::Key::PageUp, none(), ScrollMode::Paged),
            Some(ViewCommand::PreviousPage.into())
        );
        assert_eq!(
            key(egui::Key::PageUp, none(), ScrollMode::Free),
            Some(
                ViewCommand::ScrollByViewports {
                    fraction: -VIEWPORT_STEP_FRACTION
                }
                .into()
            )
        );
    }

    #[test]
    fn space_pages_forward_and_shift_space_pages_back() {
        assert_eq!(
            key(egui::Key::Space, none(), ScrollMode::Paged),
            Some(ViewCommand::NextPage.into())
        );
        assert_eq!(
            key(egui::Key::Space, egui::Modifiers::SHIFT, ScrollMode::Paged),
            Some(ViewCommand::PreviousPage.into())
        );
    }

    #[test]
    fn home_and_end_jump_to_the_ends_regardless_of_mode() {
        for mode in [ScrollMode::Free, ScrollMode::Paged] {
            assert_eq!(
                key(egui::Key::Home, none(), mode),
                Some(ViewCommand::FirstPage.into())
            );
            assert_eq!(
                key(egui::Key::End, none(), mode),
                Some(ViewCommand::LastPage.into())
            );
        }
    }

    #[test]
    fn arrows_scroll_a_small_fixed_step() {
        assert_eq!(
            key(egui::Key::ArrowDown, none(), ScrollMode::Free),
            Some(
                ViewCommand::ScrollBy {
                    points: ARROW_STEP_PT
                }
                .into()
            )
        );
        assert_eq!(
            key(egui::Key::ArrowUp, none(), ScrollMode::Free),
            Some(
                ViewCommand::ScrollBy {
                    points: -ARROW_STEP_PT
                }
                .into()
            )
        );
    }

    #[test]
    fn ctrl_bindings_control_zoom() {
        assert_eq!(
            key(egui::Key::Num0, ctrl(), ScrollMode::Free),
            Some(
                ViewCommand::SetZoom {
                    target: ZoomTarget::FitWidth
                }
                .into()
            )
        );
        assert_eq!(
            key(egui::Key::Num1, ctrl(), ScrollMode::Free),
            Some(
                ViewCommand::SetZoom {
                    target: ZoomTarget::Fixed(1.0)
                }
                .into()
            )
        );
        assert_eq!(
            key(egui::Key::Num2, ctrl(), ScrollMode::Free),
            Some(
                ViewCommand::SetZoom {
                    target: ZoomTarget::FitPage
                }
                .into()
            )
        );
        assert_eq!(
            key(egui::Key::Plus, ctrl(), ScrollMode::Free),
            Some(ViewCommand::StepZoom { rungs: 1 }.into())
        );
        assert_eq!(
            key(egui::Key::Minus, ctrl(), ScrollMode::Free),
            Some(ViewCommand::StepZoom { rungs: -1 }.into())
        );
    }

    #[test]
    fn a_ctrl_binding_does_not_also_fire_its_unmodified_meaning() {
        // Ctrl+End must not jump to the last page as a side effect of not being a
        // zoom binding.
        assert_eq!(key(egui::Key::End, ctrl(), ScrollMode::Free), None);
        assert_eq!(key(egui::Key::Space, ctrl(), ScrollMode::Free), None);
    }

    #[test]
    fn unbound_keys_produce_nothing() {
        for k in [egui::Key::A, egui::Key::F5, egui::Key::Escape] {
            assert_eq!(key(k, none(), ScrollMode::Free), None, "{k:?} is bound");
        }
    }

    #[test]
    fn every_key_binding_produces_a_command_an_agent_could_also_send() {
        // The point of the model: nothing is reachable by keyboard alone. If a
        // binding ever produced something outside the command set, this would be
        // the place it showed up.
        let bindings = [
            (egui::Key::PageDown, none()),
            (egui::Key::PageUp, none()),
            (egui::Key::Space, none()),
            (egui::Key::Home, none()),
            (egui::Key::End, none()),
            (egui::Key::ArrowDown, none()),
            (egui::Key::ArrowUp, none()),
            (egui::Key::Plus, ctrl()),
            (egui::Key::Minus, ctrl()),
            (egui::Key::Num0, ctrl()),
            (egui::Key::Num1, ctrl()),
            (egui::Key::Num2, ctrl()),
        ];
        let names = Command::all_names();
        for (k, modifiers) in bindings {
            for mode in [ScrollMode::Free, ScrollMode::Paged] {
                let command = command_for_key(k, modifiers, mode)
                    .unwrap_or_else(|| panic!("{k:?} produced no command"));
                assert!(
                    names.contains(&command.name()),
                    "{k:?} produced {}, which is not in the command reference",
                    command.name()
                );
            }
        }
    }
}
