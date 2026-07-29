//! The eframe window.
//!
//! The frame loop only ever polls for finished pages, submits requests for
//! missing ones, and paints whatever the cache currently holds — it never waits
//! for a render. That is the whole reason scrolling can stay smooth while pages
//! are still being drawn.
//!
//! Three things make it feel continuous rather than merely asynchronous:
//!
//! - **Zoom bucketing.** Renders are keyed to a quantized zoom rung, so resizing
//!   the window reuses textures instead of invalidating them on every pixel.
//! - **Stale-resolution fallback.** While the right rung renders, the nearest
//!   cached rung is drawn scaled. Slightly soft beats a grey flash.
//! - **Prefetch.** Pages just outside the viewport are requested after the
//!   visible ones, so scrolling usually finds them already there.
//!
//! The render pipeline is `hayro -> CPU pixmap -> GPU texture`, because hayro
//! rasterizes on the CPU. So this needs no custom wgpu render pass; that only
//! becomes relevant if we implement hayro's `Device` trait ourselves.
//!
//! # Everything goes through a command
//!
//! Since Goal 2, no input path reaches view state directly. A key press, a
//! toolbar click, and a message from an agent all produce a [`Command`], and
//! [`Viewer::dispatch`] is the only thing that carries one out. That is what makes
//! "every feature is programmatically controllable" structural rather than
//! aspirational — there is no second way in to be forgotten about.
//!
//! Frame-time measurement and window capture live in [`crate::devtools`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderError, RenderPool, RenderedPage};
use porpoise_view::{
    CacheKey, MAX_SCALE, MIN_SCALE, Outcome, PageCache, PageNumber, ScrollLayout, ScrollMode, View,
    ViewCommand, ViewState, Viewport, ZoomBucket, ZoomTarget, request_order,
};

use crate::command::Command;
use crate::control::Control;
use crate::devtools::{
    FrameTiming, ScreenshotOutcome, ScreenshotRequest, Screenshotter, ScrollBenchmark,
};
use crate::protocol::{Event, Reply, RequestBody, Snapshot};

/// Vertical gap between pages, in PDF points.
const PAGE_GAP_PT: f64 = 12.0;

/// Byte budget for cached page textures.
///
/// Goal 1 targets under 500 MB resident for a whole document, so this leaves
/// headroom for everything else. In practice `retain_pages` keeps usage far below
/// it — the budget is the backstop, not the mechanism.
const TEXTURE_BUDGET_BYTES: usize = 192 << 20;

/// Pages either side of the viewport to render speculatively.
const PREFETCH_PAGES: usize = 2;

/// Pages either side of the viewport whose textures are kept.
///
/// Wider than [`PREFETCH_PAGES`] so that reversing direction — a common thing to
/// do — reuses a texture instead of re-rendering. Costs a few megabytes against a
/// 192 MB budget.
///
/// Note: this was originally widened on the theory that per-frame texture
/// allocation churn explained the occasional long frame during a fast scroll.
/// Measured with `--scroll-benchmark` at both 3 and 8 pages, the tail was
/// unchanged, so that theory is **wrong** and this value is justified only by the
/// re-render saving above.
const RETAIN_PAGES: usize = 8;

/// Results absorbed per frame, so a burst of completions cannot stall a frame.
const MAX_RESULTS_PER_FRAME: usize = 8;

/// How long one page may take before the render is abandoned.
///
/// Shorter than the CLI's budget because an interactive viewer should give up and
/// show an error tile rather than leave a page blank for ten seconds.
const JOB_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra attempts a page gets after a timeout, beyond the first.
///
/// Three attempts total, costing at most three [`JOB_TIMEOUT`] periods of one
/// worker. Bounded because the failure might be the machine and might be the
/// page, and we cannot tell which from here.
const MAX_RENDER_RETRIES: u8 = 2;

/// Fraction of the viewport a page-down moves in free-scroll mode.
///
/// Slightly less than a full screen so a line or two carries over, which makes it
/// obvious nothing was skipped.
const VIEWPORT_STEP_FRACTION: f64 = 0.9;

/// How far an arrow key scrolls, in PDF points.
const ARROW_STEP_PT: f64 = 48.0;

/// Frames a command-triggered capture waits before asking.
const CAPTURE_WARMUP_FRAMES: u32 = 3;

/// Frames after which a capture gives up rather than leaving a window open.
const CAPTURE_BUDGET_FRAMES: u32 = 240;

/// A rasterization that failed, and whether it is worth another attempt.
struct Failure {
    /// The renderer's own message, shown on the error tile.
    message: String,
    /// Attempts remaining. Zero means we have given up on this rasterization.
    retries_left: u8,
}

impl Failure {
    /// The failure to record for `error`, carrying over whatever retries an
    /// earlier attempt at the same rasterization had left.
    ///
    /// A timeout usually means the machine was momentarily busy rather than that
    /// this page is unrenderable, so it earns another attempt. Every other failure
    /// is deterministic — the index is out of range, the size is refused, or the
    /// interpreter panicked — and retrying one only burns a worker to arrive at
    /// the same answer.
    fn from_error(error: &RenderError, previous: Option<&Self>) -> Self {
        let retries_left = if matches!(error, RenderError::TimedOut { .. }) {
            previous.map_or(MAX_RENDER_RETRIES, |failure| failure.retries_left)
        } else {
            0
        };
        Self {
            message: error.to_string(),
            retries_left,
        }
    }

    /// Spends one retry, reporting whether there was one to spend.
    fn take_retry(&mut self) -> bool {
        if self.retries_left == 0 {
            return false;
        }
        self.retries_left -= 1;
        true
    }

    /// Whether this rasterization has been abandoned.
    fn gave_up(&self) -> bool {
        self.retries_left == 0
    }
}

/// Everything that belongs to one open document.
///
/// Grouped so that opening another is a single replacement rather than eight
/// fields to remember to reset — the kind of bookkeeping that goes wrong once a
/// ninth is added.
struct OpenDocument {
    path: PathBuf,
    document: Arc<Document>,
    layout: ScrollLayout,
    pool: RenderPool,
    cache: PageCache<egui::TextureHandle>,
    /// Requests submitted but not yet returned, so a page is not queued twice.
    in_flight: Vec<CacheKey>,
    /// Failures keyed by rasterization, not by page, so a different zoom is still
    /// attempted. A timeout keeps a retry budget; see [`Failure::from_error`].
    failures: HashMap<CacheKey, Failure>,
    /// The rung work was last submitted for, to notice when zoom moves.
    submitted_bucket: ZoomBucket,
}

impl OpenDocument {
    fn new(path: PathBuf, document: Document, bucket: ZoomBucket) -> Self {
        let document = Arc::new(document);
        let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);
        let pool = RenderPool::new(
            Arc::clone(&document),
            HayroRenderer::new(),
            RenderPool::recommended_workers(),
            JOB_TIMEOUT,
        );
        Self {
            path,
            document,
            layout,
            pool,
            cache: PageCache::new(TEXTURE_BUDGET_BYTES),
            in_flight: Vec::new(),
            failures: HashMap::new(),
            submitted_bucket: bucket,
        }
    }

    /// Whether every requested page has arrived and nothing is outstanding.
    fn settled(&self) -> bool {
        self.in_flight.is_empty() && !self.pool.is_busy()
    }

    /// Rasterizations we have stopped trying to produce.
    fn abandoned(&self) -> usize {
        self.failures
            .values()
            .filter(|failure| failure.gave_up())
            .count()
    }
}

/// Hidden entry points used to verify and measure the window from a headless
/// context. Grouped so they are visibly not part of the viewer's real
/// configuration.
#[derive(Default)]
pub(crate) struct DevOptions {
    /// Capture the window to this path and exit.
    pub(crate) screenshot: Option<PathBuf>,
    /// Scroll the whole document over this many frames, report, and exit.
    pub(crate) benchmark_frames: Option<u32>,
    /// Report time from this instant until the first page is painted, then exit.
    pub(crate) report_first_page_from: Option<Instant>,
}

/// How to open the viewer.
pub(crate) struct ViewerOptions {
    /// The document to show, if any. `None` opens an empty window, which is what
    /// `porpoise serve` does when it expects an agent to send `open`.
    pub(crate) document: Option<(PathBuf, Document)>,
    /// Scroll here on the first frame, counting from 1.
    pub(crate) start_page: Option<PageNumber>,
    /// An external process driving the program, if one asked to.
    pub(crate) control: Option<Control>,
    /// See [`DevOptions`].
    pub(crate) devtools: DevOptions,
}

/// Opens the viewer window. Blocks until the window closes.
pub(crate) fn run(options: ViewerOptions) -> Result<(), Box<dyn std::error::Error>> {
    let wanted_screenshot = options.devtools.screenshot.is_some();
    let outcome: ScreenshotOutcome = Arc::new(Mutex::new(None));

    let title = match &options.document {
        Some((path, _)) => path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        None => "no document".to_owned(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([320.0, 240.0])
            .with_title(format!("{title} — Porpoise PDF")),
        ..Default::default()
    };

    let app_outcome = Arc::clone(&outcome);
    eframe::run_native(
        "porpoise",
        native_options,
        Box::new(move |_cc| Ok(Box::new(Viewer::new(options, app_outcome)))),
    )?;

    if wanted_screenshot {
        let captured = outcome.lock().ok().and_then(|mut slot| slot.take());
        match captured {
            Some(Ok(path)) => println!("wrote {}", path.display()),
            Some(Err(message)) => return Err(message.into()),
            None => return Err("the window closed before a screenshot was captured".into()),
        }
    }

    Ok(())
}

struct Viewer {
    /// `None` until a document is opened.
    open: Option<OpenDocument>,
    /// Everything a command can change. The single source of truth for the view.
    state: ViewState,
    /// Measured from the window each frame; environment, not state.
    viewport: Viewport,

    timing: FrameTiming,

    /// Scroll here once a document is open, then leave the operator in control.
    start_page: Option<PageNumber>,
    applied_start_page: bool,

    frame: u32,
    benchmark: Option<ScrollBenchmark>,
    screenshot: Option<Screenshotter>,
    screenshot_outcome: ScreenshotOutcome,
    /// Set when asked to report launch-to-first-page; cleared once reported.
    first_page_from: Option<Instant>,

    /// Present only under `porpoise serve`.
    control: Option<Control>,
    /// Events collected during a frame and flushed at the end of it.
    pending_events: Vec<Event>,
    /// The last state reported, so `ViewChanged` is emitted on change rather than
    /// every frame.
    last_reported: Option<Snapshot>,
    /// Whether the pipeline was settled last frame, so `Idle` fires on the edge.
    was_settled: bool,
}

impl Viewer {
    fn new(options: ViewerOptions, screenshot_outcome: ScreenshotOutcome) -> Self {
        let state = ViewState::new();
        let open = options
            .document
            .map(|(path, document)| OpenDocument::new(path, document, ZoomBucket::enclosing(1.0)));

        let benchmark = options.devtools.benchmark_frames.map(|frames| {
            let height = open
                .as_ref()
                .map_or(0.0, |open| open.layout.content_height_pt());
            ScrollBenchmark::new(frames, height)
        });
        let screenshot = options.devtools.screenshot.map(|path| {
            Screenshotter::new(
                ScreenshotRequest {
                    path,
                    warmup_frames: CAPTURE_WARMUP_FRAMES,
                    budget_frames: CAPTURE_BUDGET_FRAMES,
                    // The CLI flag means "capture and exit", so a stranded window
                    // would hang the command.
                    exit_when_done: true,
                },
                Arc::clone(&screenshot_outcome),
            )
        });

        Self {
            open,
            state,
            viewport: Viewport::new(0.0, 0.0),
            timing: FrameTiming {
                ui_ms: 0.0,
                logic_ms: 0.0,
                frame_ms: 0.0,
            },
            start_page: options.start_page,
            applied_start_page: false,
            frame: 0,
            benchmark,
            screenshot,
            screenshot_outcome,
            first_page_from: options.devtools.report_first_page_from,
            control: options.control,
            pending_events: Vec::new(),
            last_reported: None,
            // Nothing has been requested yet, so the pipeline starts settled. Set
            // deliberately: starting at `false` would emit a spurious `Idle` on the
            // first frame, before anything had been asked for.
            was_settled: true,
        }
    }

    /// Queues an event, if anyone is listening.
    ///
    /// Takes a closure so that building the event — which can allocate — costs
    /// nothing when the program is being driven by a person.
    fn emit(&mut self, event: impl FnOnce() -> Event) {
        if self.control.is_some() {
            self.pending_events.push(event());
        }
    }

    /// An empty layout, so the view can be read even with no document open.
    ///
    /// Takes the field rather than `&self` so that callers can hold this alongside
    /// a mutable borrow of [`Viewer::state`] — which `dispatch` needs, since
    /// applying a command reads the layout and writes the state at the same time.
    fn layout_of(open: &Option<OpenDocument>) -> &ScrollLayout {
        static EMPTY: std::sync::LazyLock<ScrollLayout> =
            std::sync::LazyLock::new(|| ScrollLayout::vertical(&[], PAGE_GAP_PT));
        open.as_ref().map_or(&EMPTY, |open| &open.layout)
    }

    fn layout(&self) -> &ScrollLayout {
        Self::layout_of(&self.open)
    }

    fn view(&self) -> View<'_> {
        self.state.with(self.layout(), self.viewport)
    }

    /// Everything readable about the program right now.
    fn snapshot(&self) -> Snapshot {
        let mut failed_pages: Vec<PageNumber> = self
            .open
            .as_ref()
            .map(|open| {
                open.failures
                    .iter()
                    .filter(|(_, failure)| failure.gave_up())
                    .map(|(key, _)| PageNumber::from_index(key.page))
                    .collect()
            })
            .unwrap_or_default();
        failed_pages.sort_unstable();
        failed_pages.dedup();

        Snapshot {
            document: self
                .open
                .as_ref()
                .map(|open| open.path.display().to_string()),
            view: self.view().snapshot(),
            pages_cached: self.open.as_ref().map_or(0, |open| open.cache.len()),
            cache_bytes: self.open.as_ref().map_or(0, |open| open.cache.used_bytes()),
            renders_in_flight: self.open.as_ref().map_or(0, |open| open.in_flight.len()),
            failed_pages,
            idle: self.settled(),
        }
    }

    /// Whether every requested page has arrived. Vacuously true with no document.
    fn settled(&self) -> bool {
        self.open.as_ref().is_none_or(OpenDocument::settled)
    }

    // --- Control channel ----------------------------------------------------

    /// Carries out anything the controlling process asked for, and replies.
    fn serve_control(&mut self, ctx: &egui::Context) {
        let Some(control) = &mut self.control else {
            return;
        };
        if control.hung_up() {
            // Every other stdio protocol treats a closed stdin as "we are done".
            tracing::info!("control channel closed; exiting");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        for incoming in control.poll() {
            let request = match incoming {
                Ok(request) => request,
                Err(failure) => {
                    // Reply against the id when the line got far enough to have one.
                    // A bad argument on an otherwise well-formed line has a perfectly
                    // good id, and dropping it leaves a client waiting for a reply it
                    // will never be able to match.
                    let reply = Reply::failed(failure.id, failure.reason);
                    if let Some(control) = &mut self.control {
                        control.send(&reply);
                    }
                    continue;
                }
            };

            let reply = match request.body {
                RequestBody::Snapshot => Reply::with_snapshot(request.id, self.snapshot()),
                RequestBody::Commands => Reply::with_commands(request.id),
                RequestBody::Command(command) => {
                    let result = self.dispatch(ctx, command);
                    result.into_reply(request.id)
                }
            };
            if let Some(control) = &mut self.control {
                control.send(&reply);
            }
        }
    }

    /// Emits state-change and idle events, then flushes the frame's events.
    fn report_control(&mut self) {
        if self.control.is_none() {
            return;
        }

        // Coalesced to one per frame by comparing against what was last sent, which
        // also catches changes the operator made by hand rather than by command.
        let snapshot = self.snapshot();
        if self.last_reported.as_ref() != Some(&snapshot) {
            self.last_reported = Some(snapshot.clone());
            self.pending_events.push(Event::ViewChanged {
                snapshot: Box::new(snapshot),
            });
        }

        // On the edge, not every frame: a client waiting for work to finish wants
        // one signal, not a stream of them while nothing happens.
        let settled = self.settled();
        if settled && !self.was_settled {
            self.pending_events.push(Event::Idle);
        }
        self.was_settled = settled;

        let events = std::mem::take(&mut self.pending_events);
        if let Some(control) = &mut self.control {
            for event in events {
                control.send(&event);
            }
        }
    }

    // --- Commands -----------------------------------------------------------

    /// Carries out one command. The only path into view or document state.
    fn dispatch(&mut self, ctx: &egui::Context, command: Command) -> DispatchResult {
        match command {
            Command::View(view) => {
                let layout = Self::layout_of(&self.open);
                let outcome = porpoise_view::apply(&mut self.state, layout, self.viewport, view);
                if let Some(rejection) = outcome.rejected() {
                    tracing::debug!(command = view.name(), %rejection, "command refused");
                }
                DispatchResult::View(outcome)
            }
            Command::Open { path } => match Document::open(&path) {
                Ok(document) => {
                    let bucket = self.view().bucket();
                    let page_count = document.page_count();
                    let reported = path.display().to_string();
                    self.open = Some(OpenDocument::new(path, document, bucket));
                    // A new document invalidates where we were looking.
                    self.state = ViewState::new();
                    self.applied_start_page = false;
                    self.emit(|| Event::DocumentOpened {
                        path: reported,
                        page_count,
                    });
                    DispatchResult::Opened
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not open document");
                    DispatchResult::Failed(error.to_string())
                }
            },
            Command::Close => {
                self.open = None;
                self.state = ViewState::new();
                self.emit(|| Event::DocumentClosed);
                DispatchResult::Closed
            }
            Command::Capture { path } => {
                // Clear any previous result, so "the slot holds the outcome of the
                // most recent capture" is true by construction rather than by an
                // argument about when `drive_screenshot` happens to read it.
                if let Ok(mut slot) = self.screenshot_outcome.lock() {
                    *slot = None;
                }
                self.screenshot = Some(Screenshotter::new(
                    ScreenshotRequest {
                        path,
                        warmup_frames: CAPTURE_WARMUP_FRAMES,
                        budget_frames: CAPTURE_BUDGET_FRAMES,
                        // A capture is one step in a longer session. Closing the
                        // window here would end the conversation mid-sentence.
                        exit_when_done: false,
                    },
                    Arc::clone(&self.screenshot_outcome),
                ));
                DispatchResult::CaptureStarted
            }
            Command::Quit => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                DispatchResult::Quitting
            }
        }
    }

    // --- Input --------------------------------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Collect first, then act: the closure borrows egui's input state, and
        // dispatch needs `&mut self`.
        let (pressed, zoom_delta) = ctx.input(|input| {
            let pressed: Vec<(egui::Key, egui::Modifiers)> = input
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect();
            // `zoom_delta` already means ctrl+wheel or a pinch gesture, per
            // platform convention, and is 1.0 when neither happened. Better than
            // detecting ctrl+scroll by hand, which would miss trackpad pinches.
            (pressed, input.zoom_delta())
        });

        if (zoom_delta - 1.0).abs() > 0.001 {
            // A pinch is not a command of its own — it is a zoom with a computed
            // factor, applied continuously so the gesture feels proportional.
            // Bucketing still bounds how often we re-render.
            let target = (self.view().zoom() * zoom_delta).clamp(MIN_SCALE, MAX_SCALE);
            self.dispatch(
                ctx,
                ViewCommand::SetZoom {
                    target: ZoomTarget::Fixed(target),
                }
                .into(),
            );
        }

        let mode = self.state.scroll_mode();
        for (key, modifiers) in pressed {
            if let Some(command) = command_for_key(key, modifiers, mode) {
                self.dispatch(ctx, command);
            }
        }
    }

    // --- Render pipeline ----------------------------------------------------

    /// Absorbs finished renders into the cache. Never blocks.
    fn collect_renders(&mut self, ctx: &egui::Context) {
        for _ in 0..MAX_RESULTS_PER_FRAME {
            let Some(outcome) = self.open.as_ref().and_then(|open| open.pool.try_recv()) else {
                break;
            };

            // The tag is the zoom rung we asked for. A result whose rung is no
            // longer current is still worth keeping: it is a valid render of that
            // page and serves as a fallback until the current rung arrives.
            let Ok(rung) = i16::try_from(outcome.tag) else {
                continue;
            };
            let key = CacheKey::new(outcome.page_index, ZoomBucket::from_rung(rung));
            if let Some(open) = &mut self.open {
                open.in_flight.retain(|pending| *pending != key);
            }

            match outcome.result {
                Ok(page) => {
                    self.accept_page(ctx, key, rung, &page);
                    self.emit(|| Event::PageRendered {
                        page: PageNumber::from_index(outcome.page_index),
                    });
                }
                Err(error) => {
                    let Some(open) = &mut self.open else { continue };
                    let failure = Failure::from_error(&error, open.failures.get(&key));
                    tracing::warn!(
                        page = outcome.page_index,
                        rung,
                        retries_left = failure.retries_left,
                        %error,
                        "page failed to rasterize"
                    );
                    let will_retry = !failure.gave_up();
                    let reason = failure.message.clone();
                    open.failures.insert(key, failure);
                    self.emit(|| Event::PageFailed {
                        page: PageNumber::from_index(outcome.page_index),
                        reason,
                        will_retry,
                    });
                }
            }
        }
    }

    fn accept_page(&mut self, ctx: &egui::Context, key: CacheKey, rung: i16, page: &RenderedPage) {
        let bytes = page.rgba.len();
        let image = to_color_image(page);
        let Some(open) = &mut self.open else { return };

        let Some(image) = image else {
            tracing::warn!(
                page = key.page,
                width = page.width,
                height = page.height,
                bytes,
                "renderer returned a buffer inconsistent with its dimensions"
            );
            open.failures.insert(
                key,
                Failure {
                    message: format!(
                        "renderer returned {bytes} bytes for a {}x{} page",
                        page.width, page.height
                    ),
                    retries_left: 0,
                },
            );
            return;
        };

        let handle = ctx.load_texture(
            format!("page-{}-r{rung}", key.page),
            image,
            egui::TextureOptions::LINEAR,
        );
        open.cache.insert(key, handle, bytes);
        open.failures.remove(&key);

        // Report on the first page to actually reach the cache, which is the
        // moment something is visible.
        if let Some(launched) = self.first_page_from.take() {
            println!(
                "time to first page: {:.0} ms",
                launched.elapsed().as_secs_f64() * 1000.0
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Queues anything visible or nearby that is not already cached or in flight.
    fn request_missing(&mut self, pixels_per_point: f32) {
        let bucket = self.view().bucket();
        let visible = self.view().visible_pages();
        let Some(open) = &mut self.open else { return };

        if bucket != open.submitted_bucket {
            // Queued work is for the old rung and no longer worth doing. Cached
            // textures are kept deliberately — they are the fallback that stops a
            // resize from flashing grey.
            open.pool.cancel_pending();
            open.in_flight.clear();
            open.failures.clear();
            open.submitted_bucket = bucket;
        }

        let order = request_order(visible, PREFETCH_PAGES, open.layout.page_count());
        let scale = bucket.scale() * pixels_per_point;
        let tag = i64::from(bucket.rung());

        for page in order {
            let key = CacheKey::new(page, bucket);
            if open.cache.contains(key) || open.in_flight.contains(&key) {
                continue;
            }
            // A failure with retries left earns another attempt, spending one.
            // Without a budget this would re-request a hopeless page every frame.
            if let Some(failure) = open.failures.get_mut(&key)
                && !failure.take_retry()
            {
                continue;
            }
            if open.pool.submit(page, scale, tag) {
                open.in_flight.push(key);
            }
        }
    }

    // --- Painting -----------------------------------------------------------

    /// The texture to draw for a page: the current rung, else the nearest cached
    /// rung, else nothing.
    ///
    /// Takes the cache mutably because a hit counts as a use — leaving LRU order
    /// untouched here would make the byte budget evict pages that are on screen.
    fn texture_for(
        cache: &mut PageCache<egui::TextureHandle>,
        page: usize,
        bucket: ZoomBucket,
    ) -> Option<egui::TextureId> {
        let key = CacheKey::new(page, bucket);
        if let Some(texture) = cache.get(key) {
            return Some(texture.id());
        }
        // Deliberately a second statement rather than `or_else`: the first borrow
        // is mutable and the second is not. Slightly soft beats a grey flash while
        // the right resolution renders.
        cache
            .best_for_page(page, bucket)
            .map(|(_, texture)| texture.id())
    }

    fn paint_page(
        open: &OpenDocument,
        painter: &egui::Painter,
        page: usize,
        bucket: ZoomBucket,
        rect: egui::Rect,
        texture: Option<egui::TextureId>,
    ) {
        let key = CacheKey::new(page, bucket);

        if let Some(id) = texture {
            painter.image(id, rect, FULL_UV, egui::Color32::WHITE);
            return;
        }

        // A failure with retries left is still pending, so claiming failure would
        // be premature — fall through to the placeholder instead.
        if let Some(failure) = open.failures.get(&key)
            && failure.gave_up()
        {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(52, 30, 30));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_BOTTOM,
                format!("page {} could not be rendered", page + 1),
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(230, 140, 140),
            );
            // The renderer's own message, which distinguishes a timeout from a
            // refused size from a panic. Worth showing rather than storing.
            painter.text(
                rect.center(),
                egui::Align2::CENTER_TOP,
                &failure.message,
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(170, 120, 120),
            );
            return;
        }

        // Not rasterized yet. A correct-aspect tile means scrolling never jumps
        // when the real page arrives.
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(232));
    }

    fn draw_pages(&mut self, ui: &mut egui::Ui) {
        // Cloning the context is cheap (an `Arc` inside) and avoids holding an
        // immutable borrow of `ui` across the mutable calls below.
        let ctx = ui.ctx().clone();
        let pixels_per_point = ctx.pixels_per_point();

        // The viewport is environment: measure it, do not store it as state.
        self.viewport = Viewport::new(ui.available_width(), ui.available_height());

        if self.open.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label("No document open. Pass a path, or send an `open` command.")
            });
            return;
        }
        if self.layout().page_count() == 0 {
            ui.centered_and_justified(|ui| ui.label("This document has no pages."));
            return;
        }

        // Honour --start-page once, then hand control back to the operator.
        if let (Some(page), false) = (self.start_page, self.applied_start_page) {
            self.applied_start_page = true;
            self.dispatch(&ctx, ViewCommand::GoToPage { page }.into());
        }

        let zoom = self.view().zoom();
        let bucket = self.view().bucket();

        #[expect(
            clippy::cast_possible_truncation,
            reason = "content extents are page dimensions; f32 is what egui works in"
        )]
        let content_size = egui::vec2(
            self.layout().content_width_pt() as f32 * zoom,
            self.layout().content_height_pt() as f32 * zoom,
        );

        // Both axes, because zooming past fit-width makes the document wider than the
        // window and the right-hand side of a landscape sheet is otherwise
        // unreachable. `auto_shrink` defaults to true, which sizes the scroll area to
        // its contents and puts the vertical scrollbar against the edge of the *page*
        // rather than the edge of the window.
        let mut scroll_area = egui::ScrollArea::both().auto_shrink([false; 2]);

        // Only override the offset on frames where a command asked for it, or the
        // operator could never scroll by hand. See `ViewState`'s module docs on why
        // egui keeps ownership of the live offset.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "scroll offsets are bounded by content extents"
        )]
        if let Some(top_pt) = self.state.take_requested_scroll_pt() {
            scroll_area = scroll_area.vertical_scroll_offset(top_pt as f32 * zoom);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "pan offsets are bounded by content width"
        )]
        if let Some(left_pt) = self.state.take_requested_scroll_left_pt() {
            scroll_area = scroll_area.horizontal_scroll_offset(left_pt as f32 * zoom);
        }

        scroll_area.show_viewport(ui, |ui, viewport| {
            // Claim the full width even when the document is narrower, so the pages
            // centre in the window rather than hugging the left edge with the
            // scrollbar stranded out to the right of them.
            let column_width = content_size.x.max(ui.available_width());
            let (content_rect, _response) = ui.allocate_exact_size(
                egui::vec2(column_width, content_size.y),
                egui::Sense::hover(),
            );

            // `viewport` is in content coordinates, so dividing by zoom converts
            // the scroll window back into PDF points. This is the reconciliation
            // point: egui tells us where it actually is.
            self.state
                .report_scroll_top_pt(f64::from(viewport.min.y / zoom));
            // The content column is at least as wide as the window, so when the
            // document is narrower than the window the padding sits on both sides and
            // the page's own left edge is not at x = 0. Report the offset relative to
            // the page, not to the column, or panning would appear to start halfway.
            let gutter = (column_width - content_size.x).max(0.0) * 0.5;
            self.state
                .report_scroll_left_pt(f64::from((viewport.min.x - gutter).max(0.0) / zoom));

            self.request_missing(pixels_per_point);

            let visible = self.view().visible_pages();
            let Some(open) = &mut self.open else { return };

            // Resolved in a first pass because a cache hit is a *use* and updates
            // LRU order, which needs the cache mutably — while painting needs the
            // layout and geometry immutably.
            let tiles: Vec<(usize, egui::Rect, Option<egui::TextureId>)> = visible
                .clone()
                .filter_map(|page| {
                    let top_pt = open.layout.page_top_pt(page)?;
                    let geometry = open.document.geometry().get(page).copied()?;

                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "page offsets are bounded by content height"
                    )]
                    let rect = {
                        let size = egui::vec2(geometry.width_pt * zoom, geometry.height_pt * zoom);
                        // Centre each page in the column, so a narrow page among
                        // wide ones does not sit flush left. The column is at least
                        // as wide as the window, so this also centres the whole
                        // document when it is narrower than the window.
                        let x = (column_width - size.x) * 0.5;
                        egui::Rect::from_min_size(
                            content_rect.min + egui::vec2(x, top_pt as f32 * zoom),
                            size,
                        )
                    };

                    let texture = Self::texture_for(&mut open.cache, page, bucket);
                    Some((page, rect, texture))
                })
                .collect();

            for (page, rect, texture) in tiles {
                Self::paint_page(open, ui.painter(), page, bucket, rect, texture);
            }

            // Keep memory proportional to the viewport rather than the document.
            let low = visible.start.saturating_sub(RETAIN_PAGES);
            let high = visible.end.saturating_add(RETAIN_PAGES);
            open.cache.retain_pages(|page| (low..high).contains(&page));
        });

        // Keep frames coming while anything is still being drawn.
        if self.open.as_ref().is_some_and(|open| !open.settled()) {
            ctx.request_repaint();
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        // Collected rather than dispatched inline, because dispatch needs
        // `&mut self` while `ui` is borrowed. Note every button produces the same
        // command an agent would send — there is no click-only path.
        let mut issued: Vec<Command> = Vec::new();
        let zoom_target = self.state.zoom_target();
        let paged = self.state.scroll_mode() == ScrollMode::Paged;

        ui.horizontal(|ui| {
            if ui.button("⏮").on_hover_text("First page (Home)").clicked() {
                issued.push(ViewCommand::FirstPage.into());
            }
            if ui.button("⏭").on_hover_text("Last page (End)").clicked() {
                issued.push(ViewCommand::LastPage.into());
            }
            ui.separator();

            if ui.button("−").on_hover_text("Zoom out (Ctrl+-)").clicked() {
                issued.push(ViewCommand::StepZoom { rungs: -1 }.into());
            }
            if ui.button("+").on_hover_text("Zoom in (Ctrl++)").clicked() {
                issued.push(ViewCommand::StepZoom { rungs: 1 }.into());
            }

            if ui
                .selectable_label(zoom_target == ZoomTarget::FitWidth, "Width")
                .on_hover_text("Fit width (Ctrl+0)")
                .clicked()
            {
                issued.push(
                    ViewCommand::SetZoom {
                        target: ZoomTarget::FitWidth,
                    }
                    .into(),
                );
            }
            if ui
                .selectable_label(zoom_target == ZoomTarget::FitPage, "Page")
                .on_hover_text("Fit page (Ctrl+2)")
                .clicked()
            {
                issued.push(
                    ViewCommand::SetZoom {
                        target: ZoomTarget::FitPage,
                    }
                    .into(),
                );
            }
            ui.separator();

            // Paged versus free changes what PageDown and Space mean.
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
                issued.push(ViewCommand::SetScrollMode { mode }.into());
            }
        });

        let ctx = ui.ctx().clone();
        for command in issued {
            self.dispatch(&ctx, command);
        }
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        let view = self.view();
        ui.horizontal(|ui| {
            match &self.open {
                Some(open) => {
                    ui.label(format!(
                        "page {} of {}",
                        PageNumber::from_index(view.current_page()),
                        open.layout.page_count()
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{:.0}% {}",
                        view.zoom() * 100.0,
                        self.state.zoom_target().label()
                    ));
                    ui.separator();
                    ui.label(self.state.scroll_mode().label());
                    ui.separator();
                    // Proof of virtualization: both stay small however long the
                    // document.
                    ui.label(format!(
                        "{} cached, {:.1} MB",
                        open.cache.len(),
                        open.cache.used_bytes() as f64 / (1024.0 * 1024.0)
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} workers, {} in flight",
                        open.pool.worker_count(),
                        open.in_flight.len()
                    ));
                    ui.separator();
                    ui.label(format!(
                        "ui {:.1} ms, frame {:.1} ms",
                        self.timing.ui_ms, self.timing.frame_ms
                    ));
                    // Counts only what we have given up on, matching the error
                    // tiles. A page still being retried is not a failure yet.
                    let abandoned = open.abandoned();
                    if abandoned > 0 {
                        ui.separator();
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("{abandoned} failed"),
                        );
                    }
                }
                None => {
                    ui.label("no document");
                }
            }
        });
    }

    // --- Development aids ---------------------------------------------------

    /// Advances the scripted scroll, and reports once it finishes.
    fn drive_benchmark(&mut self, ctx: &egui::Context) {
        let Some(benchmark) = &mut self.benchmark else {
            return;
        };
        benchmark.record(self.timing);

        if benchmark.is_finished() {
            benchmark.report();
            self.benchmark = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let step = benchmark.step_pt();
        // Keep frames coming at full rate; without this the app idles.
        ctx.request_repaint();
        self.dispatch(ctx, ViewCommand::ScrollBy { points: step }.into());
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let ready = self
            .open
            .as_ref()
            .is_some_and(|open| open.settled() && !open.cache.is_empty());
        let frame = self.frame;
        let Some(screenshotter) = &mut self.screenshot else {
            return;
        };
        if !screenshotter.drive(ctx, frame, ready) {
            return;
        }
        self.screenshot = None;

        // Report the result to a controlling process. The CLI `--screenshot` path
        // reads the same slot after the window closes, so both are served without
        // the capture machinery knowing which asked for it.
        if self.control.is_some() {
            let captured = self
                .screenshot_outcome
                .lock()
                .ok()
                .and_then(|slot| slot.clone());
            match captured {
                Some(Ok(path)) => self.emit(|| Event::Captured {
                    path: path.display().to_string(),
                }),
                Some(Err(error)) => self.emit(|| Event::CaptureFailed { error }),
                None => {}
            }
        }
    }
}

/// What a dispatched command did.
///
/// Coarse on purpose: a caller that needs detail reads the snapshot afterwards,
/// which is the same thing an agent does.
#[derive(Debug, Clone, PartialEq)]
enum DispatchResult {
    View(Outcome),
    Opened,
    Closed,
    CaptureStarted,
    Quitting,
    Failed(String),
}

impl DispatchResult {
    /// The reply to send for this result.
    fn into_reply(self, id: Option<u64>) -> Reply {
        match self {
            Self::View(Outcome::Changed) => Reply::ok(id, "changed"),
            Self::View(Outcome::Unchanged) => Reply::ok(id, "unchanged"),
            Self::View(Outcome::Rejected(rejection)) => Reply::rejected(id, rejection),
            Self::Opened => Reply::ok(id, "opened"),
            Self::Closed => Reply::ok(id, "closed"),
            // Deliberately not "captured": the capture has been *started*, and the
            // file does not exist until the pipeline settles. An agent that treated
            // this as completion would read a file that is not there yet, so it has
            // to wait for the `captured` event.
            Self::CaptureStarted => Reply::ok(id, "capturing"),
            Self::Quitting => Reply::ok(id, "quitting"),
            Self::Failed(error) => Reply::failed(id, error),
        }
    }
}

/// Translates a key press into a command.
///
/// Pure, and mode-aware on purpose. `PageDown` means "next page" in paged mode and
/// "next screenful" in free mode — so the *key handler* decides which command that
/// is. Putting the mode dependence inside a command would mean an agent could
/// never be sure what `NextPage` was going to do.
fn command_for_key(
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

impl eframe::App for Viewer {
    // egui 0.34 replaced `App::update(ctx, frame)` with `App::ui(ui, frame)` plus
    // this optional pre-pass. `logic` may not paint, which makes it the right
    // place to absorb finished renders and drive the screenshot state machine.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame = self.frame.saturating_add(1);
        // `stable_dt` is the interval the user actually perceives, including any
        // wait for vsync — which is the number that decides whether scrolling
        // looks smooth.
        self.timing.frame_ms = ctx.input(|input| input.stable_dt) * 1000.0;

        let started = Instant::now();
        self.collect_renders(ctx);
        self.handle_input(ctx);
        self.serve_control(ctx);
        self.timing.logic_ms = started.elapsed().as_secs_f32() * 1000.0;

        // Deliberately after the timing: these only bookkeep and would otherwise
        // charge instrumentation overhead to the pipeline.
        self.drive_benchmark(ctx);
        self.drive_screenshot(ctx);
        self.report_control();

        // A controlling process expects progress without a person moving the mouse,
        // so keep frames coming rather than idling until the next input event.
        if self.control.is_some() {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = Instant::now();

        // `TopBottomPanel` and `SidePanel` were unified into `Panel` in 0.34. The
        // root `ui` is the central area, so there is no CentralPanel here.
        egui::Panel::top("toolbar").show(ui, |ui| self.draw_toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.draw_status(ui));
        self.draw_pages(ui);

        // Our own cost, as distinct from the frame interval. If this stays well
        // under the frame budget, the pipeline has headroom.
        self.timing.ui_ms = started.elapsed().as_secs_f32() * 1000.0;
    }
}

/// The whole texture, for `Painter::image`.
const FULL_UV: egui::Rect = egui::Rect {
    min: egui::Pos2 { x: 0.0, y: 0.0 },
    max: egui::Pos2 { x: 1.0, y: 1.0 },
};

/// Converts a rasterized page into an egui image, or `None` if it could not be
/// turned into a texture safely.
///
/// This is the last thing between the renderer and the GPU, and it exists because
/// both steps past it are fallible in ways that end the process rather than the
/// page:
///
/// - `ColorImage::from_rgba_unmultiplied` *panics* on a length mismatch, and a
///   panic on the UI thread takes down the window.
/// - `load_texture` hands the result to wgpu, which validates dimensions. A
///   zero-width or zero-height image passes the length check trivially — zero
///   bytes is exactly what `0 * h * 4` asks for — and then fails validation.
///
/// `HayroRenderer` refuses a sub-pixel page before either of these is reached, so
/// neither case is reachable through the shipped renderer today. The guard does
/// not rely on that: it is the boundary's job to hold whatever the [`Renderer`]
/// on the other side happens to return.
///
/// [`Renderer`]: porpoise_render::Renderer
fn to_color_image(page: &RenderedPage) -> Option<egui::ColorImage> {
    if page.width == 0 || page.height == 0 {
        return None;
    }
    let width = usize::try_from(page.width).ok()?;
    let height = usize::try_from(page.height).ok()?;
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if expected != page.rgba.len() {
        return None;
    }
    // Our buffers are non-premultiplied, which is what this constructor wants.
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        &page.rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(width: u32, height: u32, bytes: usize) -> RenderedPage {
        RenderedPage {
            width,
            height,
            rgba: vec![0; bytes],
        }
    }

    // --- to_color_image: the UI-thread panic guard ---------------------------

    #[test]
    fn a_consistent_buffer_converts() {
        let image = to_color_image(&page(4, 3, 4 * 3 * 4)).expect("4x3 RGBA should convert");
        assert_eq!(image.size, [4, 3]);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_panicking() {
        // One byte short. `ColorImage::from_rgba_unmultiplied` would panic here,
        // on the UI thread, closing the window.
        assert!(to_color_image(&page(4, 3, 4 * 3 * 4 - 1)).is_none());
    }

    #[test]
    fn a_long_buffer_is_refused_too() {
        // Trailing bytes mean the renderer and the header disagree; we cannot tell
        // which is right, so refuse rather than display a guess.
        assert!(to_color_image(&page(4, 3, 4 * 3 * 4 + 1)).is_none());
    }

    #[test]
    fn a_zero_sized_page_is_refused() {
        assert!(to_color_image(&page(0, 3, 0)).is_none());
        assert!(to_color_image(&page(4, 0, 0)).is_none());
    }

    #[test]
    fn dimensions_that_would_overflow_are_refused() {
        assert!(to_color_image(&page(u32::MAX, u32::MAX, 16)).is_none());
    }

    // --- Failure: the retry policy -------------------------------------------

    fn timed_out() -> RenderError {
        RenderError::TimedOut {
            index: 3,
            timeout_ms: 5_000,
        }
    }

    fn panicked() -> RenderError {
        RenderError::Panicked { index: 3 }
    }

    #[test]
    fn a_timeout_starts_with_a_retry_budget() {
        let failure = Failure::from_error(&timed_out(), None);
        assert_eq!(failure.retries_left, MAX_RENDER_RETRIES);
        assert!(
            !failure.gave_up(),
            "a first timeout must not abandon the page"
        );
    }

    #[test]
    fn a_deterministic_failure_is_not_retried() {
        // Retrying a panic, a refused size, or a bad index only burns a worker to
        // reach the same answer.
        for error in [
            panicked(),
            RenderError::NoSuchPage { index: 3, count: 1 },
            RenderError::AreaTooLarge {
                index: 3,
                width: 60_000,
                height: 30_000,
                total_pixels: 1_800_000_000,
                max_total_pixels: 1 << 20,
            },
        ] {
            let failure = Failure::from_error(&error, None);
            assert!(failure.gave_up(), "{error:?} should not be retried");
        }
    }

    #[test]
    fn repeated_timeouts_exhaust_the_budget_and_then_give_up() {
        // The exact loop the viewer runs: request spends a retry, the render fails,
        // the new failure carries the reduced budget forward.
        let mut failure = Failure::from_error(&timed_out(), None);
        let mut attempts = 1;

        while failure.take_retry() {
            attempts += 1;
            failure = Failure::from_error(&timed_out(), Some(&failure));
        }

        assert_eq!(
            attempts,
            usize::from(MAX_RENDER_RETRIES) + 1,
            "expected one initial attempt plus {MAX_RENDER_RETRIES} retries"
        );
        assert!(failure.gave_up());
        assert!(
            !failure.take_retry(),
            "an exhausted failure must stay exhausted"
        );
    }

    #[test]
    fn a_timeout_that_later_panics_stops_being_retried() {
        // The budget must not survive a change of failure kind: if the page turns
        // out to panic, retrying it is pointless however many timeouts preceded it.
        let first = Failure::from_error(&timed_out(), None);
        let second = Failure::from_error(&panicked(), Some(&first));
        assert!(second.gave_up());
    }

    #[test]
    fn the_failure_message_is_the_renderers_own() {
        // It is shown on the error tile, so it has to say which failure this was.
        let failure = Failure::from_error(&timed_out(), None);
        assert!(
            failure.message.contains("5000 ms"),
            "unhelpful message: {}",
            failure.message
        );
    }

    // --- Key translation -----------------------------------------------------

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
