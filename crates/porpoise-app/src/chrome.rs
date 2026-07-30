//! Everything drawn around the pages: the toolbar, the status bar, and two overlays.
//!
//! Split out of `viewer.rs` because it had grown to 1300 lines of code with no tests
//! while every other large file in the workspace is mostly tests. These four pieces were
//! about 280 of those lines and needed none of the frame loop.
//!
//! Be clear about what the split buys: **navigability, not coverage.** Every function here
//! needs a live `egui::Context`, so none of them gains a unit test by moving. What *is*
//! newly tested is the part worth testing — which edits are possible — and that went to
//! [`crate::edits`] rather than here.
//!
//! # Nothing here decides anything
//!
//! Each function is handed a decision made elsewhere and paints it. The toolbar is given
//! [`Edits`]; the question box is given the sentence [`crate::confirm::Intent`] wrote; the
//! drop hint is given the [`DropAction`] that the drop itself will use. That last one is
//! load-bearing rather than tidy: the hint and the drop come from one decision, so the
//! window cannot invite something it then refuses.
//!
//! # Every control produces a command
//!
//! There is no click-only path. Each control returns the same [`Command`] an agent would
//! send, collected and handed back rather than dispatched here, because dispatch needs
//! `&mut Viewer` while `ui` holds the borrow. The one exception is **Open…**, which asks
//! a person a question rather than having an effect, so it comes back as its own flag —
//! see [`crate::picker`] for why the dialog is not a command.

use std::path::Path;

use eframe::egui;
use porpoise_view::{ScrollMode, ViewCommand, ZoomTarget};

use crate::command::Command;
use crate::confirm::Answer;
use crate::devtools::FrameTiming;
use crate::edits::Edits;
use crate::input::DropAction;
use crate::label::file_label;

/// Point size of the text in the two overlays.
const OVERLAY_TEXT_PT: f32 = 20.0;

/// What the toolbar reads.
pub(crate) struct Toolbar<'a> {
    /// Which page edits are possible, and what each produces. One source for the
    /// enabled state *and* the command, so a lit button cannot mean something a key
    /// press does not. See [`crate::edits`].
    pub(crate) edits: &'a Edits,
    pub(crate) zoom_target: ZoomTarget,
    pub(crate) scroll_mode: ScrollMode,
    /// Whether the page grid is showing, for the toggle's pressed look.
    pub(crate) thumbnails: bool,
    /// Whether a file dialog is already up, so a second click cannot stack one.
    pub(crate) picker_open: bool,
}

/// What the toolbar was asked for this frame.
pub(crate) struct Clicked {
    /// Commands to dispatch, in the order they were clicked.
    pub(crate) commands: Vec<Command>,
    /// The **Open…** button. Not a command; see the module docs.
    pub(crate) open_picker: bool,
}

/// What the status bar reads.
///
/// A wide struct because the status bar is a wide row — every field is one thing on
/// screen. Plain values rather than a borrow of the viewer, the same shape as
/// [`crate::thumbnails::Grid`], so this module never learns what a `Viewer` is.
pub(crate) struct Status<'a> {
    /// `None` with no document, which is a different row entirely.
    pub(crate) document: Option<StatusDocument>,
    /// Where a save in flight is going.
    pub(crate) saving_to: Option<&'a Path>,
    /// Whether the order differs from the file.
    pub(crate) unsaved_changes: bool,
    /// Why the last command failed. Shown on every path, including with no document,
    /// because a failed open is exactly the case that leaves the window empty.
    pub(crate) last_error: Option<&'a str>,
}

/// The half of the status bar that only exists when something is open.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatusDocument {
    /// Counting from 1, as everywhere a person can see it.
    pub(crate) current_page: usize,
    pub(crate) page_count: usize,
    pub(crate) zoom: f32,
    pub(crate) zoom_target: ZoomTarget,
    pub(crate) scroll_mode: ScrollMode,
    pub(crate) pages_cached: usize,
    pub(crate) cache_bytes: usize,
    pub(crate) workers: usize,
    pub(crate) renders_in_flight: usize,
    pub(crate) timing: FrameTiming,
    /// Rasterizations given up on. A page still being retried is not one yet.
    pub(crate) abandoned: usize,
}

/// Draws the toolbar and reports what was clicked.
pub(crate) fn toolbar(ui: &mut egui::Ui, state: &Toolbar<'_>) -> Clicked {
    let mut commands = Vec::new();
    let mut open_picker = false;

    ui.horizontal(|ui| {
        if ui
            .add_enabled(!state.picker_open, egui::Button::new("Open…"))
            .on_hover_text("Open a PDF (Ctrl+O), or drag one onto the window")
            .clicked()
        {
            open_picker = true;
        }
        ui.separator();

        if ui
            .selectable_label(state.thumbnails, "Pages")
            .on_hover_text("Show the page grid, to jump to a page or drag pages around (Ctrl+T)")
            .clicked()
        {
            commands.push(state.edits.toggle_thumbnails.clone());
        }
        ui.separator();

        if ui.button("⏮").on_hover_text("First page (Home)").clicked() {
            commands.push(ViewCommand::FirstPage.into());
        }
        if ui.button("⏭").on_hover_text("Last page (End)").clicked() {
            commands.push(ViewCommand::LastPage.into());
        }
        ui.separator();

        // Page editing. Words rather than arrow glyphs: U+2191/U+2193 are missing from
        // egui's bundled fonts and rendered as empty boxes — caught by looking at a
        // capture of the real toolbar rather than by any test.
        edit_button(
            ui,
            &mut commands,
            "Up",
            "Move this page earlier (Ctrl+Up)",
            state.edits.move_earlier.as_ref(),
        );
        edit_button(
            ui,
            &mut commands,
            "Down",
            "Move this page later (Ctrl+Down)",
            state.edits.move_later.as_ref(),
        );
        edit_button(
            ui,
            &mut commands,
            "Delete",
            "Delete this page",
            state.edits.delete.as_ref(),
        );
        edit_button(
            ui,
            &mut commands,
            "Undo",
            "Undo the last page edit (Ctrl+Z)",
            state.edits.undo.as_ref(),
        );
        edit_button(
            ui,
            &mut commands,
            "Save",
            "Write the changes over the file (Ctrl+S)",
            state.edits.save.as_ref(),
        );
        ui.separator();

        if ui.button("−").on_hover_text("Zoom out (Ctrl+-)").clicked() {
            commands.push(ViewCommand::StepZoom { rungs: -1 }.into());
        }
        if ui.button("+").on_hover_text("Zoom in (Ctrl++)").clicked() {
            commands.push(ViewCommand::StepZoom { rungs: 1 }.into());
        }
        if ui
            .selectable_label(state.zoom_target == ZoomTarget::FitWidth, "Width")
            .on_hover_text("Fit width (Ctrl+0)")
            .clicked()
        {
            commands.push(
                ViewCommand::SetZoom {
                    target: ZoomTarget::FitWidth,
                }
                .into(),
            );
        }
        if ui
            .selectable_label(state.zoom_target == ZoomTarget::FitPage, "Page")
            .on_hover_text("Fit page (Ctrl+2)")
            .clicked()
        {
            commands.push(
                ViewCommand::SetZoom {
                    target: ZoomTarget::FitPage,
                }
                .into(),
            );
        }
        ui.separator();

        // Paged versus free changes what PageDown and Space mean.
        let paged = state.scroll_mode == ScrollMode::Paged;
        if ui
            .selectable_label(paged, "Paged")
            .on_hover_text("Page-by-page instead of continuous scrolling")
            .clicked()
        {
            let mode = if paged {
                ScrollMode::Free
            } else {
                ScrollMode::Paged
            };
            commands.push(ViewCommand::SetScrollMode { mode }.into());
        }
    });

    Clicked {
        commands,
        open_picker,
    }
}

/// One page-edit button, enabled exactly when there is a command for it.
///
/// The whole point of taking the command as the enabled state: a lit button and a live
/// key binding cannot disagree, because they are the same `Option`.
fn edit_button(
    ui: &mut egui::Ui,
    commands: &mut Vec<Command>,
    text: &str,
    hover: &str,
    command: Option<&Command>,
) {
    if ui
        .add_enabled(command.is_some(), egui::Button::new(text))
        .on_hover_text(hover)
        .clicked()
        && let Some(command) = command
    {
        commands.push(command.clone());
    }
}

/// Draws the status bar.
pub(crate) fn status(ui: &mut egui::Ui, state: &Status<'_>) {
    ui.horizontal(|ui| {
        match &state.document {
            Some(open) => {
                ui.label(format!("page {} of {}", open.current_page, open.page_count));
                ui.separator();
                ui.label(format!(
                    "{:.0}% {}",
                    open.zoom * 100.0,
                    open.zoom_target.label()
                ));
                ui.separator();
                ui.label(open.scroll_mode.label());
                ui.separator();
                // Proof of virtualization: both stay small however long the document.
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a megabyte count for a person to read"
                )]
                let megabytes = open.cache_bytes as f64 / (1024.0 * 1024.0);
                ui.label(format!("{} cached, {megabytes:.1} MB", open.pages_cached));
                ui.separator();
                ui.label(format!(
                    "{} workers, {} in flight",
                    open.workers, open.renders_in_flight
                ));
                ui.separator();
                ui.label(format!(
                    "ui {:.1} ms, frame {:.1} ms",
                    open.timing.ui_ms, open.timing.frame_ms
                ));
                if open.abandoned > 0 {
                    ui.separator();
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("{} failed", open.abandoned),
                    );
                }
            }
            None => {
                ui.label("no document — Ctrl+O to open one, or drag one in");
            }
        }

        // Editing state, before the error, so a save failure reads next to it.
        if let Some(destination) = state.saving_to {
            ui.separator();
            ui.label(format!("saving to {}…", file_label(destination)));
        } else if state.unsaved_changes {
            ui.separator();
            ui.colored_label(ui.visuals().warn_fg_color, "unsaved changes");
        }

        // Last, and on every path: a failure with no document open is exactly the case
        // the picker and a refused drop create, so it must not live inside the `Some`
        // arm.
        if let Some(error) = state.last_error {
            ui.separator();
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    });
}

/// Asks about unsaved page changes. Returns an answer only when one was given.
///
/// A [`egui::Modal`], so it dims the window and takes the keyboard — but it deliberately
/// does **not** block the control channel, which is serviced outside the frame's UI. An
/// agent can therefore still read, still answer, and still change the order underneath it.
pub(crate) fn question(ctx: &egui::Context, what: &str) -> Option<Answer> {
    let mut choice = None;
    let modal = egui::Modal::new(egui::Id::new("porpoise-unsaved")).show(ctx, |ui| {
        ui.set_min_width(380.0);
        ui.heading("Unsaved page changes");
        ui.add_space(6.0);
        ui.label(format!(
            "The pages have been reordered and not written to the file. \
             Continuing to {what} will lose those changes."
        ));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            // Save first, because it is the answer that loses nothing.
            if ui
                .button("Save, then continue")
                .on_hover_text("Write the changes over the file, then go ahead")
                .clicked()
            {
                choice = Some(Answer::Save);
            }
            if ui
                .button("Discard changes")
                .on_hover_text("Go ahead and lose the reordering")
                .clicked()
            {
                choice = Some(Answer::Discard);
            }
            if ui.button("Cancel").clicked() {
                choice = Some(Answer::Cancel);
            }
        });
    });

    // Escape, or clicking outside the box. Treated as Cancel because that is the answer
    // that changes nothing — a dismissal must never be the destructive one.
    if choice.is_none() && modal.should_close() {
        choice = Some(Answer::Cancel);
    }
    choice
}

/// Paints what letting go of a file drag would do.
///
/// `unsaved_changes` only reaches [`DropAction::hint`], which decides whether to mention
/// it — so this function has no opinion about what the sentence says, which is the point.
pub(crate) fn drop_hint(ctx: &egui::Context, action: &DropAction, unsaved_changes: bool) {
    let colour = match action {
        DropAction::Open { .. } => egui::Color32::WHITE,
        DropAction::Refuse { .. } => egui::Color32::from_rgb(240, 150, 150),
    };

    // Its own foreground layer, so the hint covers the page column, the toolbar and the
    // thumbnail panel rather than being drawn under whichever came last.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("porpoise-drop-hint"),
    ));
    // Dim the whole viewport so nothing shows through at the edges, but centre the card in
    // the content rect, which excludes any OS status bar or notch.
    painter.rect_filled(
        ctx.viewport_rect(),
        0.0,
        egui::Color32::from_black_alpha(160),
    );

    // Laid out first so the card can be sized to the text. Dimming alone is not enough to
    // read a sentence over a drawing sheet, which is mostly white and full of lines.
    // Wrapped rather than measured freely, so a long file name cannot push the card off
    // both edges of the window.
    let content = ctx.content_rect();
    let galley = painter.layout(
        action.hint(unsaved_changes),
        egui::FontId::proportional(OVERLAY_TEXT_PT),
        colour,
        (content.width() - 96.0).max(120.0),
    );
    let padding = egui::vec2(36.0, 22.0);
    let card = egui::Rect::from_center_size(content.center(), galley.size() + padding);
    painter.rect_filled(card, 8.0, egui::Color32::from_black_alpha(235));
    painter.galley(card.center() - galley.size() * 0.5, galley, colour);
}
