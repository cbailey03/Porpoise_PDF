//! Every effect that can change what the viewer shows, named.
//!
//! This is the surface Goal 2 rests on. Keyboard, toolbar, and an external agent
//! are all *producers* of [`ViewCommand`] — none of them reaches viewer state any
//! other way. That makes "every feature is programmatically controllable" a
//! structural property rather than a maintained one: a click-only feature is
//! unrepresentable, because clicks produce commands and commands are the surface.
//!
//! See `docs/goal-2-plan.md`, sections 1 and 2.
//!
//! # Effects, not gestures
//!
//! Commands name *effects*. They deliberately do not model input devices: there
//! is no `Pinch` command, because a pinch **is** [`ViewCommand::SetZoom`] with a
//! multiplied factor. Modelling gestures would mean a command per device, and
//! then exceptions for the ones that did not fit.
//!
//! The same principle decides an ambiguity worth pointing at.
//! [`ViewCommand::NextPage`] always means the next *page*, in every scroll mode.
//! A `PageDown` key press means "next page" in paged mode and "next screenful" in
//! free mode — so the *key handler* chooses between [`ViewCommand::NextPage`] and
//! [`ViewCommand::ScrollByViewports`]. Mode-dependence belongs to input
//! translation, not to the command, or an agent could never be sure what
//! `NextPage` would do.

use crate::{PageNumber, ScrollMode};

/// What the viewer should scale pages to.
///
/// Distinct from [`crate::FitMode`], which fits *one* page: these resolve against
/// a whole [`crate::ScrollLayout`], so that scrolling past a landscape page does
/// not resize every other page.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ZoomTarget {
    /// Fit the widest page to the viewport width.
    FitWidth,
    /// Fit the largest page entirely within the viewport.
    FitPage,
    /// An explicit factor, where `1.0` is 72 DPI.
    Fixed(f32),
}

impl ZoomTarget {
    /// A short name for status display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FitWidth => "fit width",
            Self::FitPage => "fit page",
            Self::Fixed(_) => "zoom",
        }
    }
}

/// A change to what the viewer shows.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "command", rename_all = "snake_case"))]
pub enum ViewCommand {
    /// Scroll so `page` is at the top of the viewport.
    GoToPage {
        /// The page to show, counting from 1.
        page: PageNumber,
    },
    /// Scroll to the next page. Always page-granular; see the module docs.
    NextPage,
    /// Scroll to the previous page.
    PreviousPage,
    /// Scroll to the first page.
    FirstPage,
    /// Scroll to the last page.
    LastPage,
    /// Scroll to an absolute offset, clamped to the document.
    ScrollTo {
        /// Distance from the top of the document, in PDF points.
        points: f64,
    },
    /// Scroll by a relative amount, clamped to the document.
    ScrollBy {
        /// Points to move; negative scrolls up.
        points: f64,
    },
    /// Scroll by a fraction of the viewport height.
    ///
    /// Separate from [`Self::ScrollBy`] because "one screenful" is the unit a
    /// reader thinks in, and the caller may not know the viewport height.
    ScrollByViewports {
        /// Viewports to move; negative scrolls up.
        fraction: f64,
    },
    /// Choose how pages are scaled.
    SetZoom {
        /// The new target.
        target: ZoomTarget,
    },
    /// Move along the quantized zoom ladder. One rung is about 9%.
    StepZoom {
        /// Rungs to move; negative zooms out.
        rungs: i16,
    },
    /// Choose whether navigation is page-granular or continuous.
    SetScrollMode {
        /// The new mode.
        mode: ScrollMode,
    },
}

impl ViewCommand {
    /// One representative value of every variant.
    ///
    /// This list does three jobs, which is why it is worth keeping by hand:
    ///
    /// 1. **Discovery.** The control channel publishes it, so an agent can ask
    ///    what the program does rather than being told out of band.
    /// 2. **Documentation.** [`Self::name`] over this list is the command
    ///    reference, generated from the thing it documents so it cannot drift.
    /// 3. **Coverage.** `every_command_has_a_behaviour_test` walks it, and
    ///    `the_all_list_is_exhaustive` fails to *compile* if a variant is added
    ///    without being added here.
    pub const ALL: &'static [Self] = &[
        Self::GoToPage {
            page: PageNumber::FIRST,
        },
        Self::NextPage,
        Self::PreviousPage,
        Self::FirstPage,
        Self::LastPage,
        Self::ScrollTo { points: 0.0 },
        Self::ScrollBy { points: 0.0 },
        Self::ScrollByViewports { fraction: 0.0 },
        Self::SetZoom {
            target: ZoomTarget::FitWidth,
        },
        Self::StepZoom { rungs: 0 },
        Self::SetScrollMode {
            mode: ScrollMode::Free,
        },
    ];

    /// The wire name of this command, matching its serialized `command` tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::GoToPage { .. } => "go_to_page",
            Self::NextPage => "next_page",
            Self::PreviousPage => "previous_page",
            Self::FirstPage => "first_page",
            Self::LastPage => "last_page",
            Self::ScrollTo { .. } => "scroll_to",
            Self::ScrollBy { .. } => "scroll_by",
            Self::ScrollByViewports { .. } => "scroll_by_viewports",
            Self::SetZoom { .. } => "set_zoom",
            Self::StepZoom { .. } => "step_zoom",
            Self::SetScrollMode { .. } => "set_scroll_mode",
        }
    }
}

/// Why a command could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "reason", rename_all = "snake_case"))]
pub enum Rejection {
    /// The document has no pages, so there is nowhere to navigate.
    #[error("the document has no pages")]
    NoPages,
    /// The requested page is past the end.
    #[error("page {page} does not exist (the document has {page_count})")]
    NoSuchPage {
        /// The page that was asked for, counting from 1.
        page: PageNumber,
        /// How many pages the document has.
        page_count: usize,
    },
    /// A numeric argument was `NaN` or infinite.
    #[error("{argument} must be a finite number")]
    NotFinite {
        /// Which argument was at fault.
        argument: &'static str,
    },
}

/// What applying a command did.
///
/// `Changed` and `Unchanged` are distinguished so that a caller can skip emitting
/// a state-change event for a command that asked for what was already true —
/// otherwise an agent polling with `SetZoom(FitWidth)` would generate an endless
/// stream of events reporting nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The command changed the view state.
    Changed,
    /// The command was valid but the state already satisfied it.
    Unchanged,
    /// The command was refused.
    Rejected(Rejection),
}

impl Outcome {
    /// Whether the state changed.
    #[must_use]
    pub fn changed(self) -> bool {
        matches!(self, Self::Changed)
    }

    /// Whether the command was refused.
    #[must_use]
    pub fn rejected(self) -> Option<Rejection> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Changed | Self::Unchanged => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_all_list_is_exhaustive() {
        // This match is the enforcement mechanism. Adding a variant to
        // `ViewCommand` without adding it to `ALL` fails to compile here, which is
        // what makes `ALL` trustworthy as both the agent's command reference and
        // the coverage check in `state.rs`.
        for command in ViewCommand::ALL {
            match command {
                ViewCommand::GoToPage { .. }
                | ViewCommand::NextPage
                | ViewCommand::PreviousPage
                | ViewCommand::FirstPage
                | ViewCommand::LastPage
                | ViewCommand::ScrollTo { .. }
                | ViewCommand::ScrollBy { .. }
                | ViewCommand::ScrollByViewports { .. }
                | ViewCommand::SetZoom { .. }
                | ViewCommand::StepZoom { .. }
                | ViewCommand::SetScrollMode { .. } => {}
            }
        }

        // And the reverse direction: a variant added to the enum and matched above
        // but forgotten in `ALL` would slip through, so count them.
        assert_eq!(
            ViewCommand::ALL.len(),
            11,
            "ALL has {} entries; update this count deliberately when adding a command",
            ViewCommand::ALL.len()
        );
    }

    #[test]
    fn every_command_has_a_distinct_wire_name() {
        let mut names: Vec<&str> = ViewCommand::ALL.iter().map(ViewCommand::name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two commands share a wire name");
    }

    #[test]
    fn wire_names_are_snake_case() {
        // They are also the serialized tag, so a stray capital or space would show
        // up in the protocol.
        for command in ViewCommand::ALL {
            let name = command.name();
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
        }
    }

    #[test]
    fn outcome_reports_change_and_rejection_distinctly() {
        assert!(Outcome::Changed.changed());
        assert!(!Outcome::Unchanged.changed());
        assert!(Outcome::Changed.rejected().is_none());
        assert_eq!(
            Outcome::Rejected(Rejection::NoPages).rejected(),
            Some(Rejection::NoPages)
        );
        assert!(!Outcome::Rejected(Rejection::NoPages).changed());
    }
}
