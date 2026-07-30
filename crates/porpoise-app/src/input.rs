//! Translating raw input into commands.
//!
//! Pure: no viewer state, no window, no side effects. That is what makes every
//! binding testable, and it is also the shape the command model wants — input is a
//! *producer* of commands like any other, so the translation is a function rather
//! than a method reaching into the app.

use eframe::egui;
use porpoise_view::{ScrollMode, ViewCommand, ZoomTarget};

use crate::command::Command;

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
