//! Translating raw input into commands.
//!
//! Pure: no viewer state, no window, no side effects. That is what makes every
//! binding testable, and it is also the shape the command model wants — input is a
//! *producer* of commands like any other, so the translation is a function rather
//! than a method reaching into the app.

use std::path::{Path, PathBuf};

use eframe::egui;
use porpoise_view::{ScrollMode, ScrollRoom, ViewCommand, ZoomTarget};

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
/// Like the file dialog, a drop is a **producer** of [`Command::Open`] or
/// [`Command::InsertFile`] rather than a command of its own — an agent already has
/// both, with a path, which is strictly more capable than a gesture. See
/// `docs/goal-3-plan.md` §1 and `docs/goal-5-plan.md` §6.
///
/// One decision serves two callers: the hint painted while the drag is still in the air
/// and the action that happens when the button is released. Computing those separately
/// would let the window promise one thing and do another, which is worse than having no
/// hint at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropAction {
    /// Open this document, replacing whatever is open, ignoring `ignored` other
    /// dropped files.
    Open {
        path: PathBuf,
        /// How many other paths came with it. Zero for the ordinary single drop.
        ignored: usize,
    },
    /// Add this document's pages to the one already open, ignoring `ignored` other
    /// dropped files.
    ///
    /// Only ever produced when a document is open and the drop lands on the page
    /// grid — see [`crate::viewer`]'s use of this type. Dropped anywhere else, or
    /// with nothing open to insert into, a PDF still means [`Self::Open`].
    Insert {
        path: PathBuf,
        /// How many other paths came with it.
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
    /// down, so the drag can be abandoned rather than answered. Inserting is never
    /// guarded — see `docs/goal-5-plan.md` §6 — so it has nothing to say here.
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
            Self::Insert { path, ignored } => {
                let mut parts = vec![format!("Add pages from {}", file_label(path))];
                if *ignored > 0 {
                    parts.push(format!("ignoring {ignored} other file(s)"));
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
///
/// `insert` says whether this drop lands somewhere that means "add to what is open"
/// rather than "replace it" — the caller works that out from where the pointer is and
/// whether a document is open, since this function knows about neither. See
/// [`crate::viewer::Viewer::drop_targets_the_grid`].
pub(crate) fn drop_action(paths: &[PathBuf], insert: bool) -> Option<DropAction> {
    if paths.is_empty() {
        return None;
    }
    // The first PDF, not the first path: dropping a folder of drawings alongside the
    // one PDF in it should open the PDF rather than refuse the lot.
    match paths.iter().find(|path| is_pdf(path)) {
        Some(path) => {
            let ignored = paths.len() - 1;
            Some(if insert {
                DropAction::Insert {
                    path: path.clone(),
                    ignored,
                }
            } else {
                DropAction::Open {
                    path: path.clone(),
                    ignored,
                }
            })
        }
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

/// How far a wheel gesture scrolls when it turns a page, in screenfuls.
///
/// One screenful is more than enough to cross an edge the view is already sitting on,
/// and an edge is the only place a turn is ever emitted from — so the distance decides
/// nothing. It is a screenful because that is the honest name for "as much as the window
/// can show", and because [`porpoise_view::ViewCommand::ScrollByViewports`] already
/// means exactly that.
const TURN_SCREENFULS: f64 = 1.0;

/// One frame of wheel input, in the terms a single-page view needs.
///
/// Two kinds of device, and the difference matters here in a way it does not in free
/// mode. A **mouse wheel** sends one event per notch, and a person spinning it expects a
/// page per notch. A **trackpad** sends a continuous stream, and one swipe should turn
/// one page however many events it is made of.
///
/// The test for which is which is the one egui itself uses to decide whether the input
/// needs smoothing: small movements measured in points come from a trackpad, everything
/// else is a notch. Reused rather than reinvented, so the two cannot disagree about the
/// device in front of them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Wheel {
    /// Movement from discrete notches this frame. Positive moves further into the
    /// document — the opposite sign to egui's delta, which says how the *content* moves.
    pub(crate) notched: f32,
    /// Movement from a continuous gesture this frame, same sign.
    pub(crate) glided: f32,
    /// Whether a scroll is still in progress, including egui's own smoothing tail and a
    /// trackpad's kinetic one. This is what tells one gesture from the next.
    pub(crate) gliding: bool,
}

impl Wheel {
    /// Reads this frame's wheel events.
    ///
    /// The raw events, not [`egui::InputState::smooth_scroll_delta`]: egui spreads one
    /// notch over half a dozen frames to make hand-scrolling feel right, and a page turn
    /// per frame of that would fly through the document six pages at a time.
    pub(crate) fn read(input: &egui::InputState) -> Self {
        let mut wheel = Self {
            gliding: input.is_scrolling(),
            ..Self::default()
        };
        for event in &input.events {
            let egui::Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            // Ctrl or cmd plus wheel is a zoom, and egui has already folded it into
            // `zoom_delta` for `handle_input` to act on. Counted here as well it would
            // zoom *and* turn the page.
            //
            // Shift plus wheel is horizontal scrolling by platform convention, and a
            // single-page view has nowhere sideways to turn.
            if modifiers.command || modifiers.ctrl || modifiers.shift {
                continue;
            }
            let movement = -delta.y;
            if matches!(unit, egui::MouseWheelUnit::Point) && delta.length() < 8.0 {
                wheel.glided += movement;
            } else {
                wheel.notched += movement;
            }
        }
        wheel
    }
}

/// Whether a wheel gesture belongs to the pages rather than to the thumbnail strip.
///
/// `strip` is where the thumbnail strip is, or `None` when it is not showing.
///
/// Deliberately *not* "is the pointer over the pages", which is the obvious way round and
/// is wrong. A wheel event can arrive with no pointer position at all: winit's mouse-leave
/// tracking clears egui's `hover_pos` the moment the cursor is not where it expects, and
/// that was measured rather than imagined — under a pointer-over-the-pages test, page turns
/// silently stopped happening.
///
/// The strip is the only other scrolling area, so anything not over it belongs to the
/// pages. Written this way round because the two failure modes are not symmetric: guessing
/// "the pages" wrongly scrolls the wrong panel for one gesture, while guessing "not the
/// pages" wrongly leaves paged mode looking exactly as broken as it did before it confined
/// anything.
pub(crate) fn wheel_is_for_the_pages(
    pointer: Option<egui::Pos2>,
    strip: Option<egui::Rect>,
) -> bool {
    match (pointer, strip) {
        (Some(at), Some(strip)) => !strip.contains(at),
        _ => true,
    }
}

/// Turns wheel gestures into page turns at the edges of a page.
///
/// Only meaningful in [`ScrollMode::Paged`]: it is the piece that makes the wheel obey a
/// single-page view instead of rolling straight through the document.
#[derive(Debug, Default)]
pub(crate) struct PageTurns {
    /// Whether the gesture in progress has already had its turn.
    ///
    /// Not spent by a notch, which is a gesture of its own by definition.
    spent: bool,
}

impl PageTurns {
    /// What this frame's wheel should do, or `None` to leave it to the scroll area.
    ///
    /// Returning `None` is the common case and the important one: with room left inside
    /// the page, egui scrolls it with its own inertia and smoothing, which is what makes
    /// hand-scrolling feel right and is not worth reimplementing.
    pub(crate) fn turn(&mut self, wheel: Wheel, room: ScrollRoom) -> Option<ViewCommand> {
        // A continuous gesture stays spent until the movement stops. Without this one
        // trackpad swipe -- dozens of events over dozens of frames -- turns dozens of
        // pages.
        if !wheel.gliding {
            self.spent = false;
        }

        let movement = if wheel.notched != 0.0 {
            wheel.notched
        } else if wheel.glided != 0.0 && !self.spent {
            // Claimed whether or not it turns a page, so that a swipe which starts by
            // scrolling *within* the page does not also turn one when it reaches the
            // bottom. One gesture does one thing.
            self.spent = true;
            wheel.glided
        } else {
            return None;
        };
        if !movement.is_finite() {
            return None;
        }

        // Room left inside the page: turning here would skip whatever is below the fold.
        if movement > 0.0 && room.below || movement < 0.0 && room.above {
            return None;
        }
        Some(ViewCommand::ScrollByViewports {
            fraction: if movement > 0.0 {
                TURN_SCREENFULS
            } else {
                -TURN_SCREENFULS
            },
        })
    }
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
            drop_action(&paths(&["plans/sheet.pdf"]), false),
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
                matches!(drop_action(&paths(&[name]), false), Some(DropAction::Open { .. })),
                "{name} was not recognised as a PDF"
            );
        }
    }

    #[test]
    fn nothing_dropped_asks_for_nothing() {
        assert_eq!(drop_action(&[], false), None);
        assert_eq!(drop_action(&[], true), None);
    }

    #[test]
    fn a_dropped_file_that_is_not_a_pdf_is_refused_by_name() {
        // Named, because "nothing happened" is indistinguishable from the window
        // being broken.
        let Some(DropAction::Refuse { reason }) =
            drop_action(&paths(&["notes/minutes.docx"]), false)
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
            drop_action(&paths(&["C:/plans/gdot"]), false),
            Some(DropAction::Refuse { .. })
        ));
    }

    #[test]
    fn the_first_pdf_wins_rather_than_the_first_path() {
        // Dropping a folder's worth of files that happens to contain one PDF should
        // open the PDF, not refuse everything because a README came first.
        assert_eq!(
            drop_action(&paths(&["readme.txt", "sheet.pdf", "logo.png"]), false),
            Some(DropAction::Open {
                path: PathBuf::from("sheet.pdf"),
                ignored: 2,
            })
        );
    }

    #[test]
    fn refusing_several_files_says_how_many() {
        let Some(DropAction::Refuse { reason }) = drop_action(&paths(&["a.txt", "b.png"]), false)
        else {
            panic!("neither is a PDF");
        };
        assert!(reason.contains('2'), "unhelpful: {reason}");
    }

    #[test]
    fn the_hint_names_the_file_that_would_open() {
        let action = drop_action(&paths(&["plans/ROLT14.pdf"]), false).expect("a PDF was dropped");
        let hint = action.hint(false);
        assert!(hint.contains("ROLT14.pdf"), "unhelpful: {hint}");
        // Just the file name: a full path of a hundred characters would run off both
        // edges of the window.
        assert!(!hint.contains("plans"), "showed the whole path: {hint}");
    }

    #[test]
    fn the_hint_says_when_other_files_will_be_ignored() {
        let action =
            drop_action(&paths(&["sheet.pdf", "other.pdf"]), false).expect("a PDF was dropped");
        let hint = action.hint(false);
        assert!(hint.contains("ignoring 1"), "unhelpful: {hint}");
    }

    #[test]
    fn the_hint_mentions_unsaved_page_changes() {
        // So the drag can be abandoned rather than answered. It must not claim they
        // *will be lost* — `crate::confirm` asks first, and a warning that overstates
        // the stakes is one people stop believing.
        let action = drop_action(&paths(&["sheet.pdf"]), false).expect("a PDF was dropped");
        assert!(!action.hint(false).contains("unsaved"));
        let warned = action.hint(true);
        assert!(warned.contains("unsaved"), "unhelpful: {warned}");
        assert!(!warned.contains("lost"), "overstates the stakes: {warned}");
    }

    // --- Dropping onto the page grid: inserting rather than opening -----------

    #[test]
    fn a_pdf_dropped_on_the_grid_asks_to_insert_it() {
        assert_eq!(
            drop_action(&paths(&["plans/sheet.pdf"]), true),
            Some(DropAction::Insert {
                path: PathBuf::from("plans/sheet.pdf"),
                ignored: 0,
            })
        );
    }

    #[test]
    fn the_insert_hint_never_mentions_unsaved_changes() {
        // Inserting is never guarded — see `crate::confirm` — so nothing is at risk to
        // warn about, unlike an open that would replace the document.
        let action = drop_action(&paths(&["sheet.pdf"]), true).expect("a PDF was dropped");
        assert!(!action.hint(true).contains("unsaved"));
    }

    #[test]
    fn a_non_pdf_is_still_refused_when_dropped_on_the_grid() {
        // `insert` only changes what a *PDF* means; it does not relax which files count
        // as one.
        assert!(matches!(
            drop_action(&paths(&["notes.docx"]), true),
            Some(DropAction::Refuse { .. })
        ));
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
            for insert in [false, true] {
                // Nothing there means no hint is drawn, which cannot disagree with
                // anything.
                let Some(action) = drop_action(&case, insert) else {
                    continue;
                };
                let hint = action.hint(false);
                match &action {
                    DropAction::Open { path, .. } => assert!(
                        hint.starts_with("Open ") && hint.contains(&file_label(path)),
                        "{case:?} would open {} but the hint said {hint:?}",
                        path.display()
                    ),
                    DropAction::Insert { path, .. } => assert!(
                        hint.starts_with("Add pages from ") && hint.contains(&file_label(path)),
                        "{case:?} would insert {} but the hint said {hint:?}",
                        path.display()
                    ),
                    DropAction::Refuse { reason } => assert_eq!(
                        &hint, reason,
                        "{case:?} refuses but the hint said something else"
                    ),
                }
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

    // --- Turning pages with the wheel ----------------------------------------

    /// A page with nowhere left to scroll — the normal case at fit-page, and the one
    /// where every gesture is a page turn.
    fn stuck() -> ScrollRoom {
        ScrollRoom {
            above: false,
            below: false,
        }
    }

    /// A page taller than the window, scrolled to somewhere in the middle.
    fn room_both_ways() -> ScrollRoom {
        ScrollRoom {
            above: true,
            below: true,
        }
    }

    /// One mouse-wheel notch. Positive moves further into the document.
    fn notch(movement: f32) -> Wheel {
        Wheel {
            notched: movement,
            glided: 0.0,
            // A notch is discrete: egui reports the scroll as still in progress while it
            // smooths the notch out, and that must not hold the next notch back.
            gliding: true,
        }
    }

    /// One frame of a trackpad swipe.
    fn glide(movement: f32) -> Wheel {
        Wheel {
            notched: 0.0,
            glided: movement,
            gliding: true,
        }
    }

    fn forward() -> Option<ViewCommand> {
        Some(ViewCommand::ScrollByViewports {
            fraction: TURN_SCREENFULS,
        })
    }

    fn back() -> Option<ViewCommand> {
        Some(ViewCommand::ScrollByViewports {
            fraction: -TURN_SCREENFULS,
        })
    }

    #[test]
    fn a_notch_at_the_bottom_of_a_page_turns_to_the_next() {
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(notch(30.0), stuck()), forward());
    }

    #[test]
    fn a_notch_at_the_top_of_a_page_turns_back() {
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(notch(-30.0), stuck()), back());
    }

    #[test]
    fn every_notch_turns_a_page() {
        // The reason notches and swipes are told apart at all. A person spinning a wheel
        // expects a page per notch, and `is_scrolling` stays true across a spin — so a
        // rule based on the gesture alone would turn one page and then sit there.
        let mut turns = PageTurns::default();
        for spin in 0..5 {
            assert_eq!(
                turns.turn(notch(30.0), stuck()),
                forward(),
                "notch {spin} of a spin did not turn a page"
            );
        }
    }

    #[test]
    fn a_notch_with_room_left_in_the_page_is_left_to_the_scroll_area() {
        // The common case on a page taller than the window: egui scrolls it with its own
        // inertia, which is what makes hand-scrolling feel right.
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(notch(30.0), room_both_ways()), None);
        assert_eq!(turns.turn(notch(-30.0), room_both_ways()), None);
    }

    #[test]
    fn room_is_read_per_direction() {
        // At the bottom of a tall page: scrolling on turns the page, scrolling back does
        // not, because there is still page above.
        let room = ScrollRoom {
            above: true,
            below: false,
        };
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(notch(30.0), room), forward());
        assert_eq!(turns.turn(notch(-30.0), room), None);
    }

    #[test]
    fn one_swipe_turns_one_page() {
        // A trackpad swipe is dozens of events over dozens of frames. Without the latch
        // it would turn dozens of pages.
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(glide(20.0), stuck()), forward());
        for frame in 0..30 {
            assert_eq!(
                turns.turn(glide(20.0), stuck()),
                None,
                "frame {frame} of one swipe turned a second page"
            );
        }
    }

    #[test]
    fn the_next_swipe_turns_again() {
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(glide(20.0), stuck()), forward());
        assert_eq!(turns.turn(glide(20.0), stuck()), None);
        // Movement stopped: the gesture is over, kinetic tail and all.
        assert_eq!(turns.turn(Wheel::default(), stuck()), None);
        assert_eq!(turns.turn(glide(20.0), stuck()), forward());
    }

    #[test]
    fn a_swipe_that_starts_by_scrolling_does_not_also_turn_a_page() {
        // One gesture does one thing. A swipe that begins with page left to scroll spends
        // itself on that page rather than carrying on into the next one when it hits the
        // bottom — otherwise a long swipe would scroll *and* turn, which reads as the
        // document jumping.
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(glide(20.0), room_both_ways()), None);
        for frame in 0..10 {
            assert_eq!(
                turns.turn(glide(20.0), stuck()),
                None,
                "frame {frame} of a scrolling swipe turned a page"
            );
        }
    }

    #[test]
    fn no_movement_does_nothing() {
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(Wheel::default(), stuck()), None);
    }

    #[test]
    fn a_non_finite_delta_does_not_turn_a_page() {
        let mut turns = PageTurns::default();
        assert_eq!(turns.turn(notch(f32::NAN), stuck()), None);
        assert_eq!(turns.turn(notch(f32::INFINITY), stuck()), None);
    }

    // --- Which panel a gesture belongs to ------------------------------------

    fn strip() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 22.0), egui::vec2(300.0, 700.0))
    }

    #[test]
    fn a_gesture_over_the_thumbnail_strip_is_not_a_page_turn() {
        assert!(!wheel_is_for_the_pages(
            Some(egui::pos2(150.0, 400.0)),
            Some(strip())
        ));
    }

    #[test]
    fn a_gesture_beside_the_strip_is_a_page_turn() {
        assert!(wheel_is_for_the_pages(
            Some(egui::pos2(600.0, 400.0)),
            Some(strip())
        ));
    }

    #[test]
    fn a_gesture_with_no_pointer_at_all_still_turns_the_page() {
        // The one that had to be measured. winit clears egui's hover position as soon as the
        // cursor is not where it expects, so a wheel event routinely arrives with no pointer
        // — and under the obvious test, "is the pointer over the pages", every page turn
        // silently stopped happening.
        assert!(wheel_is_for_the_pages(None, Some(strip())));
    }

    #[test]
    fn with_the_strip_hidden_every_gesture_is_a_page_turn() {
        assert!(wheel_is_for_the_pages(Some(egui::pos2(150.0, 400.0)), None));
        assert!(wheel_is_for_the_pages(None, None));
    }

    // --- Reading the wheel off a frame ---------------------------------------

    fn wheel_event(unit: egui::MouseWheelUnit, delta: egui::Vec2) -> egui::Event {
        egui::Event::MouseWheel {
            unit,
            delta,
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn read(events: Vec<egui::Event>) -> Wheel {
        // `InputState` has private fields, so a default and one public field is the only
        // way to build one from outside egui.
        let mut input = egui::InputState::default();
        input.events = events;
        Wheel::read(&input)
    }

    #[test]
    fn a_wheel_notch_reads_as_movement_into_the_document() {
        // egui's delta says how the *content* moves; a scroll position says how the *view*
        // moves. Getting this backwards would turn pages the wrong way, which is why the
        // sign is pinned down rather than assumed.
        let wheel = read(vec![wheel_event(
            egui::MouseWheelUnit::Line,
            egui::vec2(0.0, -1.0),
        )]);
        assert!(wheel.notched > 0.0, "scrolling down read as {wheel:?}");
        assert_eq!(wheel.glided, 0.0);
    }

    #[test]
    fn a_small_trackpad_movement_reads_as_a_glide() {
        let wheel = read(vec![wheel_event(
            egui::MouseWheelUnit::Point,
            egui::vec2(0.0, -3.0),
        )]);
        assert!(wheel.glided > 0.0, "a trackpad swipe read as {wheel:?}");
        assert_eq!(wheel.notched, 0.0);
    }

    #[test]
    fn a_large_movement_in_points_is_a_notch_not_a_glide() {
        // Some mice report in points. egui treats a large movement as a notch for the
        // purpose of smoothing, and this reuses that test so the two agree about the
        // device rather than each guessing.
        let wheel = read(vec![wheel_event(
            egui::MouseWheelUnit::Point,
            egui::vec2(0.0, -50.0),
        )]);
        assert!(wheel.notched > 0.0, "a large point delta read as {wheel:?}");
        assert_eq!(wheel.glided, 0.0);
    }

    #[test]
    fn ctrl_and_shift_wheel_are_not_page_turns() {
        // Ctrl+wheel is a zoom, which `handle_input` acts on through `zoom_delta`. Counted
        // here as well it would zoom *and* turn the page. Shift+wheel is horizontal
        // scrolling, and a single-page view has nowhere sideways to turn.
        for modifiers in [egui::Modifiers::CTRL, egui::Modifiers::SHIFT] {
            let wheel = read(vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -1.0),
                phase: egui::TouchPhase::Move,
                modifiers,
            }]);
            assert_eq!(wheel, Wheel::default(), "{modifiers:?} read as {wheel:?}");
        }
    }
}
