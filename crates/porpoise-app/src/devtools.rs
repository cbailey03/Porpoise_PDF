//! Development aids: scripted scrolling for frame-time measurement, and window
//! capture for headless verification.
//!
//! None of this is part of the viewer. It lives in its own module because the
//! alternative — which is what this started as — is a frame loop where the
//! measurement apparatus is interleaved with the thing being measured, and a
//! `Viewer` where a quarter of the fields exist only for instrumentation. Neither
//! reads well, and neither can be tested without opening a window.
//!
//! Everything here is reached through the hidden flags in `main.rs`. The
//! [`ScrollBenchmark`] accounting and the [`percentiles`] arithmetic are pure and
//! tested; only the parts that genuinely need an `egui::Context` are not.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui;
use porpoise_render::RenderedPage;

/// Frames the scroll benchmark discards before it starts recording.
///
/// Window creation, GPU device setup and font loading all land in the first
/// frames and are not scrolling costs. Including them puts a ~150 ms outlier in
/// the maximum and makes the numbers useless for judging smoothness.
const WARMUP_FRAMES: u32 = 60;

/// What one frame cost, as measured by the viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameTiming {
    /// Time spent in our own `ui`.
    pub(crate) ui_ms: f32,
    /// Time spent in `logic`, which is where finished pages are uploaded to the
    /// GPU. Measured separately because uploads must happen on the UI thread and
    /// are the one part of the pipeline that cannot be moved off it.
    pub(crate) logic_ms: f32,
    /// Interval since the previous frame, which is what the user perceives.
    pub(crate) frame_ms: f32,
}

/// A scripted scroll used to measure frame times.
///
/// Goal 1 asks for sustained 60 fps while scrolling, and every other check in
/// this project is a static capture — which proves geometry and caching but says
/// nothing about behaviour under motion. This drives the scroll from code so the
/// claim can actually be measured.
pub(crate) struct ScrollBenchmark {
    frames_left: u32,
    warmup_left: u32,
    step_pt: f64,
    ui_ms: Vec<f32>,
    logic_ms: Vec<f32>,
    frame_ms: Vec<f32>,
    warmup_worst_ms: f32,
}

impl ScrollBenchmark {
    /// Spreads `frames` of scrolling across the whole document, so the benchmark
    /// exercises every page rather than thrashing one spot.
    pub(crate) fn new(frames: u32, content_height_pt: f64) -> Self {
        let frames = frames.max(1);
        Self {
            frames_left: frames,
            warmup_left: WARMUP_FRAMES,
            step_pt: content_height_pt / f64::from(frames),
            ui_ms: Vec::with_capacity(frames as usize),
            logic_ms: Vec::with_capacity(frames as usize),
            frame_ms: Vec::with_capacity(frames as usize),
            warmup_worst_ms: 0.0,
        }
    }

    /// Records one frame, or discards it if still warming up.
    pub(crate) fn record(&mut self, timing: FrameTiming) {
        if self.warmup_left > 0 {
            self.warmup_left -= 1;
            self.warmup_worst_ms = self.warmup_worst_ms.max(timing.frame_ms);
            return;
        }
        self.ui_ms.push(timing.ui_ms);
        self.logic_ms.push(timing.logic_ms);
        self.frame_ms.push(timing.frame_ms);
        self.frames_left = self.frames_left.saturating_sub(1);
    }

    /// Whether the scripted scroll has run its course.
    pub(crate) fn is_finished(&self) -> bool {
        self.frames_left == 0
    }

    /// Points to advance per frame.
    pub(crate) fn step_pt(&self) -> f64 {
        self.step_pt
    }

    /// How many frames were actually measured, warmup excluded.
    ///
    /// Only the tests need this — the report reads the sample vectors directly —
    /// so it is gated rather than left as a dead accessor in the shipped binary.
    #[cfg(test)]
    pub(crate) fn measured_frames(&self) -> usize {
        self.frame_ms.len()
    }

    /// Prints the percentile summary to stdout.
    pub(crate) fn report(&self) {
        println!("frames measured: {}", self.frame_ms.len());
        print_percentiles("logic time (incl. GPU upload)", &self.logic_ms);
        print_percentiles("ui time", &self.ui_ms);
        print_percentiles("frame interval", &self.frame_ms);
        println!(
            "  (discarded warmup: worst frame interval {:.2} ms — window and GPU setup)",
            self.warmup_worst_ms
        );
    }
}

/// The distribution of a set of frame-time samples.
///
/// Percentiles rather than a mean, because a mean hides exactly the thing that
/// makes scrolling feel bad: a handful of very long frames among many short ones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Percentiles {
    pub(crate) p50: f32,
    pub(crate) p95: f32,
    pub(crate) p99: f32,
    pub(crate) max: f32,
}

/// Summarizes `samples`, or `None` if there are none.
///
/// Uses nearest-rank on a sorted copy — no interpolation, so every reported value
/// is a frame that actually happened.
pub(crate) fn percentiles(samples: &[f32]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    // `total_cmp` rather than `partial_cmp`: a NaN sample must not silently
    // corrupt the ordering, and one bad frame time should not poison the report.
    sorted.sort_by(f32::total_cmp);

    let at = |fraction: f64| -> f32 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "index into a bounded sample vector"
        )]
        let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
        sorted.get(index).copied().unwrap_or(0.0)
    };

    Some(Percentiles {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: at(1.0),
    })
}

fn print_percentiles(label: &str, samples: &[f32]) {
    match percentiles(samples) {
        Some(spread) => println!(
            "  {label}: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
            spread.p50, spread.p95, spread.p99, spread.max
        ),
        None => println!("  {label}: no samples"),
    }
}

/// A request to capture the window.
///
/// This exists because a native window cannot be inspected from a headless
/// context, so without it "the window works" would be an untested claim.
pub(crate) struct ScreenshotRequest {
    /// Where to write the PNG.
    pub(crate) path: PathBuf,
    /// Frames to draw before asking, so real content is on screen first.
    pub(crate) warmup_frames: u32,
    /// Frames after which the attempt is abandoned.
    pub(crate) budget_frames: u32,
    /// Whether resolving the capture should also close the window.
    ///
    /// True for the one-shot CLI `--screenshot` flag, where capture-then-exit is
    /// the entire point and a stranded window would hang the command. False for a
    /// `capture` command, where the controlling process expects to carry on
    /// driving afterwards.
    pub(crate) exit_when_done: bool,
}

/// What the screenshot attempt produced, shared with the caller because
/// `run_native` gives us no other way to report it.
pub(crate) type ScreenshotOutcome = Arc<Mutex<Option<Result<PathBuf, String>>>>;

/// What to do on a given frame of a capture attempt.
///
/// Split out from [`Screenshotter::drive`] so the frame arithmetic is testable
/// without a live window. The bug this prevents is not hypothetical: warmup and
/// budget used to be compared against the app's lifetime frame counter, so a
/// capture requested after the budget had already elapsed expired on arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Warming up, waiting for the pipeline, or waiting for the image.
    Wait,
    /// Ask egui for the image.
    Request,
    /// Give up.
    Expire,
}

/// Decides a frame's action from elapsed frames alone.
///
/// `elapsed` counts from the frame the attempt started, never from process start.
/// Expiry is checked first so the request is never sent on the same frame the
/// attempt is abandoned.
fn step(elapsed: u32, sent: bool, pipeline_settled: bool, request: &ScreenshotRequest) -> Step {
    if elapsed > request.budget_frames {
        Step::Expire
    } else if !sent && elapsed >= request.warmup_frames && pipeline_settled {
        Step::Request
    } else {
        Step::Wait
    }
}

/// Drives a capture to a PNG on disk.
pub(crate) struct Screenshotter {
    request: ScreenshotRequest,
    /// The frame this was first driven on, or `None` before the first frame.
    ///
    /// Warmup and budget are measured from here. A `capture` command can arrive on
    /// frame 50,000 of a long-lived window and must still get its full budget.
    started_at: Option<u32>,
    sent: bool,
    outcome: ScreenshotOutcome,
}

impl Screenshotter {
    pub(crate) fn new(request: ScreenshotRequest, outcome: ScreenshotOutcome) -> Self {
        Self {
            request,
            started_at: None,
            sent: false,
            outcome,
        }
    }

    /// Advances the state machine by one frame.
    ///
    /// `pipeline_settled` means every requested page has arrived; capturing before
    /// that shows placeholders rather than pages. Returns `true` once the capture
    /// has resolved, one way or the other, and this should be dropped.
    pub(crate) fn drive(
        &mut self,
        ctx: &egui::Context,
        frame: u32,
        pipeline_settled: bool,
    ) -> bool {
        // Without this the app idles between frames and the reply never arrives.
        ctx.request_repaint();

        // Check for the reply before asking again, so we notice it the frame it
        // lands rather than a frame later.
        let captured = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });

        if let Some(image) = captured {
            let result = save_screenshot(&image, &self.request.path);
            self.finish(ctx, result);
            return true;
        }

        let started_at = *self.started_at.get_or_insert(frame);
        let elapsed = frame.saturating_sub(started_at);

        match step(elapsed, self.sent, pipeline_settled, &self.request) {
            Step::Wait => false,
            Step::Request => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.sent = true;
                false
            }
            Step::Expire => {
                let budget = self.request.budget_frames;
                self.finish(
                    ctx,
                    Err(format!("no screenshot arrived within {budget} frames")),
                );
                true
            }
        }
    }

    fn finish(&mut self, ctx: &egui::Context, result: Result<PathBuf, String>) {
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(result);
        }
        if self.request.exit_when_done {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn save_screenshot(image: &egui::ColorImage, path: &Path) -> Result<PathBuf, String> {
    // egui's `Color32` is premultiplied. A window screenshot is fully opaque, so
    // writing these bytes as straight RGBA is faithful in practice.
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_array())
        .collect();

    let width = u32::try_from(image.size[0]).map_err(|_| "screenshot too wide".to_owned())?;
    let height = u32::try_from(image.size[1]).map_err(|_| "screenshot too tall".to_owned())?;

    let png = RenderedPage {
        width,
        height,
        rgba,
    }
    .encode_png()
    .map_err(|error| error.to_string())?;

    std::fs::write(path, png).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with no interest in where the PNG would land.
    fn capture_request(warmup: u32, budget: u32) -> ScreenshotRequest {
        ScreenshotRequest {
            path: PathBuf::from("unused-by-these-tests.png"),
            warmup_frames: warmup,
            budget_frames: budget,
            exit_when_done: false,
        }
    }

    #[test]
    fn a_capture_waits_for_warmup_and_for_a_settled_pipeline() {
        let request = capture_request(3, 240);
        assert_eq!(step(0, false, true, &request), Step::Wait, "before warmup");
        assert_eq!(
            step(3, false, false, &request),
            Step::Wait,
            "pages still rendering: capturing here shows placeholders"
        );
        assert_eq!(step(3, false, true, &request), Step::Request);
    }

    #[test]
    fn a_capture_is_requested_only_once() {
        let request = capture_request(3, 240);
        assert_eq!(step(10, true, true, &request), Step::Wait);
    }

    #[test]
    fn a_capture_expires_only_after_its_budget() {
        let request = capture_request(3, 240);
        assert_eq!(step(240, true, true, &request), Step::Wait, "on the line");
        assert_eq!(step(241, true, true, &request), Step::Expire);
    }

    #[test]
    fn expiring_wins_over_requesting_on_the_same_frame() {
        // Asking for an image on the frame the attempt is abandoned would leave a
        // reply arriving after nobody is waiting for it.
        let request = capture_request(3, 240);
        assert_eq!(step(241, false, true, &request), Step::Expire);
    }

    #[test]
    fn a_capture_requested_late_in_a_long_session_still_gets_its_budget() {
        // The regression. Warmup and budget used to be compared against the app's
        // lifetime frame counter, so a `capture` command arriving after about four
        // seconds of window uptime expired on the frame it arrived -- egui was
        // asked for an image and then given no frame to answer in. Found by
        // driving a real window by hand, which is also the only way it could be
        // found: the end-to-end capture test happens to run inside the first 240
        // frames of process life.
        let outcome: ScreenshotOutcome = Arc::new(Mutex::new(None));
        let mut shot = Screenshotter::new(capture_request(3, 240), Arc::clone(&outcome));
        let ctx = egui::Context::default();

        assert!(
            !shot.drive(&ctx, 50_000, true),
            "expired on the frame it arrived"
        );
        assert!(
            outcome.lock().unwrap().is_none(),
            "reported an outcome before waiting a single frame"
        );

        assert!(
            !shot.drive(&ctx, 50_100, true),
            "gave up 100 frames into a 240 frame budget"
        );

        assert!(
            shot.drive(&ctx, 50_241, true),
            "never expired, so a lost capture would wait forever"
        );
        let reported = outcome.lock().unwrap().clone();
        assert!(matches!(reported, Some(Err(_))), "{reported:?}");
    }

    fn timing(frame_ms: f32) -> FrameTiming {
        FrameTiming {
            ui_ms: 1.0,
            logic_ms: 0.5,
            frame_ms,
        }
    }

    #[test]
    fn warmup_frames_are_discarded_rather_than_measured() {
        // This is the bug the warmup exists to prevent: a ~150 ms startup frame
        // landing in the maximum and making the report unusable.
        let mut benchmark = ScrollBenchmark::new(10, 1000.0);

        for _ in 0..WARMUP_FRAMES {
            benchmark.record(timing(150.0));
        }
        assert_eq!(
            benchmark.measured_frames(),
            0,
            "warmup frames leaked into the samples"
        );
        assert!(!benchmark.is_finished(), "warmup consumed the frame budget");

        benchmark.record(timing(16.0));
        assert_eq!(benchmark.measured_frames(), 1);

        // The discarded outlier is still remembered, so the report can disclose it
        // rather than quietly dropping it.
        assert_eq!(benchmark.warmup_worst_ms, 150.0);
    }

    #[test]
    fn the_benchmark_finishes_after_the_requested_frame_count() {
        let mut benchmark = ScrollBenchmark::new(3, 1000.0);
        for _ in 0..WARMUP_FRAMES {
            benchmark.record(timing(16.0));
        }
        for _ in 0..3 {
            assert!(!benchmark.is_finished());
            benchmark.record(timing(16.0));
        }
        assert!(benchmark.is_finished());
        assert_eq!(benchmark.measured_frames(), 3);
    }

    #[test]
    fn the_step_covers_the_whole_document() {
        let benchmark = ScrollBenchmark::new(100, 5000.0);
        assert_eq!(benchmark.step_pt(), 50.0);
    }

    #[test]
    fn a_zero_frame_benchmark_does_not_divide_by_zero() {
        let benchmark = ScrollBenchmark::new(0, 5000.0);
        assert!(
            benchmark.step_pt().is_finite(),
            "got {}",
            benchmark.step_pt()
        );
    }

    #[test]
    fn percentiles_of_no_samples_is_none() {
        assert!(percentiles(&[]).is_none());
    }

    #[test]
    fn percentiles_report_real_samples_not_interpolated_ones() {
        // 100 samples: 0.0, 1.0, ... 99.0.
        let samples: Vec<f32> = (0..100).map(|value| value as f32).collect();
        let spread = percentiles(&samples).expect("100 samples");

        assert_eq!(spread.p50, 50.0);
        assert_eq!(spread.p95, 94.0);
        assert_eq!(spread.p99, 98.0);
        assert_eq!(spread.max, 99.0);
    }

    #[test]
    fn percentiles_of_one_sample_are_all_that_sample() {
        let spread = percentiles(&[7.5]).expect("one sample");
        assert_eq!(spread.p50, 7.5);
        assert_eq!(spread.p99, 7.5);
        assert_eq!(spread.max, 7.5);
    }

    #[test]
    fn a_nan_sample_does_not_corrupt_the_ordering() {
        // `partial_cmp`-based sorting on NaN leaves the slice in an arbitrary
        // order, which would silently misreport every percentile.
        let spread = percentiles(&[16.0, f32::NAN, 1.0, 8.0]).expect("four samples");
        assert!(
            spread.p50.is_finite(),
            "a single NaN poisoned the median: {}",
            spread.p50
        );
        // total_cmp sorts NaN above every real number, so it lands in `max`.
        assert!(spread.max.is_nan());
    }
}
