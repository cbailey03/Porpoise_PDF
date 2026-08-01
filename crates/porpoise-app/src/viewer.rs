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
//! | Module | What | Tested |
//! |---|---|---|
//! | [`crate::input`] | Key press or file drop to [`Command`] — pure | ✅ |
//! | [`crate::edits`] | Which page edits are possible right now — pure | ✅ |
//! | [`crate::confirm`] | Which commands would discard unsaved work — pure | ✅ |
//! | [`crate::failure`] | Whether a failed render is worth retrying — pure policy | ✅ |
//! | [`crate::label`] | How a path is shown in limited space — pure | ✅ |
//! | [`crate::tiles`] | Rasterized page to egui texture — the GPU boundary | ✅ |
//! | [`crate::picker`] | The file dialog, off the frame loop | ✅ |
//! | [`crate::saver`] | Writing the document out, off the frame loop | ✅ |
//! | [`crate::thumbnails`] | The page grid's arithmetic | ✅ |
//! | [`crate::chrome`] | Toolbar, status bar and the two overlays — painting only | ❌ |
//! | [`crate::devtools`] | Frame timing and window capture | ✅ |
//!
//! [`crate::chrome`] is the one extraction that gained no tests, and that is honest
//! rather than an oversight: it needs a live `egui::Context` exactly as this module does.
//! It was moved for navigability. The part of the toolbar that *was* worth testing — which
//! buttons should be live — went to [`crate::edits`] instead, and has tests.
//!
//! What remains here has no unit tests either, for the same reason: everything left needs
//! a live context, a GPU adapter, or both. It is covered by `tests/control.rs`, which
//! drives the real binary over a real pipe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use porpoise_doc::{Document, Overwrite, PageGeometry, PageOrder, Source};
use porpoise_render::{HayroRenderer, RenderPool, RenderedPage};
use porpoise_view::{
    CacheKey, MAX_SCALE, MIN_SCALE, Outcome, PAGE_GAP_PT, PageCache, PageNumber, ScrollLayout,
    ScrollMode, View, ViewCommand, ViewState, Viewport, ZoomBucket, ZoomTarget, request_order,
};

use crate::chrome;
use crate::command::Command;
use crate::confirm::{self, Answer, Guard, Intent};
use crate::control::Control;
use crate::devtools::{
    FrameTiming, ScreenshotOutcome, ScreenshotRequest, Screenshotter, ScrollBenchmark,
};
use crate::edits::{Edits, Situation};
use crate::failure::Failure;
use crate::input::{
    DropAction, DropZone, EditKey, PageTurns, Wheel, command_for_key, drop_action, edit_for_key,
    opens_the_picker, wheel_is_for_the_pages,
};
use crate::label::file_label;
use crate::picker::{FilePicker, Purpose};
use crate::protocol::{Event, Reply, RequestBody, Snapshot, StagedSnapshot};
use crate::queue::RenderQueue;
use crate::retain;
use crate::saver::Saver;
use crate::search::PageFilter;
use crate::selection::Selection;
use crate::stage::StageId;
use crate::thumbnails::{self, Grid, GridMode, StagedInfo, StagedTab};
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

/// One document contributing pages to what is open, and where it came from.
///
/// A merge (`docs/goal-5-plan.md` §3) means the pages on screen can come from more
/// than one file, so `path` and `document` — a single pair before this goal —
/// become entries in [`OpenDocument::files`] instead of fields of it.
struct OpenFile {
    path: PathBuf,
    document: Arc<Document>,
}

/// One currently staged document: its stable [`StageId`] and which of
/// [`OpenDocument::files`] it points to.
struct Staged {
    id: StageId,
    document: usize,
}

/// Everything that belongs to one open document.
///
/// Grouped so that opening another is a single replacement rather than eight
/// fields to remember to reset — the kind of bookkeeping that goes wrong once a
/// ninth is added.
struct OpenDocument {
    /// Every contributing file, index 0 being the one the viewer was opened with.
    /// `Source::document` in [`Self::order`] indexes this.
    files: Vec<OpenFile>,
    /// What order the pages are shown in. Only the identity mapping — position `n`
    /// is page `n` of document 0 — before the very first edit; a move or a delete
    /// breaks that identity immediately, and an insert is the only edit that can
    /// make a position's source name anything other than document 0.
    ///
    /// Everything that rasterizes, caches or measures a page goes through this to turn
    /// a *display position* into a *source* — a document and a page within it; see
    /// `porpoise-doc`'s `order` module for why that distinction gets its own crossing
    /// point.
    order: PageOrder,
    /// Page positions laid out in a column. Rebuilt whenever [`Self::order`] changes,
    /// because the column is in display order while geometry is in source order.
    layout: ScrollLayout,
    /// One pool serving every contributing file, sized once regardless of how many
    /// there are. See `docs/goal-5-plan.md` §4 for why this is not one pool per file.
    pool: RenderPool,
    cache: PageCache<egui::TextureHandle>,
    /// Requests submitted but not yet returned, so a page is not queued twice.
    in_flight: Vec<CacheKey>,
    /// Failures keyed by rasterization, not by page, so a different zoom is still
    /// attempted. A timeout keeps a retry budget; see [`Failure::from_error`].
    failures: HashMap<CacheKey, Failure>,
    /// The rung work was last submitted for, to notice when zoom moves.
    submitted_bucket: ZoomBucket,
    /// Every document currently staged for the merge tab, in the order each was
    /// staged — `docs/goal-5-plan.md` §10.7, §10.12.
    ///
    /// A `Vec` rather than a single pointer: staging never removes a `files`
    /// entry once added (the same reason `add_file` never reuses one — see its
    /// own doc comment), and more than one document can now be staged at once.
    /// Each entry's [`StageId`] is permanent — clearing one never reassigns its
    /// number to a different document.
    staging: Vec<Staged>,
    /// The [`StageId`] the next staged document will be given. Only ever
    /// increases — see that type's own docs for why an id, once handed out, is
    /// never handed out again.
    next_stage_id: StageId,
}

/// Refusal for a command that needs a document when none is open.
///
/// One string rather than three copies of it, so the three commands that can hit this
/// cannot start explaining it differently.
const NOTHING_OPEN: &str = "nothing is open";

/// Paths from a file drop that has landed.
///
/// An **edge**: egui drains these each frame, so reading it once opens the file once
/// rather than reopening it every frame for as long as the window lives.
fn dropped_paths(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.input(|input| {
        input
            .raw
            .dropped_files
            .iter()
            .filter_map(|file| file.path.clone())
            .collect()
    })
}

/// Paths from a drag still in the air over the window.
///
/// A **level**, unlike [`dropped_paths`]: egui keeps these until the drop lands or the
/// drag leaves, and both of those wake the frame loop. So one repaint paints the hint and
/// one clears it. Both behaviours were checked in egui 0.35's `RawInput::take` rather than
/// assumed, because the API gives no hint that the two fields differ.
fn hovered_paths(ctx: &egui::Context) -> Vec<PathBuf> {
    ctx.input(|input| {
        input
            .raw
            .hovered_files
            .iter()
            .filter_map(|file| file.path.clone())
            .collect()
    })
}

/// The geometry of one displayed page, resolved across whichever file it belongs
/// to. Shared by [`geometry_in_display_order`] and [`OpenDocument::geometry_of`],
/// which differ only in whether they resolve one source or every one `order`
/// currently names.
fn geometry_of(files: &[OpenFile], source: Source) -> Option<PageGeometry> {
    files
        .get(source.document)?
        .document
        .geometry()
        .get(source.page)
        .copied()
}

/// Page sizes in display order, for laying out the scrolling column.
fn geometry_in_display_order(files: &[OpenFile], order: &PageOrder) -> Vec<PageGeometry> {
    order
        .as_slice()
        .iter()
        .filter_map(|source| geometry_of(files, *source))
        .collect()
}

impl OpenDocument {
    fn new(path: PathBuf, document: Document, bucket: ZoomBucket) -> Self {
        let document = Arc::new(document);
        let order = PageOrder::identity(document.page_count());
        let files = vec![OpenFile {
            path,
            document: Arc::clone(&document),
        }];
        let layout =
            ScrollLayout::vertical(&geometry_in_display_order(&files, &order), PAGE_GAP_PT);
        let pool = RenderPool::new(
            document,
            HayroRenderer::new(),
            RenderPool::recommended_workers(),
            JOB_TIMEOUT,
        );
        Self {
            files,
            order,
            layout,
            pool,
            cache: PageCache::new(TEXTURE_BUDGET_BYTES),
            in_flight: Vec::new(),
            failures: HashMap::new(),
            submitted_bucket: bucket,
            staging: Vec::new(),
            next_stage_id: StageId::FIRST,
        }
    }

    /// The file this was opened with — what "Save" writes over, and what the window
    /// title and the snapshot's `document` field name.
    fn primary_path(&self) -> &Path {
        // `files` always has at least one entry: `new` puts it there and nothing
        // ever removes an entry, only appends.
        #[expect(
            clippy::indexing_slicing,
            reason = "files always has at least one entry; see OpenDocument::new"
        )]
        &self.files[0].path
    }

    /// Every contributing file's path, in the order [`PageOrder::source_lens`]
    /// indexes them — what saving reads from.
    fn source_paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(|file| file.path.clone()).collect()
    }

    /// Registers another file's pages as available to show, returning the document
    /// index a fresh call to [`PageOrder::append`] or [`PageOrder::stage`] should
    /// use for it.
    ///
    /// Always a new entry, never reused even if the path matches one already open:
    /// the file may have changed on disk since it was first read, and treating it as
    /// unchanged would be exactly the kind of quiet incorrectness this project
    /// refuses to ship. See `docs/goal-5-plan.md` §9.
    fn add_file(&mut self, path: PathBuf, document: Document) -> usize {
        let document = Arc::new(document);
        let pool_index = self.pool.add_document(Arc::clone(&document));
        self.files.push(OpenFile { path, document });
        // The pool and `files` are appended to together, in the same call, so their
        // indices agree by construction — this is the one place that could drift.
        debug_assert_eq!(
            pool_index,
            self.files.len() - 1,
            "the render pool and the file list disagreed about the next index"
        );
        self.files.len() - 1
    }

    /// The geometry of one displayed page, resolved across whichever file it
    /// belongs to.
    fn geometry_of(&self, source: Source) -> Option<PageGeometry> {
        geometry_of(&self.files, source)
    }

    /// The `files` index `stage` points to, if it is currently staged.
    fn staged_document(&self, stage: StageId) -> Option<usize> {
        self.staging
            .iter()
            .find(|staged| staged.id == stage)
            .map(|staged| staged.document)
    }

    /// How many pages `stage`'s document has, or 0 if it is not currently staged
    /// — what [`Viewer::staged_filter`] parses a query against.
    fn staged_page_count(&self, stage: StageId) -> usize {
        self.staged_document(stage)
            .and_then(|document| self.files.get(document))
            .map_or(0, |file| file.document.page_count())
    }

    /// The `Source`s `stage`'s pane is currently showing, if it is staged:
    /// `(0..page_count).map(|page| Source { document, page })` — the same list
    /// `crate::thumbnails::draw_staged_grid` builds for itself.
    ///
    /// Recomputed rather than cached: it is cheap, and a cached copy would need its
    /// own invalidation to remember whenever the staged document changes.
    fn staged_sources(&self, stage: StageId) -> Option<Vec<Source>> {
        let document = self.staged_document(stage)?;
        Some(
            (0..self.staged_page_count(stage))
                .map(|page| Source { document, page })
                .collect(),
        )
    }

    /// Registers `document` as a newly staged file, returning its fresh
    /// [`StageId`]. Never fails and never replaces an existing entry — every
    /// call adds one, the same way `add_file` always adds a fresh `files`
    /// entry rather than reusing one.
    fn stage(&mut self, document: usize) -> StageId {
        let id = self.next_stage_id;
        self.next_stage_id = id.next();
        self.staging.push(Staged { id, document });
        id
    }

    /// Forgets `stage`, if it is currently staged. Returns whether anything
    /// changed — never removes anything from [`Self::files`] itself, only the
    /// pointer to it, the same as the single-slot version this generalizes.
    fn unstage(&mut self, stage: StageId) -> bool {
        let before = self.staging.len();
        self.staging.retain(|staged| staged.id != stage);
        self.staging.len() != before
    }

    /// Every currently staged document's id and path, in the order each was
    /// staged — what the merge tab's tab strip draws, and what a snapshot lists.
    fn staged_summaries(&self) -> impl Iterator<Item = (StageId, &Path)> {
        self.staging
            .iter()
            .filter_map(|staged| Some((staged.id, self.files.get(staged.document)?.path.as_path())))
    }

    /// Which [`StageId`] a `files` index belongs to, if it is currently staged —
    /// translates the [`crate::thumbnails::Inserted`] drag payload's raw document
    /// index back to a stable id, since that payload stays decoupled from the
    /// stage concept the same way [`StagedInfo::document`] already is.
    fn stage_for_document(&self, document: usize) -> Option<StageId> {
        self.staging
            .iter()
            .find(|staged| staged.document == document)
            .map(|staged| staged.id)
    }

    /// Rebuilds the column after an edit.
    ///
    /// The layout is in display order and each file's geometry is in its own source
    /// order, so any change to [`Self::order`] makes the column wrong until this
    /// runs. Cached textures are deliberately *not* touched: they are keyed by
    /// source, so moving page 300 to the front costs nothing to redraw.
    fn relayout(&mut self) {
        self.layout = ScrollLayout::vertical(
            &geometry_in_display_order(&self.files, &self.order),
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
        Some((path, _)) => file_label(path),
        None => "no document".to_owned(),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_min_inner_size([320.0, 240.0])
            .with_maximized(true)
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
    /// Where the thumbnail strip was drawn last frame, in window coordinates.
    ///
    /// So that a wheel gesture over the strip scrolls the strip instead of turning the
    /// page. Last frame's rectangle, because the wheel is read before this frame's panels
    /// are laid out — a panel that moved in between could mis-route one gesture, which is
    /// why this is a plain rectangle test and not a claim about the current layout.
    ///
    /// The strip rather than the pages, deliberately: see [`wheel_is_for_the_pages`].
    thumbnails_rect: egui::Rect,
    /// Where the merge tab's staging viewport was drawn last frame, in window
    /// coordinates — `None` whenever [`GridMode::Merge`] did not draw one.
    ///
    /// The same last-frame caveat [`Self::thumbnails_rect`] carries, for the same
    /// reason: [`Self::drop_zone`] is asked before this frame's panels are laid out.
    staging_rect: Option<egui::Rect>,
    /// One page turn per wheel gesture, in paged mode. See [`PageTurns`].
    page_turns: PageTurns,
    /// Source pages the thumbnail grid drew this frame, for [`Self::retain_textures`].
    ///
    /// Emptied as it is used, so a frame that does not draw the grid cannot leave a stale
    /// list keeping textures alive. Unlike [`Self::thumbnails_rect`], which is read *before*
    /// the grid draws and is therefore last frame's, this is written and read within one
    /// frame.
    grid_pages: Vec<Source>,

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

    /// What clicking a page in the grid does. See [`GridMode`].
    ///
    /// Kept here rather than in the panel, so closing the grid and reopening it does not
    /// silently put a click back to meaning something else.
    grid_mode: GridMode,

    /// Pages picked out in the grid. See [`crate::selection`].
    ///
    /// Outside `open`, so it is not one of the things a reorder has to remember to fix up
    /// — it holds source pages and follows them on its own.
    selection: Selection,

    /// Pages picked out in each staged document's own pane, keyed by
    /// [`StageId`]. Separate from [`Self::selection`], since picking a page
    /// there says nothing about the main document — see `docs/goal-5-plan.md`
    /// §10.4. One entry per stage rather than one shared instance, so switching
    /// which pane is active does not lose what was picked in another — an entry
    /// simply does not exist yet for a stage nothing has picked in.
    ///
    /// Updated directly by a click or marquee in the pane, the same way
    /// `Self::selection` is — but also settable explicitly over the control
    /// channel via `Command::SetStagedSelection`, unlike `selection`'s
    /// `SetSelection`-only path: nothing implicitly consults "whatever is
    /// picked out" the way `DeletePage` consults `selection`, so this stayed
    /// local state at first, but a `path`-then-`stage` key that survives more
    /// than one simultaneously staged document made it worth exposing to an
    /// agent too. See `docs/goal-5-plan.md` §10.11, §10.12.
    staging_selections: HashMap<StageId, Selection>,
    /// Which staged document's pane the merge tab's single staging viewport
    /// currently shows. `None` only when nothing is staged at all — see
    /// `docs/goal-5-plan.md` §10.12.
    active_stage: Option<StageId>,

    /// What is typed in the grid's search box. Empty when nothing is.
    ///
    /// The text is the state; the pages it names are re-derived each frame by
    /// [`PageFilter::parse`], which is cheap and means there is no resolved list to fall
    /// out of step with the document after an edit.
    page_filter: String,

    /// A request held back because it would discard unsaved page changes.
    ///
    /// See [`crate::confirm`]. `None` whenever nothing is waiting, which is almost
    /// always.
    guard: Option<Guard>,

    /// Whether the window has been told to close for real.
    ///
    /// Load-bearing, not defensive. `ViewportCommand::Close` comes back round as a
    /// close *request* on the next frame — egui-winit pushes it onto the same event
    /// queue the X button feeds — so a close interception with no way to tell "the
    /// person clicked X" from "we asked to close" re-raises the question forever and the
    /// window can never be shut. Checked in egui 0.35's source rather than discovered.
    quitting: bool,

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
            thumbnails_rect: egui::Rect::NOTHING,
            staging_rect: None,
            page_turns: PageTurns::default(),
            grid_pages: Vec::new(),
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
            grid_mode: GridMode::default(),
            selection: Selection::default(),
            staging_selections: HashMap::new(),
            active_stage: None,
            page_filter: String::new(),
            guard: None,
            quitting: false,
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
                .map(|open| open.primary_path().display().to_string()),
            view: self.view().snapshot(),
            pages_cached: self.open.as_ref().map_or(0, |open| open.cache.len()),
            cache_bytes: self.open.as_ref().map_or(0, |open| open.cache.used_bytes()),
            renders_in_flight: self.open.as_ref().map_or(0, |open| open.in_flight.len()),
            failed_pages,
            last_error: self.last_error.clone(),
            thumbnails: self.thumbnails,
            grid_mode: self.grid_mode,
            staged: self.staged_snapshots(),
            active_stage: self.active_stage,
            page_filter: self.page_filter.clone(),
            filtered_pages: self.filtered_pages(),
            selection: self.selected_pages(),
            unsaved_changes: self.unsaved_changes(),
            awaiting_answer: match &self.guard {
                Some(Guard::Asking(intent)) => Some(intent.describe()),
                // A `Saving` guard has already been answered; it is waiting on the disk,
                // not on anybody, and `saving_to` below is what reports it.
                Some(Guard::Saving(_)) | None => None,
            },
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
        //
        // So is a question nobody has answered. The program itself has nothing left to
        // do, but the thing that was asked for has not happened — and `idle` means
        // "everything you asked for is done", not "the CPU is quiet". A client that read
        // idle after `quit` would believe the window had closed.
        no_pending_move
            && self.guard.is_none()
            && !self.saver.is_busy()
            && self.open.as_ref().is_none_or(OpenDocument::settled)
    }

    /// Whether the page order differs from the file it came from.
    fn unsaved_changes(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|open| !open.order.is_unedited())
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
        // In front of `carry_out`, so every producer — the X button, the keyboard, the
        // toolbar, a file drop, the control channel — is covered by one check rather
        // than each remembering to make it. See [`crate::confirm`] for why this guards
        // the command instead of the gesture.
        if self.unsaved_changes()
            && !self.quitting
            && let Some(intent) = confirm::intent_of(&command)
        {
            // A newer request replaces an older unanswered one. The alternative is
            // stacking questions, and nobody wants to answer two.
            self.guard = Some(Guard::Asking(intent));
            return DispatchResult::NeedsAnswer;
        }

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
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
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
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        };
        // Saving an unedited document over itself would rewrite the file for no gain —
        // and not even byte-identically, since the writer makes its own choices about
        // object encoding. See `docs/goal-4-plan.md` §5a.
        //
        // Deliberately ahead of the busy check below, so "there is nothing to write" wins
        // over "a save is already running" when both are true. It is the more accurate of
        // the two, and it is the one that does not put an error in the status bar.
        if open.order.is_unedited() && destination == open.primary_path() {
            return DispatchResult::Unchanged;
        }
        // `Saver::start` refuses only when a save is already running, so this is the one
        // place that condition is tested. It used to be checked here *and* above, with
        // the same message written out twice.
        if self
            .saver
            .start(&open.source_paths(), &open.order, &destination, overwrite)
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
            Command::Open { path } => self.open_document(path),
            Command::Close => self.close_document(),
            Command::InsertFile { path } => self.insert_file(path),
            Command::StageDocument { path } => self.stage_document(path),
            Command::ClearStaging { stage } => self.clear_staging(stage),
            Command::InsertPages { stage, pages, at } => {
                self.insert_pages_from_staging(stage, &pages, at)
            }
            Command::MovePage { from, to } => {
                self.edit(|order| order.move_page(from.index(), to.index()))
            }
            Command::MovePages { from, to } => {
                let positions: Vec<usize> = from.iter().map(|page| page.index()).collect();
                self.edit(|order| order.move_pages(&positions, to.index()))
            }
            Command::DeletePage { page } => self.edit(|order| order.remove(page.index())),
            Command::DeletePages { pages } => {
                let positions: Vec<usize> = pages.iter().map(|page| page.index()).collect();
                self.edit(|order| order.remove_pages(&positions))
            }
            Command::SetPageFilter { query } => {
                if self.page_filter == query {
                    return DispatchResult::Unchanged;
                }
                self.page_filter = query;
                // For the reason closing the grid clears it: **Delete** acts on the
                // selection, and pages hidden behind a query are pages nobody can see it
                // is about to remove. Costs re-picking after a search, which is the
                // cheaper mistake.
                self.selection.clear();
                DispatchResult::View(Outcome::Changed)
            }
            Command::SetSelection { pages } => {
                let Some(open) = &self.open else {
                    return DispatchResult::Failed(NOTHING_OPEN.to_owned());
                };
                let positions: Vec<usize> = pages.iter().map(|page| page.index()).collect();
                let mut wanted = self.selection.clone();
                wanted.set_positions(open.order.as_slice(), &positions);
                if wanted == self.selection {
                    return DispatchResult::Unchanged;
                }
                self.selection = wanted;
                DispatchResult::View(Outcome::Changed)
            }
            Command::SetStagedSelection { stage, pages } => {
                let Some(open) = &self.open else {
                    return DispatchResult::Failed(NOTHING_OPEN.to_owned());
                };
                let Some(shown) = open.staged_sources(stage) else {
                    return DispatchResult::Failed(format!(
                        "stage {stage} is not currently staged"
                    ));
                };
                let positions: Vec<usize> = pages.iter().map(|page| page.index()).collect();
                let current = self
                    .staging_selections
                    .get(&stage)
                    .cloned()
                    .unwrap_or_default();
                let mut wanted = current.clone();
                wanted.set_positions(&shown, &positions);
                if wanted == current {
                    return DispatchResult::Unchanged;
                }
                self.staging_selections.insert(stage, wanted);
                DispatchResult::View(Outcome::Changed)
            }
            Command::SetActiveStage { stage } => {
                let Some(open) = &self.open else {
                    return DispatchResult::Failed(NOTHING_OPEN.to_owned());
                };
                if open.staged_document(stage).is_none() {
                    return DispatchResult::Failed(format!(
                        "stage {stage} is not currently staged"
                    ));
                }
                if self.active_stage == Some(stage) {
                    return DispatchResult::Unchanged;
                }
                self.active_stage = Some(stage);
                DispatchResult::View(Outcome::Changed)
            }
            Command::Undo => self.edit(PageOrder::undo),
            Command::Save => {
                let Some(open) = &self.open else {
                    return DispatchResult::Failed(NOTHING_OPEN.to_owned());
                };
                let destination = open.primary_path().to_path_buf();
                self.begin_save(destination, Overwrite::Allow)
            }
            Command::SaveAs { path } => self.begin_save(path, Overwrite::Refuse),
            // Both of these can hide the selection, and a selection nobody can see is a
            // trap: **Delete** acts on it, so it has to go when the highlight does.
            // Cleared on the way out rather than ignored while hidden, so the state and
            // the snapshot always say the same thing.
            Command::SetThumbnails { visible } => {
                if self.thumbnails == visible {
                    return DispatchResult::Unchanged;
                }
                self.thumbnails = visible;
                if !visible {
                    self.selection.clear();
                }
                DispatchResult::View(Outcome::Changed)
            }
            Command::SetGridMode { mode } => {
                if self.grid_mode == mode {
                    return DispatchResult::Unchanged;
                }
                self.grid_mode = mode;
                if mode != GridMode::Reorganize {
                    self.selection.clear();
                }
                // Same reasoning, for every staged document's own selection: leaving
                // Merge mode entirely leaves the whole staging concept behind, and a
                // selection nobody can see in any pane is a trap the same way one in
                // the closed page grid is. Switching *within* Merge — which tab is
                // active — deliberately does not reach this: each stage's selection
                // survives that, since you have not left the pane it is in.
                if mode != GridMode::Merge {
                    self.staging_selections.clear();
                }
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
            Command::Answer { choice } => self.answer(ctx, choice),
            Command::Quit => self.quit(ctx),
        }
    }

    // --- The three things that discard a document ---------------------------

    // Split out of `carry_out` so that answering a question can perform one *without*
    // going back through `dispatch`, which would guard it again and ask forever.

    fn open_document(&mut self, path: PathBuf) -> DispatchResult {
        match Document::open(&path) {
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
        }
    }

    fn close_document(&mut self) -> DispatchResult {
        self.open = None;
        self.state = ViewState::new();
        self.emit(|| Event::DocumentClosed);
        DispatchResult::Closed
    }

    /// Adds every page of another file to the end of the document that is open.
    ///
    /// Goes through [`Self::edit`], the same path every other page edit takes, so an
    /// insert relayouts, clamps the scroll position and reports `PagesReordered`
    /// exactly as a move or a delete would — an inserted page is an ordinary one from
    /// the moment it lands. See `docs/goal-5-plan.md` §3 and §6.
    fn insert_file(&mut self, path: PathBuf) -> DispatchResult {
        if self.open.is_none() {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        }
        let document = match Document::open(&path) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not insert document");
                return DispatchResult::Failed(error.to_string());
            }
        };
        let page_count = document.page_count();
        let Some(open) = &mut self.open else {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        };
        let document_index = open.add_file(path, document);
        self.edit(|order| order.append(document_index, page_count))
    }

    /// Opens another document for the merge tab, adding a new pane rather than
    /// replacing any already staged.
    ///
    /// Not an edit: nothing about the open document changes, so unlike
    /// [`Self::insert_file`] this does not go through [`Self::edit`] — no relayout,
    /// no `PagesReordered`. The new document's `OpenDocument::files` entry, like
    /// every one before it, is never reused or reclaimed. See
    /// `docs/goal-5-plan.md` §10.7, §10.12.
    fn stage_document(&mut self, path: PathBuf) -> DispatchResult {
        if self.open.is_none() {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        }
        let document = match Document::open(&path) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not stage document");
                return DispatchResult::Failed(error.to_string());
            }
        };
        let page_count = document.page_count();
        if page_count == 0 {
            return DispatchResult::Failed(format!("{} has no pages", path.display()));
        }
        let Some(open) = &mut self.open else {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        };
        let document_index = open.add_file(path, document);
        let staged = open.order.stage(document_index, page_count);
        debug_assert!(staged, "page_count was checked non-zero above");
        let id = open.stage(document_index);
        // The newly staged document is the one the single visible pane shows,
        // the same as when there was only ever one slot to show. A fresh
        // `StageId` has no entry in `staging_selections` yet, so there is
        // nothing stale to clear the way there was when one pointer stood for
        // whichever document happened to occupy it.
        self.active_stage = Some(id);
        DispatchResult::View(Outcome::Changed)
    }

    /// Closes `stage`'s pane, forgetting the document it staged.
    ///
    /// Its pages already placed by [`Self::insert_pages_from_staging`] are
    /// unaffected — they are ordinary pages of the open document by that point, and
    /// this only forgets the pointer to the staging slot, never
    /// `OpenDocument::files` itself. See `docs/goal-5-plan.md` §10.6, §10.12.
    fn clear_staging(&mut self, stage: StageId) -> DispatchResult {
        let Some(open) = &mut self.open else {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        };
        if !open.unstage(stage) {
            return DispatchResult::Unchanged;
        }
        self.staging_selections.remove(&stage);
        if self.active_stage == Some(stage) {
            // Falls back to whichever remaining stage was staged most
            // recently, rather than leaving the visible pane blank while
            // others are still open — `StageId` only ever increases, so the
            // highest one left is the most recent.
            self.active_stage = open.staged_summaries().map(|(id, _)| id).max();
        }
        DispatchResult::View(Outcome::Changed)
    }

    /// Inserts pages of `stage`'s document into the open document.
    ///
    /// Goes through [`Self::edit`], the same path [`Self::insert_file`] and every
    /// other page edit takes — an inserted page is an ordinary one from the moment
    /// it lands, whichever of the two commands put it there. Refused when `stage`
    /// is not currently staged, which is a real error rather than a no-op: an
    /// agent that calls this without staging anything first has made a mistake
    /// worth telling it about, unlike a move to a position a page is already at.
    fn insert_pages_from_staging(
        &mut self,
        stage: StageId,
        pages: &[PageNumber],
        at: PageNumber,
    ) -> DispatchResult {
        let Some(open) = &self.open else {
            return DispatchResult::Failed(NOTHING_OPEN.to_owned());
        };
        let Some(document) = open.staged_document(stage) else {
            return DispatchResult::Failed(format!("stage {stage} is not currently staged"));
        };
        let positions: Vec<usize> = pages.iter().map(|page| page.index()).collect();
        let at = at.index();
        self.edit(|order| order.insert_pages(document, &positions, at))
    }

    fn quit(&mut self, ctx: &egui::Context) -> DispatchResult {
        // Set before asking, so the close request this produces is recognised as ours
        // when it arrives back next frame. Without it the guard re-fires and the window
        // cannot be closed at all.
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        DispatchResult::Quitting
    }

    /// Carries out a held-back request, past the guard.
    fn carry_out_intent(&mut self, ctx: &egui::Context, intent: Intent) -> DispatchResult {
        match intent {
            Intent::Quit => self.quit(ctx),
            Intent::CloseDocument => self.close_document(),
            Intent::Open(path) => self.open_document(path),
        }
    }

    /// Settles a question about unsaved page changes.
    fn answer(&mut self, ctx: &egui::Context, choice: Answer) -> DispatchResult {
        let intent = match self.guard.take() {
            Some(Guard::Asking(intent)) => intent,
            // A `Saving` guard has already been answered and is waiting on the disk
            // rather than on anybody. Put it back: losing it here would leave the save
            // running and the thing it was for forgotten.
            waiting => {
                self.guard = waiting;
                return DispatchResult::Unchanged;
            }
        };

        match choice {
            Answer::Cancel => DispatchResult::Cancelled,
            Answer::Discard => self.carry_out_intent(ctx, intent),
            Answer::Save => {
                let Some(open) = &self.open else {
                    // Nothing to save, so there is nothing being protected either.
                    return self.carry_out_intent(ctx, intent);
                };
                let destination = open.primary_path().to_path_buf();
                match self.begin_save(destination, Overwrite::Allow) {
                    DispatchResult::Saving => {
                        self.guard = Some(Guard::Saving(intent));
                        DispatchResult::Saving
                    }
                    // Nothing left to write — the order got put back while the question
                    // was up, which an agent can do because the control channel is not
                    // blocked by the box. "Save, then continue" with nothing to save is
                    // just "continue"; leaving the question up here would strand it with
                    // no answer that works.
                    DispatchResult::Unchanged => self.carry_out_intent(ctx, intent),
                    // The save could not start. Leave the question up rather than going
                    // ahead: throwing the changes away because the save failed is the
                    // opposite of what was asked for.
                    refused => {
                        self.guard = Some(Guard::Asking(intent));
                        refused
                    }
                }
            }
        }
    }

    // --- Input --------------------------------------------------------------

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Collect first, then act: the closure borrows egui's input state, and
        // dispatch needs `&mut self`.
        let strip = self.thumbnails.then_some(self.thumbnails_rect);
        let (pressed, zoom_delta, wheel, wheel_is_ours) = ctx.input(|input| {
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
            (
                pressed,
                input.zoom_delta(),
                Wheel::read(input),
                wheel_is_for_the_pages(input.pointer.hover_pos(), strip),
            )
        });

        // The wheel only becomes a command in paged mode, where the view is confined to
        // one page and rolling off the end of it has to mean the next page. In free mode
        // the scroll area handles it, offset and all.
        if self.state.scroll_mode() == ScrollMode::Paged && wheel_is_ours {
            let room = self.view().scroll_room();
            if let Some(command) = self.page_turns.turn(wheel, room) {
                self.dispatch(ctx, command.into());
            }
        }

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
                self.picker.open(Purpose::Open);
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

    /// Which page edits are possible right now.
    ///
    /// The single answer the keyboard and the toolbar both read. See [`crate::edits`] for
    /// what went wrong while they each worked it out for themselves.
    fn edits(&self) -> Edits {
        Edits::available(Situation {
            current: PageNumber::from_index(self.view().current_page()),
            pages: self.open.as_ref().map_or(0, |open| open.order.len()),
            can_undo: self.open.as_ref().is_some_and(|open| open.order.can_undo()),
            unsaved_changes: self.unsaved_changes(),
            saving: self.saver.is_busy(),
            thumbnails: self.thumbnails,
            selection: self.selected_pages(),
        })
    }

    /// Sends a gesture's intended selection through the command channel.
    ///
    /// The gestures work out *which* pages they want by asking [`Selection`], then hand
    /// the answer over as `set_selection` rather than writing it in place — so a click in
    /// the grid and an agent's `set_selection` take exactly the same path, and there is no
    /// selection a person can reach that a client cannot.
    fn dispatch_selection(&mut self, ctx: &egui::Context, wanted: &Selection) {
        let Some(open) = &self.open else { return };
        let pages: Vec<PageNumber> = wanted
            .positions(open.order.as_slice())
            .into_iter()
            .map(PageNumber::from_index)
            .collect();
        self.dispatch(ctx, Command::SetSelection { pages });
    }

    /// Which pages the grid is showing, from what is typed in its search box.
    ///
    /// Re-derived rather than stored. Parsing is a walk over a short string, and holding
    /// the resolved list instead would give a reorder or a delete something else to keep
    /// in step — which is the shape that has already caused three bugs here.
    fn page_filter(&self) -> PageFilter {
        PageFilter::parse(
            &self.page_filter,
            self.open.as_ref().map_or(0, |open| open.order.len()),
        )
    }

    /// Which pages the merge tab's staging viewport is showing, from the same text
    /// typed in the one shared search box — `docs/goal-5-plan.md` M30.
    ///
    /// A second parse of the same query, not a second box: `PageFilter::parse` takes
    /// a page count, and a staged document almost never has the same one as the
    /// document being edited, so the *resolved* positions from [`Self::page_filter`]
    /// cannot simply be reused — `"1-9"` against a 3-page primary clamps to its
    /// three pages at parse time, and reusing that result would hide pages 4 through
    /// 9 of a 10-page staged document that plainly matched. One parse per stage,
    /// since each has its own page count.
    fn staged_filter(&self, stage: StageId) -> PageFilter {
        PageFilter::parse(
            &self.page_filter,
            self.open
                .as_ref()
                .map_or(0, |open| open.staged_page_count(stage)),
        )
    }

    /// The filtered pages as display page numbers, or `None` when nothing is typed.
    fn filtered_pages(&self) -> Option<Vec<PageNumber>> {
        let Some(open) = &self.open else { return None };
        match self.page_filter() {
            PageFilter::All => None,
            PageFilter::Only(positions) => Some(
                positions
                    .into_iter()
                    .filter(|position| *position < open.order.len())
                    .map(PageNumber::from_index)
                    .collect(),
            ),
        }
    }

    /// `stage`'s pages the same query resolves to, as display page numbers —
    /// `None` if `stage` is not currently staged, as well as with nothing typed.
    /// See [`Self::filtered_pages`] for the main document's equivalent, and
    /// [`Self::staged_filter`] for why this is a second parse rather than a shared
    /// result.
    fn staged_filtered_pages(&self, stage: StageId) -> Option<Vec<PageNumber>> {
        let open = self.open.as_ref()?;
        let page_count = open.staged_page_count(stage);
        if page_count == 0 {
            return None;
        }
        match self.staged_filter(stage) {
            PageFilter::All => None,
            PageFilter::Only(positions) => Some(
                positions
                    .into_iter()
                    .filter(|position| *position < page_count)
                    .map(PageNumber::from_index)
                    .collect(),
            ),
        }
    }

    /// Selected pages as display page numbers, ascending. Empty with no document.
    ///
    /// The one place the selection is read out for anything other than drawing it, so
    /// the toolbar, the snapshot and the keyboard cannot disagree about what is picked.
    fn selected_pages(&self) -> Vec<PageNumber> {
        let Some(open) = &self.open else {
            return Vec::new();
        };
        self.selection
            .positions(open.order.as_slice())
            .into_iter()
            .map(PageNumber::from_index)
            .collect()
    }

    /// `stage`'s pages currently picked out, counting from 1, ascending — the
    /// staging pane's equivalent of [`Self::selected_pages`]. Empty if `stage`
    /// has no selection recorded yet, or is not (or no longer) staged at all.
    fn staged_selected_pages(&self, stage: StageId) -> Vec<PageNumber> {
        let Some(open) = &self.open else {
            return Vec::new();
        };
        let Some(shown) = open.staged_sources(stage) else {
            return Vec::new();
        };
        let Some(selection) = self.staging_selections.get(&stage) else {
            return Vec::new();
        };
        selection
            .positions(&shown)
            .into_iter()
            .map(PageNumber::from_index)
            .collect()
    }

    /// Every currently staged document, as what an agent reads over the
    /// control channel — one entry per [`OpenDocument::staged_summaries`],
    /// each carrying its own page count, the shared search query resolved
    /// against it, and its own selection.
    fn staged_snapshots(&self) -> Vec<StagedSnapshot> {
        let Some(open) = &self.open else {
            return Vec::new();
        };
        open.staged_summaries()
            .map(|(id, path)| StagedSnapshot {
                id,
                path: path.display().to_string(),
                page_count: open.staged_page_count(id),
                filtered_pages: self.staged_filtered_pages(id),
                selection: self.staged_selected_pages(id),
            })
            .collect()
    }

    /// Turns a page-edit key press into a command against the page on screen.
    ///
    /// `None` when the edit does not apply. Dispatching a command that would be refused
    /// instead would make the control channel's `unchanged` replies mean "a key did
    /// nothing" as well as "you asked for something already true".
    fn command_for_edit(&self, edit: EditKey) -> Option<Command> {
        let edits = self.edits();
        match edit {
            EditKey::MoveEarlier => edits.move_earlier,
            EditKey::MoveLater => edits.move_later,
            EditKey::Undo => edits.undo,
            // Now `None` while a save is running, which is what the button always did.
            EditKey::Save => edits.save,
            EditKey::ToggleThumbnails => Some(edits.toggle_thumbnails),
        }
    }

    /// Turns the window's close button into a `Quit` command.
    ///
    /// So the X, `Alt+F4`, the taskbar's Close, and `quit` from an agent all take one
    /// path and meet one guard. The alternative — checking for unsaved changes here as
    /// well as in dispatch — is how two producers end up disagreeing.
    fn intercept_close(&mut self, ctx: &egui::Context) {
        // `quitting` is what stops this from firing on the close *we* asked for. See the
        // field's own note: egui-winit feeds `ViewportCommand::Close` back through the
        // same queue as the X button.
        if self.quitting || !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.dispatch(ctx, Command::Quit) == DispatchResult::NeedsAnswer {
            // eframe decides whether to exit from *this* frame's commands, so the cancel
            // has to go out now rather than next frame. Only sent when the question is
            // up: on the way through, `quit` has already asked to close and cancelling
            // as well would just delay it a frame.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    /// Reports a finished save. Never blocks.
    fn collect_save(&mut self, ctx: &egui::Context) {
        let Some(saved) = self.saver.poll() else {
            return;
        };
        let where_to = saved.path.display().to_string();
        match saved.error {
            None => {
                self.last_error = None;
                if let Some(open) = &mut self.open {
                    // The primary file is now the one just written, so a Save As
                    // switches to it — what every editor does, and what stops the status
                    // bar nagging about a document that has been saved somewhere. The
                    // other contributing files, if any, keep the paths they were
                    // inserted from.
                    if let Some(primary) = open.files.first_mut() {
                        primary.path = saved.path;
                    }
                    // The order the *write* saw, not the current one. If pages were
                    // moved while the save ran, those moves are still unsaved and this
                    // is what keeps that true.
                    //
                    // Document 0 always, because the primary file — just reassigned
                    // above — is always what a save just rewrote. This is also what
                    // lets a second save over the same path find the right pages: see
                    // `PageOrder::on_disk`.
                    open.order.mark_saved(0, &saved.written);
                }
                self.emit(|| Event::Saved { path: where_to });

                // A question answered with "save first" was waiting on exactly this.
                if let Some(Guard::Saving(intent)) = self.guard.take() {
                    self.carry_out_intent(ctx, intent);
                }
            }
            Some(error) => {
                tracing::warn!(path = %saved.path.display(), %error, "could not save");
                // Visible, not just logged. A save that quietly failed would leave
                // somebody believing their reordering is on disk.
                self.last_error = Some(error.clone());
                self.emit(|| Event::SaveFailed { error });

                // Put the question back rather than going ahead. Quitting now would
                // throw the changes away *because* the save failed, which is the one
                // moment they matter most.
                if let Some(Guard::Saving(intent)) = self.guard.take() {
                    self.guard = Some(Guard::Asking(intent));
                }
            }
        }
    }

    /// Turns a chosen path into an `Open` or `InsertFile` command, matching whichever
    /// button opened the dialog. Never blocks.
    fn collect_picked_file(&mut self, ctx: &egui::Context) {
        let purpose = self.picker.purpose();
        if let Some(path) = self.picker.poll() {
            // Through the normal dispatch, so it emits the same event, reaches the
            // control channel, and reports failure exactly like either command from
            // any other producer.
            let command = match purpose {
                Purpose::Open => Command::Open { path },
                Purpose::Insert => Command::InsertFile { path },
                Purpose::Stage => Command::StageDocument { path },
            };
            self.dispatch(ctx, command);
        }
    }

    /// Which part of the window a drop landing right now would mean.
    ///
    /// The staging viewport is checked first: it sits *inside* `thumbnails_rect`, so
    /// a drop over it would otherwise also read as being over the grid. Shared by
    /// [`Self::collect_dropped_files`] and [`Self::draw_drop_hint`] so the hint and
    /// the actual drop can never disagree, the same reasoning [`crate::input::
    /// drop_action`] itself is built on.
    ///
    /// `thumbnails_rect` and `staging_rect` are last frame's rectangles — see their
    /// own docs — the same caveat [`crate::input::wheel_is_for_the_pages`] already
    /// carries for routing a gesture by pointer position.
    fn drop_zone(&self, ctx: &egui::Context) -> DropZone {
        let Some(pos) = ctx.pointer_latest_pos() else {
            return DropZone::Elsewhere;
        };
        if self.open.is_none() || !self.thumbnails {
            return DropZone::Elsewhere;
        }
        if self.staging_rect.is_some_and(|rect| rect.contains(pos)) {
            return DropZone::Staging;
        }
        if self.thumbnails_rect.contains(pos) {
            return DropZone::Grid;
        }
        DropZone::Elsewhere
    }

    /// Turns a file dropped on the window into an `Open`, `InsertFile` or
    /// `StageDocument` command.
    ///
    /// The third producer of `Open`, after the command line and the file dialog, and a
    /// producer rather than a command for the same reason the dialog is: an agent
    /// already has `open` with a path. Dropped on the page grid with a document
    /// already open, the same gesture produces `InsertFile` instead; dropped on the
    /// merge tab's staging viewport specifically, it produces `StageDocument` — see
    /// [`crate::input::DropAction`], `docs/goal-5-plan.md` §6 and §10.6.
    fn collect_dropped_files(&mut self, ctx: &egui::Context) {
        let zone = self.drop_zone(ctx);
        match drop_action(&dropped_paths(ctx), zone) {
            None => {}
            Some(DropAction::Open { path, ignored }) => {
                if ignored > 0 {
                    tracing::info!(ignored, "opened the first PDF of several dropped files");
                }
                self.dispatch(ctx, Command::Open { path });
            }
            Some(DropAction::Insert { path, ignored }) => {
                if ignored > 0 {
                    tracing::info!(ignored, "inserted the first PDF of several dropped files");
                }
                self.dispatch(ctx, Command::InsertFile { path });
            }
            Some(DropAction::Stage { path, ignored }) => {
                if ignored > 0 {
                    tracing::info!(ignored, "staged the first PDF of several dropped files");
                }
                self.dispatch(ctx, Command::StageDocument { path });
            }
            // Set here rather than through `dispatch`, because nothing became a
            // command — the drop was refused before there was one. It still has to be
            // visible, or dropping a `.docx` looks like the window ignoring you.
            Some(DropAction::Refuse { reason }) => self.last_error = Some(reason),
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
            let key = CacheKey::new(
                outcome.document,
                outcome.page_index,
                ZoomBucket::from_rung(rung),
            );
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
                        document = outcome.document,
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
                document = key.document,
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
            format!("page-{}-{}-r{rung}", key.document, key.page),
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
            //
            // Everything, not only this rung's, because `cancel_pending` is
            // all-or-nothing: the grid's queued thumbnails go with it, so their in-flight
            // records have to go too or they would never be asked for again.
            open.pool.cancel_pending();
            open.in_flight.clear();
            open.failures.clear();
            open.submitted_bucket = bucket;
        }

        let wanted = request_order(visible, PREFETCH_PAGES, open.layout.page_count());
        let cache = &open.cache;
        let mut queue = RenderQueue::new(&open.pool, &mut open.in_flight, &mut open.failures);

        for position in wanted {
            // `request_order` works in display positions, because that is what the
            // layout and the viewport are in. The renderer and the cache work in
            // sources, so this is where the two meet.
            let Some(source) = open.order.source_of(position) else {
                continue;
            };
            let key = CacheKey::new(source.document, source.page, bucket);
            if cache.contains(key) {
                continue;
            }
            // Everything else — already queued, or given up on — is [`RenderQueue`]'s to
            // decide, because the thumbnail grid has to decide it the same way.
            queue.want(key, pixels_per_point);
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
        source: Source,
        bucket: ZoomBucket,
    ) -> Option<egui::TextureId> {
        let key = CacheKey::new(source.document, source.page, bucket);
        if let Some(texture) = cache.get(key) {
            return Some(texture.id());
        }
        // Deliberately a second statement rather than `or_else`: the first borrow
        // is mutable and the second is not. Slightly soft beats a grey flash while
        // the right resolution renders.
        cache
            .best_for_page(source.document, source.page, bucket)
            .map(|(_, texture)| texture.id())
    }

    fn paint_page(
        open: &OpenDocument,
        painter: &egui::Painter,
        source: Source,
        bucket: ZoomBucket,
        rect: egui::Rect,
        texture: Option<egui::TextureId>,
    ) {
        let key = CacheKey::new(source.document, source.page, bucket);

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
                format!("page {} could not be rendered", source.page + 1),
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

        // The scrolling column is the whole document in free mode and a single page in
        // paged mode, and that one difference is what makes paged mode a mode: egui owns
        // the live offset and clamps it to the content, so handing it one page's worth of
        // content is what stops the wheel from rolling into the next one. Every offset
        // below is therefore relative to `column_top_pt`, not to the document.
        let column_top_pt = self.view().column_top_pt();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "content extents are page dimensions; f32 is what egui works in"
        )]
        let content_size = egui::vec2(
            self.layout().content_width_pt() as f32 * zoom,
            self.view().column_height_pt() as f32 * zoom,
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
            scroll_area =
                scroll_area.vertical_scroll_offset((top_pt - column_top_pt) as f32 * zoom);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "pan offsets are bounded by content width"
        )]
        if let Some(left_pt) = self.state.take_requested_scroll_left_pt() {
            scroll_area = scroll_area.horizontal_scroll_offset(left_pt as f32 * zoom);
        }

        scroll_area.show_viewport(ui, |ui, viewport| {
            // Claim the full window even when the content is smaller, so pages centre in
            // it rather than hugging the top-left corner with the scrollbar stranded out
            // to the right of them. Vertically that only ever happens in paged mode, where
            // a page shorter than the window is the normal case — and where a page pinned
            // to the top with grey below it reads as a layout fault rather than a mode.
            let column = egui::vec2(
                content_size.x.max(ui.available_width()),
                content_size.y.max(ui.available_height()),
            );
            let (content_rect, _response) = ui.allocate_exact_size(column, egui::Sense::hover());
            // Half the slack on each side. The page's own top-left corner is therefore not
            // at the column's, so both reports below subtract it — otherwise scrolling and
            // panning would each appear to start partway along.
            let gutter = ((column - content_size) * 0.5).max(egui::Vec2::ZERO);

            // `viewport` is in content coordinates, so dividing by zoom converts
            // the scroll window back into PDF points, and adding the column's own top
            // converts it back into the document. This is the reconciliation point: egui
            // tells us where it actually is.
            self.state.report_scroll_top_pt(
                column_top_pt + f64::from((viewport.min.y - gutter.y).max(0.0) / zoom),
            );
            self.state
                .report_scroll_left_pt(f64::from((viewport.min.x - gutter.x).max(0.0) / zoom));

            self.request_missing(pixels_per_point);

            let visible = self.view().visible_pages();
            let Some(open) = &mut self.open else { return };

            // Resolved in a first pass because a cache hit is a *use* and updates
            // LRU order, which needs the cache mutably — while painting needs the
            // layout and geometry immutably.
            let tiles: Vec<(Source, egui::Rect, Option<egui::TextureId>)> = visible
                .clone()
                .filter_map(|position| {
                    // `position` is where the page sits in the column; `source` is which
                    // document and page that is. They differ after any edit, so the
                    // layout is asked in positions and the geometry, cache and renderer
                    // in sources.
                    let source = open.order.source_of(position)?;
                    // Relative to the column, which in paged mode starts at this page.
                    let top_pt = open.layout.page_top_pt(position)? - column_top_pt;
                    let geometry = open.geometry_of(source)?;

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
                        let x = (column.x - size.x) * 0.5;
                        egui::Rect::from_min_size(
                            content_rect.min + egui::vec2(x, gutter.y + top_pt as f32 * zoom),
                            size,
                        )
                    };

                    let texture = Self::texture_for(&mut open.cache, source, bucket);
                    Some((source, rect, texture))
                })
                .collect();

            for (source, rect, texture) in tiles {
                Self::paint_page(open, ui.painter(), source, bucket, rect, texture);
            }
        });

        // Keep frames coming while anything is still being drawn.
        if self.open.as_ref().is_some_and(|open| !open.settled()) {
            ctx.request_repaint();
        }
    }

    /// Drops page textures that neither panel is showing any more.
    ///
    /// Once per frame and *after* both panels have drawn, because the cache has two
    /// consumers and this needs both their working sets. It used to run inside the page
    /// column with only that column's window to go on, which meant the column evicted the
    /// grid's thumbnails as fast as the grid could ask for them. See [`crate::retain`],
    /// which owns the policy and records the arithmetic.
    fn retain_textures(&mut self) {
        let visible = self.view().visible_pages();
        // Taken rather than read: a frame in which the grid did not draw — because the
        // panel is closed — must not go on keeping alive whatever it last showed.
        let grid = std::mem::take(&mut self.grid_pages);
        let Some(open) = &mut self.open else { return };
        let keep = retain::pages_to_keep(&visible, RETAIN_PAGES, grid, |position| {
            open.order.source_of(position)
        });
        open.cache
            .retain_pages(|document, page| keep.contains(&Source { document, page }));
    }

    /// Draws the page grid and dispatches whatever gesture landed in it.
    fn draw_thumbnails(&mut self, ui: &mut egui::Ui) {
        let pixels_per_point = ui.ctx().pixels_per_point();
        // Kept for the wheel, which is read a step earlier than this.
        // See `wheel_is_for_the_pages`.
        self.thumbnails_rect = ui.available_rect_before_wrap();
        let current = self.view().current_page();
        // All read before `open` is borrowed mutably below.
        let mode = self.grid_mode;
        let selection = self.selection.clone();
        let active_stage = self.active_stage;
        let staging_selection = active_stage
            .and_then(|stage| self.staging_selections.get(&stage))
            .cloned()
            .unwrap_or_default();
        let filter = self.page_filter();
        let staged_filter = active_stage.map_or(PageFilter::All, |stage| self.staged_filter(stage));
        let query = self.page_filter.clone();
        let Some(open) = &mut self.open else {
            ui.label("No document open.");
            // Nothing drew a staging viewport this frame either.
            self.staging_rect = None;
            return;
        };

        // A small, cheap-to-clone list of `Arc<Document>` handles, so the grid can
        // resolve a `Source` to geometry across every contributing file without
        // reaching into `OpenDocument`'s own layout, which stays private to this
        // module.
        let documents: Vec<Arc<Document>> = open
            .files
            .iter()
            .map(|file| Arc::clone(&file.document))
            .collect();
        // Every currently staged document's id and path, for the tab strip.
        // Cloned rather than borrowed: a borrow living through this whole
        // function would keep `open` borrowed against the `&mut open.cache`
        // and friends `Grid` needs below, for what is only ever a handful of
        // small paths once a frame.
        let staged_tabs: Vec<StagedTab> = open
            .staged_summaries()
            .map(|(id, path)| StagedTab {
                id,
                path: path.to_path_buf(),
            })
            .collect();
        // `None` until something is staged and active — until then the merge
        // tab's right pane shows its placeholder.
        let staged = active_stage.and_then(|stage| {
            let document = open.staged_document(stage)?;
            documents.get(document).map(|doc| StagedInfo {
                id: stage,
                document,
                geometries: doc.geometry(),
                selection: &staging_selection,
                filter: &staged_filter,
            })
        });
        let mut grid = Grid {
            order: &open.order,
            documents: &documents,
            cache: &mut open.cache,
            queue: RenderQueue::new(&open.pool, &mut open.in_flight, &mut open.failures),
            current,
            mode,
            selection: &selection,
            query: &query,
            filter: &filter,
            pixels_per_point,
            staged,
            staged_tabs: &staged_tabs,
            picker_open: self.picker.is_open(),
        };
        let drawn = thumbnails::draw(ui, &mut grid);
        // Kept for `retain_textures`, which runs once both panels have had their say.
        self.grid_pages = drawn.showing;
        // Last frame's, for `Self::drop_zone` — see that field's own docs.
        self.staging_rect = drawn.staging_rect;

        // Picks first: a drag that begins on an unpicked page reports both, and the
        // selection has to be right before the move reads it.
        if let Some((position, pick)) = drawn.picked {
            let ctx = ui.ctx().clone();
            let mut wanted = self.selection.clone();
            if let Some(open) = &self.open {
                wanted.pick(open.order.as_slice(), position, pick);
            }
            self.dispatch_selection(&ctx, &wanted);
        }
        if let Some(covered) = drawn.marquee {
            let ctx = ui.ctx().clone();
            let mut wanted = self.selection.clone();
            if let Some(open) = &self.open {
                wanted.set_positions(open.order.as_slice(), &covered);
            }
            self.dispatch_selection(&ctx, &wanted);
        }

        // The active pane's own selection, updated directly rather than through a
        // command — the same as a click or marquee in the main grid always could,
        // even now that `Command::SetStagedSelection` also reaches it.
        if let Some((position, pick)) = drawn.staged_picked
            && let Some(stage) = active_stage
            && let Some(open) = &self.open
            && let Some(shown) = open.staged_sources(stage)
        {
            self.staging_selections
                .entry(stage)
                .or_default()
                .pick(&shown, position, pick);
        }
        if let Some(covered) = drawn.staged_marquee
            && let Some(stage) = active_stage
            && let Some(open) = &self.open
            && let Some(shown) = open.staged_sources(stage)
        {
            self.staging_selections
                .entry(stage)
                .or_default()
                .set_positions(&shown, &covered);
        }

        // Through the normal dispatch, so a drag is indistinguishable from an agent
        // sending `move_pages` — which is the whole point of the command model.
        if let Some((from, to)) = drawn.moved {
            let ctx = ui.ctx().clone();
            let from: Vec<PageNumber> = from
                .iter()
                .filter_map(|position| PageNumber::new(position.saturating_add(1)))
                .collect();
            if let (false, Some(to)) = (from.is_empty(), PageNumber::new(to.saturating_add(1))) {
                self.dispatch(&ctx, Command::MovePages { from, to });
            }
        }

        // A drop from the staging viewport, the same reasoning as `moved` above: the
        // gesture and `insert_pages` sent by hand produce the identical command.
        // `Inserted` carries a raw `files` index rather than a `StageId` — it stays
        // decoupled from the stage concept the same way `StagedInfo::document`
        // already is — so it is translated back here, the one place a drop
        // becomes a command.
        if let Some((document, pages, position)) = drawn.inserted {
            let ctx = ui.ctx().clone();
            let pages: Vec<PageNumber> = pages
                .iter()
                .filter_map(|&page| PageNumber::new(page.saturating_add(1)))
                .collect();
            if let (false, Some(at), Some(stage)) = (
                pages.is_empty(),
                PageNumber::new(position.saturating_add(1)),
                self.open
                    .as_ref()
                    .and_then(|open| open.stage_for_document(document)),
            ) {
                self.dispatch(&ctx, Command::InsertPages { stage, pages, at });
            }
        }

        // A click in navigation mode: jump the main view there, same as `go_to_page`
        // from the control channel.
        if let Some(position) = drawn.navigated {
            let ctx = ui.ctx().clone();
            if let Some(page) = PageNumber::new(position.saturating_add(1)) {
                self.dispatch(&ctx, ViewCommand::GoToPage { page }.into());
            }
        }

        // And the tabs, the search box, and the staging viewport's close control,
        // which are controls like any other.
        if let Some(mode) = drawn.mode {
            let ctx = ui.ctx().clone();
            self.dispatch(&ctx, Command::SetGridMode { mode });
        }
        if let Some(query) = drawn.query {
            let ctx = ui.ctx().clone();
            self.dispatch(&ctx, Command::SetPageFilter { query });
        }
        // Whichever stage's own close control was clicked — the active pane's,
        // or one of the tab strip's for a stage that was not even showing.
        if let Some(stage) = drawn.clear_staging {
            let ctx = ui.ctx().clone();
            self.dispatch(&ctx, Command::ClearStaging { stage });
        }
        // Through the normal dispatch, so the button and an agent sending
        // `set_staged_selection` by hand produce the identical command.
        if let Some(stage) = drawn.select_all_staged
            && let Some(open) = &self.open
        {
            let pages: Vec<PageNumber> = (1..=open.staged_page_count(stage))
                .filter_map(PageNumber::new)
                .collect();
            let ctx = ui.ctx().clone();
            self.dispatch(&ctx, Command::SetStagedSelection { stage, pages });
        }
        // A tab clicked that was not already active, the same reasoning `mode`
        // above gets.
        if let Some(stage) = drawn.stage_switched {
            let ctx = ui.ctx().clone();
            self.dispatch(&ctx, Command::SetActiveStage { stage });
        }
        // Not a command, the same reason **Open…** and **Add pages…** are not one
        // either — see `crate::picker`.
        if drawn.stage_requested {
            self.picker.open(Purpose::Stage);
        }
    }

    /// Asks about unsaved page changes, and dispatches the answer.
    ///
    /// The three buttons produce the same `answer` command an agent sends, so there is no
    /// click-only path out of the question — which is what keeps the whole flow testable.
    /// See [`crate::confirm`].
    fn draw_question(&mut self, ctx: &egui::Context) {
        // Read as an owned string so the borrow of `self.guard` ends before dispatch.
        let Some(Guard::Asking(intent)) = &self.guard else {
            return;
        };
        let what = intent.describe();

        if let Some(choice) = chrome::question(ctx, &what) {
            self.dispatch(ctx, Command::Answer { choice });
        }
    }

    /// Paints what a drop would do, while the files are still in the air.
    ///
    /// Drawn from the same [`drop_action`] the drop itself uses, so the window cannot
    /// invite something it then refuses.
    fn draw_drop_hint(&self, ctx: &egui::Context) {
        let zone = self.drop_zone(ctx);
        let Some(action) = drop_action(&hovered_paths(ctx), zone) else {
            return;
        };
        chrome::drop_hint(ctx, &action, self.unsaved_changes());
    }

    /// Draws the toolbar and dispatches whatever was clicked.
    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        let edits = self.edits();
        let clicked = chrome::toolbar(
            ui,
            &chrome::Toolbar {
                edits: &edits,
                zoom_target: self.state.zoom_target(),
                scroll_mode: self.state.scroll_mode(),
                thumbnails: self.thumbnails,
                picker_open: self.picker.is_open(),
                document_open: self.open.is_some(),
            },
        );

        let ctx = ui.ctx().clone();
        if let Some(purpose) = clicked.open_picker {
            self.picker.open(purpose);
        }
        for command in clicked.commands {
            self.dispatch(&ctx, command);
        }
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        let view = self.view();
        chrome::status(
            ui,
            &chrome::Status {
                document: self.open.as_ref().map(|open| chrome::StatusDocument {
                    current_page: PageNumber::from_index(view.current_page()).get(),
                    page_count: open.layout.page_count(),
                    zoom: view.zoom(),
                    zoom_target: self.state.zoom_target(),
                    scroll_mode: self.state.scroll_mode(),
                    pages_cached: open.cache.len(),
                    cache_bytes: open.cache.used_bytes(),
                    workers: open.pool.worker_count(),
                    renders_in_flight: open.in_flight.len(),
                    timing: self.timing,
                    abandoned: open.abandoned(),
                    selected: self.selection.count(open.order.as_slice()),
                }),
                saving_to: self.saver.destination(),
                unsaved_changes: self.unsaved_changes(),
                last_error: self.last_error.as_deref(),
            },
        );
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
    /// Held back because it would discard unsaved page changes. Nothing happened yet.
    NeedsAnswer,
    /// A held-back request was abandoned. Nothing happened, deliberately.
    Cancelled,
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
            // `ok`, because being asked is not an error — but emphatically *not*
            // carried out. A client reads `awaiting_answer` in the snapshot to find out
            // what it is being asked, then replies with `answer`.
            Self::NeedsAnswer => Reply::ok(id, "needs_answer"),
            Self::Cancelled => Reply::ok(id, "cancelled"),
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
        self.collect_dropped_files(ctx);
        self.collect_save(ctx);
        // Before `handle_input`, so a close request is answered on the frame it arrives:
        // eframe reads the cancel from this frame's output and exits otherwise.
        self.intercept_close(ctx);
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
            // The merge tab needs room for two viewports rather than one. Widening
            // the *allowed* range rather than the panel's actual width: egui only
            // honours `default_size` the first time a panel opens, so there is no
            // cheap way from here to also resize one already open — dragging it
            // wider is left to the person, for now. See `docs/goal-5-plan.md` §10.7.
            let (min_width, max_width) = if self.grid_mode == GridMode::Merge {
                (
                    thumbnails::PANEL_MERGE_MIN_WIDTH,
                    thumbnails::PANEL_MERGE_MAX_WIDTH,
                )
            } else {
                (thumbnails::PANEL_MIN_WIDTH, thumbnails::PANEL_MAX_WIDTH)
            };
            egui::Panel::left("thumbnails")
                .resizable(true)
                // Derived from the thumbnail width rather than a round number, so the
                // panel opens sized for the columns it will actually lay out. See
                // [`thumbnails::PANEL_WIDTH`].
                .default_size(thumbnails::PANEL_WIDTH)
                // Bounded, because a panel grows to fit its content and one child asking
                // for the available width is enough to swallow the page view. See
                // [`thumbnails::PANEL_MAX_WIDTH`].
                .size_range(egui::Rangef::new(min_width, max_width))
                .show(ui, |ui| self.draw_thumbnails(ui));
        }
        self.draw_pages(ui);
        // After both panels, because it needs to know what each of them is showing.
        self.retain_textures();
        // Last, and over everything: a drag can be anywhere on the window.
        let ctx = ui.ctx().clone();
        self.draw_drop_hint(&ctx);
        self.draw_question(&ctx);

        // Our own cost, as distinct from the frame interval. If this stays well
        // under the frame budget, the pipeline has headroom.
        self.timing.ui_ms = started.elapsed().as_secs_f32() * 1000.0;
    }
}
