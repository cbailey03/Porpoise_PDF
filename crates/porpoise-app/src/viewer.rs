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
//! # What lives elsewhere
//!
//! This module is the *stateful shell*: the frame loop, dispatch, and painting. The
//! pieces that do not need any of that state were pulled out, which is why they have
//! unit tests and this does not:
//!
//! | Module | What |
//! |---|---|
//! | [`crate::input`] | Key press to [`Command`] — pure |
//! | [`crate::failure`] | Whether a failed render is worth retrying — pure policy |
//! | [`crate::tiles`] | Rasterized page to egui texture — the GPU boundary |
//! | [`crate::picker`] | The file dialog, off the frame loop |
//! | [`crate::devtools`] | Frame timing and window capture |
//!
//! What remains has no unit tests, and that is deliberate rather than an oversight:
//! everything left needs a live `egui::Context`, a GPU adapter, or both. It is
//! covered by `tests/control.rs`, which drives the real binary over a real pipe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use porpoise_doc::{Document, Overwrite, PageGeometry, PageOrder};
use porpoise_render::{HayroRenderer, RenderPool, RenderedPage};
use porpoise_view::{
    CacheKey, MAX_SCALE, MIN_SCALE, Outcome, PAGE_GAP_PT, PageCache, PageNumber, ScrollLayout,
    ScrollMode, View, ViewCommand, ViewState, Viewport, ZoomBucket, ZoomTarget, request_order,
};

use crate::command::Command;
use crate::control::Control;
use crate::devtools::{
    FrameTiming, ScreenshotOutcome, ScreenshotRequest, Screenshotter, ScrollBenchmark,
};
use crate::failure::Failure;
use crate::input::{EditKey, command_for_key, edit_for_key, opens_the_picker};
use crate::picker::FilePicker;
use crate::protocol::{Event, Reply, RequestBody, Snapshot};
use crate::saver::Saver;
use crate::thumbnails::{self, Grid};
use crate::tiles::{FULL_UV, to_color_image};

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

/// Frames a command-triggered capture waits before asking.
const CAPTURE_WARMUP_FRAMES: u32 = 3;

/// Frames after which a capture gives up rather than leaving a window open.
const CAPTURE_BUDGET_FRAMES: u32 = 240;

/// Everything that belongs to one open document.
///
/// Grouped so that opening another is a single replacement rather than eight
/// fields to remember to reset — the kind of bookkeeping that goes wrong once a
/// ninth is added.
struct OpenDocument {
    path: PathBuf,
    document: Arc<Document>,
    /// What order the pages are shown in. Identity until somebody edits it.
    ///
    /// Everything that rasterizes, caches or measures a page goes through this to turn
    /// a *display position* into a *source page*. The two are the same only before the
    /// first edit; see `porpoise-doc`'s `order` module for why that distinction gets
    /// its own crossing point.
    order: PageOrder,
    /// Page positions laid out in a column. Rebuilt whenever [`Self::order`] changes,
    /// because the column is in display order while geometry is in source order.
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

/// Page sizes in display order, for laying out the scrolling column.
fn geometry_in_display_order(document: &Document, order: &PageOrder) -> Vec<PageGeometry> {
    order
        .as_slice()
        .iter()
        .filter_map(|&source| document.geometry().get(source).copied())
        .collect()
}

impl OpenDocument {
    fn new(path: PathBuf, document: Document, bucket: ZoomBucket) -> Self {
        let document = Arc::new(document);
        let order = PageOrder::identity(document.page_count());
        let layout =
            ScrollLayout::vertical(&geometry_in_display_order(&document, &order), PAGE_GAP_PT);
        let pool = RenderPool::new(
            Arc::clone(&document),
            HayroRenderer::new(),
            RenderPool::recommended_workers(),
            JOB_TIMEOUT,
        );
        Self {
            path,
            document,
            order,
            layout,
            pool,
            cache: PageCache::new(TEXTURE_BUDGET_BYTES),
            in_flight: Vec::new(),
            failures: HashMap::new(),
            submitted_bucket: bucket,
        }
    }

    /// Rebuilds the column after an edit.
    ///
    /// The layout is in display order and the document's geometry is in source order,
    /// so any change to [`Self::order`] makes the column wrong until this runs. Cached
    /// textures are deliberately *not* touched: they are keyed by source page, so
    /// moving page 300 to the front costs nothing to redraw.
    fn relayout(&mut self) {
        self.layout = ScrollLayout::vertical(
            &geometry_in_display_order(&self.document, &self.order),
            PAGE_GAP_PT,
        );
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

    /// The system file dialog, when one is open. See [`crate::picker`].
    picker: FilePicker,

    /// A save in flight, if any. See [`crate::saver`] for why it is not inline.
    saver: Saver,

    /// Whether the page grid is showing. See [`crate::thumbnails`].
    thumbnails: bool,

    /// Why the last command failed, for the person to read.
    ///
    /// Failures used to go only to `tracing::warn!`, which for a windowed app means
    /// nowhere. That was survivable while a bad path was a startup error printed to a
    /// terminal; once a file can be chosen at runtime, "that would not open" is a
    /// normal outcome and has to be visible. Also in [`Snapshot`], so an agent reads
    /// exactly what a person sees.
    last_error: Option<String>,

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
            picker: FilePicker::default(),
            saver: Saver::default(),
            thumbnails: false,
            last_error: None,
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
            last_error: self.last_error.clone(),
            thumbnails: self.thumbnails,
            unsaved_changes: self
                .open
                .as_ref()
                .is_some_and(|open| !open.order.is_unedited()),
            can_undo: self.open.as_ref().is_some_and(|open| open.order.can_undo()),
            saving_to: self
                .saver
                .destination()
                .map(|path| path.display().to_string()),
            idle: self.settled(),
        }
    }

    /// Whether every requested page has arrived. Vacuously true with no document.
    /// Whether nothing is outstanding: no renders, and no move still to be made.
    ///
    /// The pending-request half is easy to leave out and matters more than the render
    /// half. A scroll command records a *request*, and the shell only carries it out
    /// while painting. Until then the view has not moved — so reporting `idle` with a
    /// request outstanding tells a client "everything you asked for is done" when the
    /// thing it asked for has not happened.
    ///
    /// On a visible window that window is one frame wide. On a **minimised** one it is
    /// indefinite: painting stops, so the request is never consumed, and the program
    /// goes on cheerfully answering commands and reporting idle. Measured by hand —
    /// `go_to_page 7` on a minimised window replied `changed`, reported `idle: true`,
    /// and left `current_page` at 3 until the window was restored. Nothing is lost, but
    /// a client that trusted `idle` would have read the old page and believed it.
    fn settled(&self) -> bool {
        let no_pending_move = self.state.requested_scroll_pt().is_none()
            && self.state.requested_scroll_left_pt().is_none();
        // A save in flight is outstanding work too. Reporting idle during one would
        // let a client read the file before it exists.
        no_pending_move
            && !self.saver.is_busy()
            && self.open.as_ref().is_none_or(OpenDocument::settled)
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
    ///
    /// Wraps [`Self::carry_out`] purely to keep [`Self::last_error`] in step, so that
    /// every producer — keyboard, toolbar, picker, control channel — reports failure
    /// the same way rather than each remembering to.
    fn dispatch(&mut self, ctx: &egui::Context, command: Command) -> DispatchResult {
        let result = self.carry_out(ctx, command);
        match &result {
            DispatchResult::Failed(message) => self.last_error = Some(message.clone()),
            // A new or closed document is the natural point to clear it: whatever the
            // message was about is no longer what is on screen.
            DispatchResult::Opened | DispatchResult::Closed => self.last_error = None,
            _ => {}
        }
        result
    }

    /// Applies a page edit and rebuilds what depends on the order.
    ///
    /// One place for all of them, because every edit has the same three consequences:
    /// the column has to be laid out again, the scroll position may now be past the end
    /// of a shorter document, and an unchanged order is `Unchanged` rather than an
    /// error. Missing any of those in one command and not another is exactly how the
    /// view and the document drift apart.
    fn edit(&mut self, change: impl FnOnce(&mut PageOrder) -> bool) -> DispatchResult {
        let Some(open) = &mut self.open else {
            return DispatchResult::Failed("nothing is open".to_owned());
        };
        if !change(&mut open.order) {
            return DispatchResult::Unchanged;
        }
        open.relayout();
        let pages = open.order.len();

        // Deleting pages can leave the viewport past the end of the document. Clamping
        // through the normal command keeps one definition of "how far can we scroll".
        let here = self.state.scroll_top_pt();
        porpoise_view::apply(
            &mut self.state,
            Self::layout_of(&self.open),
            self.viewport,
            ViewCommand::ScrollTo { points: here },
        );
        self.emit(|| Event::PagesReordered { page_count: pages });
        DispatchResult::Edited
    }

    /// Starts writing the document out, off the UI thread.
    fn begin_save(&mut self, destination: PathBuf, overwrite: Overwrite) -> DispatchResult {
        let Some(open) = &self.open else {
            return DispatchResult::Failed("nothing is open".to_owned());
        };
        if self.saver.is_busy() {
            return DispatchResult::Failed("a save is already running".to_owned());
        }
        // Saving an unedited document over itself would rewrite the file for no gain —
        // and not even byte-identically, since the writer makes its own choices about
        // object encoding. See `docs/goal-4-plan.md` §5a.
        if open.order.is_unedited() && destination == open.path {
            return DispatchResult::Unchanged;
        }
        if self
            .saver
            .start(&open.path, &open.order, &destination, overwrite)
        {
            DispatchResult::Saving
        } else {
            DispatchResult::Failed("a save is already running".to_owned())
        }
    }

    fn carry_out(&mut self, ctx: &egui::Context, command: Command) -> DispatchResult {
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
            Command::MovePage { from, to } => {
                self.edit(|order| order.move_page(from.index(), to.index()))
            }
            Command::DeletePage { page } => self.edit(|order| order.remove(page.index())),
            Command::Undo => self.edit(PageOrder::undo),
            Command::Save => {
                let Some(open) = &self.open else {
                    return DispatchResult::Failed("nothing is open".to_owned());
                };
                let destination = open.path.clone();
                self.begin_save(destination, Overwrite::Allow)
            }
            Command::SaveAs { path } => self.begin_save(path, Overwrite::Refuse),
            Command::SetThumbnails { visible } => {
                if self.thumbnails == visible {
                    return DispatchResult::Unchanged;
                }
                self.thumbnails = visible;
                DispatchResult::View(Outcome::Changed)
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
            // Ctrl+O is handled here rather than in `command_for_key`, and the reason
            // is the point of the design: opening the dialog is not a command. That
            // function's whole job is to turn a key into one, so a key that instead
            // asks a person a question does not belong in it.
            if opens_the_picker(key, modifiers) {
                self.picker.open();
                continue;
            }
            // Page edits are handled here rather than in `command_for_key` because
            // they need to know which page is on screen, and that function is pure by
            // design. It decides *what was asked for*; this turns it into a command.
            if let Some(edit) = edit_for_key(key, modifiers) {
                if let Some(command) = self.command_for_edit(edit) {
                    self.dispatch(ctx, command);
                }
                continue;
            }
            if let Some(command) = command_for_key(key, modifiers, mode) {
                self.dispatch(ctx, command);
            }
        }
    }

    /// Turns a page-edit key press into a command against the page on screen.
    ///
    /// `None` when the edit does not apply — the first page cannot move earlier, and
    /// nothing can be edited with no document open. Returning `None` rather than
    /// dispatching a command that would be refused keeps the control channel's
    /// `unchanged` replies meaning "you asked for something already true" rather than
    /// "a key did nothing".
    fn command_for_edit(&self, edit: EditKey) -> Option<Command> {
        let open = self.open.as_ref()?;
        let here = PageNumber::from_index(self.view().current_page());
        match edit {
            EditKey::MoveEarlier => Some(Command::MovePage {
                from: here,
                to: PageNumber::new(here.get().checked_sub(1)?)?,
            }),
            EditKey::MoveLater if here.get() < open.order.len() => Some(Command::MovePage {
                from: here,
                to: PageNumber::new(here.get() + 1)?,
            }),
            EditKey::MoveLater => None,
            EditKey::Undo => Some(Command::Undo),
            EditKey::Save => Some(Command::Save),
            EditKey::ToggleThumbnails => Some(Command::SetThumbnails {
                visible: !self.thumbnails,
            }),
        }
    }

    /// Reports a finished save. Never blocks.
    fn collect_save(&mut self) {
        let Some(saved) = self.saver.poll() else {
            return;
        };
        let where_to = saved.path.display().to_string();
        match saved.error {
            None => {
                self.last_error = None;
                self.emit(|| Event::Saved { path: where_to });
            }
            Some(error) => {
                tracing::warn!(path = %saved.path.display(), %error, "could not save");
                // Visible, not just logged. A save that quietly failed would leave
                // somebody believing their reordering is on disk.
                self.last_error = Some(error.clone());
                self.emit(|| Event::SaveFailed { error });
            }
        }
    }

    /// Turns a chosen path into an `Open` command. Never blocks.
    fn collect_picked_file(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.picker.poll() {
            // Through the normal dispatch, so it emits `DocumentOpened`, reaches the
            // control channel, and reports failure exactly like an `open` from any
            // other producer.
            self.dispatch(ctx, Command::Open { path });
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

        let wanted = request_order(visible, PREFETCH_PAGES, open.layout.page_count());
        let scale = bucket.scale() * pixels_per_point;
        let tag = i64::from(bucket.rung());

        for position in wanted {
            // `request_order` works in display positions, because that is what the
            // layout and the viewport are in. The renderer and the cache work in source
            // pages, so this is where the two meet.
            let Some(page) = open.order.source_of(position) else {
                continue;
            };
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
                ui.label("No document open. Press Ctrl+O, or pass a path on the command line.")
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
                .filter_map(|position| {
                    // `position` is where the page sits in the column; `page` is which
                    // page of the source document that is. They differ after any edit,
                    // so the layout is asked in positions and the geometry, cache and
                    // renderer in source pages.
                    let page = open.order.source_of(position)?;
                    let top_pt = open.layout.page_top_pt(position)?;
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
            //
            // The window is a range of display *positions*, and the cache is keyed by
            // *source* page — so the positions have to be resolved before comparing.
            // Comparing the two directly evicts textures for pages that are on screen
            // and keeps ones that are not, which after a reorder shows up as pages
            // flashing grey while scrolling near an edit.
            let low = visible.start.saturating_sub(RETAIN_PAGES);
            let high = visible.end.saturating_add(RETAIN_PAGES);
            let keep: Vec<usize> = (low..high)
                .filter_map(|position| open.order.source_of(position))
                .collect();
            open.cache.retain_pages(|page| keep.contains(&page));
        });

        // Keep frames coming while anything is still being drawn.
        if self.open.as_ref().is_some_and(|open| !open.settled()) {
            ctx.request_repaint();
        }
    }

    /// Draws the page grid and dispatches any drag that landed.
    fn draw_thumbnails(&mut self, ui: &mut egui::Ui) {
        let pixels_per_point = ui.ctx().pixels_per_point();
        let current = self.view().current_page();
        let Some(open) = &mut self.open else {
            ui.label("No document open.");
            return;
        };

        let mut grid = Grid {
            order: &open.order,
            document: &open.document,
            cache: &mut open.cache,
            pool: &open.pool,
            in_flight: &mut open.in_flight,
            current,
            pixels_per_point,
        };
        let dropped = thumbnails::draw(ui, &mut grid);

        // Through the normal dispatch, so a drag is indistinguishable from an agent
        // sending `move_page` — which is the whole point of the command model.
        if let Some((from, to)) = dropped {
            let ctx = ui.ctx().clone();
            if let (Some(from), Some(to)) = (
                PageNumber::new(from.saturating_add(1)),
                PageNumber::new(to.saturating_add(1)),
            ) {
                self.dispatch(&ctx, Command::MovePage { from, to });
            }
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        // Collected rather than dispatched inline, because dispatch needs
        // `&mut self` while `ui` is borrowed. Note every button produces the same
        // command an agent would send — there is no click-only path.
        let mut issued: Vec<Command> = Vec::new();
        let zoom_target = self.state.zoom_target();
        let paged = self.state.scroll_mode() == ScrollMode::Paged;
        // Not pushed onto `issued`: the dialog is not a command. Collected the same
        // way only because `ui` holds the borrow.
        let mut open_picker = false;

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.picker.is_open(), egui::Button::new("Open…"))
                .on_hover_text("Open a PDF (Ctrl+O)")
                .clicked()
            {
                open_picker = true;
            }
            ui.separator();

            if ui
                .selectable_label(self.thumbnails, "Pages")
                .on_hover_text("Show the page grid, to drag pages around (Ctrl+T)")
                .clicked()
            {
                issued.push(Command::SetThumbnails {
                    visible: !self.thumbnails,
                });
            }
            ui.separator();

            if ui.button("⏮").on_hover_text("First page (Home)").clicked() {
                issued.push(ViewCommand::FirstPage.into());
            }
            if ui.button("⏭").on_hover_text("Last page (End)").clicked() {
                issued.push(ViewCommand::LastPage.into());
            }
            ui.separator();

            // Page editing. Every one of these produces the same command an agent
            // would send, so there is nothing here a script cannot do.
            let here = PageNumber::from_index(self.view().current_page());
            let pages = self.open.as_ref().map_or(0, |open| open.order.len());
            let can_edit = pages > 0;

            if ui
                .add_enabled(can_edit && here.get() > 1, egui::Button::new("Up"))
                // Words, not arrow glyphs. U+2191/U+2193 are missing from egui's
                // bundled fonts and rendered as empty boxes -- caught by looking at a
                // capture of the real toolbar rather than by any test.
                .on_hover_text("Move this page earlier (Ctrl+Up)")
                .clicked()
                && let Some(to) = PageNumber::new(here.get() - 1)
            {
                issued.push(Command::MovePage { from: here, to });
            }
            if ui
                .add_enabled(can_edit && here.get() < pages, egui::Button::new("Down"))
                .on_hover_text("Move this page later (Ctrl+Down)")
                .clicked()
                && let Some(to) = PageNumber::new(here.get() + 1)
            {
                issued.push(Command::MovePage { from: here, to });
            }
            if ui
                .add_enabled(pages > 1, egui::Button::new("Delete"))
                .on_hover_text("Delete this page")
                .clicked()
            {
                issued.push(Command::DeletePage { page: here });
            }
            if ui
                .add_enabled(
                    self.open.as_ref().is_some_and(|open| open.order.can_undo()),
                    egui::Button::new("Undo"),
                )
                .on_hover_text("Undo the last page edit (Ctrl+Z)")
                .clicked()
            {
                issued.push(Command::Undo);
            }

            let edited = self
                .open
                .as_ref()
                .is_some_and(|open| !open.order.is_unedited());
            if ui
                .add_enabled(edited && !self.saver.is_busy(), egui::Button::new("Save"))
                .on_hover_text("Write the changes over the original (Ctrl+S)")
                .clicked()
            {
                issued.push(Command::Save);
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
        if open_picker {
            self.picker.open();
        }
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
                    ui.label("no document — Ctrl+O to open one");
                }
            }

            // Editing state, before the error, so a save failure reads next to it.
            if let Some(destination) = self.saver.destination() {
                ui.separator();
                ui.label(format!(
                    "saving to {}…",
                    destination.file_name().map_or_else(
                        || destination.display().to_string(),
                        |name| name.to_string_lossy().into_owned()
                    )
                ));
            } else if self
                .open
                .as_ref()
                .is_some_and(|open| !open.order.is_unedited())
            {
                ui.separator();
                ui.colored_label(ui.visuals().warn_fg_color, "unsaved changes");
            }

            // Last, and on every path: a failure with no document open is exactly the
            // case the picker creates, so it must not live inside the `Some` arm.
            if let Some(error) = &self.last_error {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, error);
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
        // `is_none_or`, not `is_some_and`: with no document there is no pipeline to
        // wait for, so the window is as ready as it will ever be. Under
        // `is_some_and` an empty window could never be captured at all — it would
        // spin out the whole frame budget and report "no screenshot arrived". That
        // did not matter while a path was mandatory; since Goal 3 an empty window is
        // how the program starts.
        let ready = self
            .open
            .as_ref()
            .is_none_or(|open| open.settled() && !open.cache.is_empty());
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
    /// A page edit took effect. The file on disk is unchanged.
    Edited,
    /// A save has *started*. Like a capture, the file does not exist yet.
    Saving,
    /// The command asked for something that was already true.
    Unchanged,
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
            Self::Edited => Reply::ok(id, "edited"),
            // Started, not finished. A 400-page save takes about a second, so an agent
            // that treated this as completion would read a file that is not there yet;
            // it has to wait for the `saved` event or for `idle`.
            Self::Saving => Reply::ok(id, "saving"),
            Self::Unchanged => Reply::ok(id, "unchanged"),
            Self::Quitting => Reply::ok(id, "quitting"),
            Self::Failed(error) => Reply::failed(id, error),
        }
    }
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
        self.collect_picked_file(ctx);
        self.collect_save();
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
        // Before the pages, so the central area is what is left over.
        if self.thumbnails {
            egui::Panel::left("thumbnails")
                .resizable(true)
                .default_size(300.0)
                .show(ui, |ui| self.draw_thumbnails(ui));
        }
        self.draw_pages(ui);

        // Our own cost, as distinct from the frame interval. If this stays well
        // under the frame budget, the pipeline has headroom.
        self.timing.ui_ms = started.elapsed().as_secs_f32() * 1000.0;
    }
}
