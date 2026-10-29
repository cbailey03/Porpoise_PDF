//! Running the viewer with no window: `porpoise serve --headless`.
//!
//! A window gives the viewer two things: frames, and somewhere to put them. Headless
//! mode supplies the frames itself, from a bare `egui::Context` on a fixed tick, and
//! drives the same [`crate::viewer`] frame body eframe does. Every command still goes
//! through `Viewer::dispatch`, so an agent driving a headless process is exercising the
//! same code a person at a window is.
//!
//! What a window does with the viewer's output has to be copied, not approximated. The
//! viewer sends three window commands - `Close`, `CancelClose`, and `Screenshot` - and
//! the unsaved-changes guard depends on exactly how eframe answers the first two. That
//! answer lives in [`after_frame`], pure so it can be tested without a context.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, ViewportCommand, ViewportId};
use eframe::egui_wgpu::wgpu;

use crate::offscreen::Offscreen;
use crate::viewer::{Viewer, ViewerOptions};

/// The screen a headless process draws to, in points at one pixel per point.
///
/// The window's default size in `viewer::run`, so a snapshot's geometry reads the same
/// either way and a test written against one mode holds in the other.
const SCREEN: egui::Vec2 = egui::vec2(1024.0, 768.0);

/// Time between frames: 60 Hz, fixed.
///
/// Not chosen for smoothness - nobody is watching. Some things count frames rather than
/// seconds (the capture warmup and its 240-frame budget, in `devtools`), so a loop that
/// ran flat out would make them fire early, and would burn a core while an agent
/// thinks. At 60 Hz a frame takes the same real time as on a typical display, which is
/// what those frame counts were written against.
///
/// Measured idle, a debug build with a document open and nothing being asked of it:
/// 3 to 4.5% of one core in `top` over 4-second samples.
const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// The reason `capture` gives when this process cannot render one.
fn capture_unavailable(reason: &str) -> String {
    format!("capture needs a GPU or software renderer, and none was found: {reason}")
}

/// Runs the viewer with no window. Blocks until the program exits.
///
/// The headless counterpart of `viewer::run`, taking the same options. Commands arrive
/// only over the control channel, so `serve --headless` is the only way in.
pub(crate) fn run(mut options: ViewerOptions) -> Result<(), Box<dyn std::error::Error>> {
    // Set up now rather than at the first capture: the renderer has to see every
    // texture upload from the first frame on, or it would be missing the font atlas
    // and every page drawn before it existed.
    let mut offscreen = match Offscreen::new() {
        Ok(offscreen) => Some(offscreen),
        Err(reason) => {
            tracing::warn!("headless: capture is unavailable: {reason}");
            options.capture_unavailable = Some(capture_unavailable(&reason));
            None
        }
    };
    // The limit eframe would have read off the GPU. Without it egui assumes 2048 and
    // debug-asserts on page textures a window accepts. With no device, wgpu's default
    // is the safe promise: nothing is uploaded anywhere to break it.
    let max_texture_side = offscreen.as_ref().map_or_else(
        || wgpu::Limits::default().max_texture_dimension_2d as usize,
        Offscreen::max_texture_side,
    );

    let ctx = egui::Context::default();
    let mut viewer = Viewer::new(options, Arc::new(Mutex::new(None)));
    let started = Instant::now();
    let mut close_next_frame = false;
    let mut events = Vec::new();

    loop {
        let tick = Instant::now();
        let input = frame_input(
            started.elapsed(),
            max_texture_side,
            close_next_frame,
            std::mem::take(&mut events),
        );
        let close_requested = input.viewport().close_requested();
        let output = ctx.run_ui(input, |ui| {
            viewer.frame_logic(ui.ctx());
            viewer.frame_ui(ui);
        });

        let commands = output
            .viewport_output
            .get(&ViewportId::ROOT)
            .map_or(&[][..], |viewport| viewport.commands.as_slice());
        for command in commands {
            if !matches!(
                command,
                ViewportCommand::Close
                    | ViewportCommand::CancelClose
                    | ViewportCommand::Screenshot(_)
            ) {
                tracing::debug!("headless: no window to apply {command:?} to");
            }
        }

        let next = after_frame(close_requested, commands);
        if next.exit {
            return Ok(());
        }
        close_next_frame = next.close_next_frame;

        if let Some(offscreen) = &mut offscreen {
            // Set, draw, free: the order a window's painter applies a frame's textures in.
            offscreen.set_textures(&output.textures_delta);
            if !next.screenshots.is_empty() {
                let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
                let size = (SCREEN * output.pixels_per_point).round();
                match offscreen.capture(
                    &primitives,
                    [size.x as u32, size.y as u32],
                    output.pixels_per_point,
                ) {
                    // Delivered next frame, as eframe delivers a window's.
                    Ok(image) => {
                        let image = Arc::new(image);
                        events.extend(next.screenshots.into_iter().map(|user_data| {
                            egui::Event::Screenshot {
                                viewport_id: ViewportId::ROOT,
                                user_data,
                                image: Arc::clone(&image),
                            }
                        }));
                    }
                    // Nothing is delivered, so the viewer's capture runs out its frame
                    // budget and fails through the path every capture failure takes.
                    Err(reason) => tracing::error!("headless: capture failed: {reason}"),
                }
            }
            offscreen.free_textures(&output.textures_delta);
        } else if !next.screenshots.is_empty() {
            // Unreachable while `capture_unavailable` is set: the viewer refuses the
            // capture before it can ask for a screenshot.
            tracing::warn!("headless: a screenshot was asked for with no renderer");
        }

        std::thread::sleep(FRAME_INTERVAL.saturating_sub(tick.elapsed()));
    }
}

/// One frame's input: a focused screen of [`SCREEN`], plus whatever the last frame queued.
fn frame_input(
    since_start: Duration,
    max_texture_side: usize,
    close_requested: bool,
    events: Vec<egui::Event>,
) -> egui::RawInput {
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, SCREEN);
    let mut input = egui::RawInput {
        screen_rect: Some(screen),
        max_texture_side: Some(max_texture_side),
        time: Some(since_start.as_secs_f64()),
        predicted_dt: FRAME_INTERVAL.as_secs_f32(),
        focused: true,
        events,
        ..egui::RawInput::default()
    };
    let viewport = input.viewports.entry(ViewportId::ROOT).or_default();
    viewport.native_pixels_per_point = Some(1.0);
    viewport.inner_rect = Some(screen);
    viewport.focused = Some(true);
    if close_requested {
        viewport.events.push(egui::ViewportEvent::Close);
    }
    input
}

/// What the frame loop does next, given what a frame asked of its window.
#[derive(Debug, PartialEq)]
struct AfterFrame {
    /// The program ends now.
    exit: bool,
    /// The next frame's input carries a close request.
    close_next_frame: bool,
    /// Screenshots to take of this frame, each answered with its own user data.
    screenshots: Vec<egui::UserData>,
}

/// Answers a frame's window commands the way eframe 0.35 does.
///
/// `close_requested` is whether this frame's *input* carried a close request, read
/// before the frame ran. Copied from two places, checked in their source rather than
/// inferred: eframe's `EpiIntegration::update` closes when a frame that was asked to
/// close did not send `CancelClose`, and egui-winit's `process_viewport_command` turns
/// a `Close` command into a close request on the next frame rather than closing.
///
/// Any other command is ignored here; the caller logs it. The viewer sends none today.
fn after_frame(close_requested: bool, commands: &[ViewportCommand]) -> AfterFrame {
    let exit = close_requested && !commands.contains(&ViewportCommand::CancelClose);
    if exit {
        return AfterFrame {
            exit,
            close_next_frame: false,
            screenshots: Vec::new(),
        };
    }

    let mut close_next_frame = false;
    let mut screenshots = Vec::new();
    for command in commands {
        match command {
            ViewportCommand::Close => close_next_frame = true,
            ViewportCommand::Screenshot(user_data) => screenshots.push(user_data.clone()),
            _ => {}
        }
    }
    AfterFrame {
        exit,
        close_next_frame,
        screenshots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_request_with_no_cancel_exits() {
        assert!(after_frame(true, &[]).exit);
    }

    #[test]
    fn a_cancelled_close_keeps_running() {
        let next = after_frame(true, &[ViewportCommand::CancelClose]);
        assert!(!next.exit);
        assert!(!next.close_next_frame);
    }

    #[test]
    fn a_close_command_becomes_next_frames_request() {
        // egui-winit does not close on `Close`: it queues a close *request*, which the
        // viewer's guard then gets a chance to cancel. Closing here would skip the guard.
        let next = after_frame(false, &[ViewportCommand::Close]);
        assert!(!next.exit);
        assert!(next.close_next_frame);
    }

    #[test]
    fn screenshots_keep_their_user_data_in_order() {
        // `UserData` compares by pointer, so the expected values have to be the same
        // allocations that went in, not fresh ones built from the same bytes.
        let first = egui::UserData::new(1u8);
        let second = egui::UserData::new(2u8);
        let next = after_frame(
            false,
            &[
                ViewportCommand::Screenshot(first.clone()),
                ViewportCommand::Screenshot(second.clone()),
            ],
        );
        assert_eq!(next.screenshots, vec![first, second]);
    }

    #[test]
    fn a_close_that_goes_through_wins_over_a_screenshot() {
        // Taking the picture would mean delivering it to a frame that never runs.
        let next = after_frame(
            true,
            &[ViewportCommand::Screenshot(egui::UserData::default())],
        );
        assert!(next.exit);
        assert!(next.screenshots.is_empty());
    }

    #[test]
    fn other_commands_are_ignored() {
        let next = after_frame(false, &[ViewportCommand::Title("x".into())]);
        assert_eq!(
            next,
            AfterFrame {
                exit: false,
                close_next_frame: false,
                screenshots: Vec::new(),
            }
        );
    }

    #[test]
    fn cancel_without_a_request_does_nothing() {
        let next = after_frame(false, &[ViewportCommand::CancelClose]);
        assert_eq!(
            next,
            AfterFrame {
                exit: false,
                close_next_frame: false,
                screenshots: Vec::new(),
            }
        );
    }
}
