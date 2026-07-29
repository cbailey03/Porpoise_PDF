//! The eframe window.
//!
//! M4 rasterizes off the UI thread. The frame loop only ever polls for finished
//! pages, submits requests for missing ones, and paints whatever the cache
//! currently holds — it never waits for a render. That is the whole reason
//! scrolling can stay smooth while pages are still being drawn.
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

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderPool, RenderedPage};
use porpoise_view::{
    CacheKey, MAX_SCALE, MIN_SCALE, PageCache, ScrollLayout, ZoomBucket, request_order,
};

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

/// Fraction of the viewport a page-down moves in free-scroll mode.
///
/// Slightly less than a full screen so a line or two carries over, which makes it
/// obvious nothing was skipped.
const VIEWPORT_STEP_FRACTION: f64 = 0.9;

/// How far an arrow key scrolls, in PDF points.
const ARROW_STEP_PT: f64 = 48.0;

/// Frames the scroll benchmark discards before recording.
const BENCHMARK_WARMUP_FRAMES: u32 = 60;

/// How zoom is chosen each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ZoomMode {
    /// Fit the widest page to the viewport width.
    FitWidth,
    /// Fit the largest page entirely within the viewport.
    FitPage,
    /// An explicit factor, from the zoom keys or ctrl+wheel.
    Fixed(f32),
}

impl ZoomMode {
    fn label(self) -> &'static str {
        match self {
            Self::FitWidth => "fit width",
            Self::FitPage => "fit page",
            Self::Fixed(_) => "zoom",
        }
    }
}

/// How navigation behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollMode {
    /// Continuous scrolling; page-down moves by a viewport.
    Free,
    /// Page-down moves to the next page boundary.
    Paged,
}

impl ScrollMode {
    fn label(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paged => "paged",
        }
    }
}

/// A scripted scroll used to measure frame times.
///
/// Goal 1 asks for sustained 60 fps while scrolling, and every other check in
/// this project is a static capture — which proves geometry and caching but says
/// nothing about behaviour under motion. This drives the scroll from code so the
/// claim can actually be measured.
struct ScrollBenchmark {
    frames_left: u32,
    /// Frames still to be discarded before recording starts.
    ///
    /// Window creation, GPU device setup and font loading all land in the first
    /// frames and are not scrolling costs. Including them puts a ~150 ms outlier
    /// in the maximum and makes the numbers unusable for judging smoothness.
    warmup_left: u32,
    /// Points to advance per frame.
    step_pt: f64,
    /// Time spent in our own `ui` each frame.
    ui_ms: Vec<f32>,
    /// Time spent in `logic`, which is where finished pages are uploaded to the
    /// GPU. Measured separately because uploads must happen on this thread and
    /// are the one part of the pipeline that cannot be moved off it.
    logic_ms: Vec<f32>,
    /// Interval between frames, which is what the user actually perceives.
    frame_ms: Vec<f32>,
    /// Worst frame interval seen during warmup, reported separately for honesty.
    warmup_worst_ms: f32,
}

impl ScrollBenchmark {
    fn report(&self) {
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

fn print_percentiles(label: &str, samples: &[f32]) {
    if samples.is_empty() {
        println!("  {label}: no samples");
        return;
    }
    let mut sorted = samples.to_vec();
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

    println!(
        "  {label}: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
        at(0.50),
        at(0.95),
        at(0.99),
        at(1.0)
    );
}

/// A development request to capture the window and exit.
///
/// This exists because a native window cannot be inspected from a headless
/// context, so without it "the window works" would be an untested claim.
pub(crate) struct ScreenshotRequest {
    /// Where to write the PNG.
    pub(crate) path: PathBuf,
    /// Frames to draw before asking, so real content is on screen first.
    pub(crate) warmup_frames: u32,
    /// Hard frame budget, so a failed capture can never leave a window open.
    pub(crate) budget_frames: u32,
}

/// What the screenshot attempt produced, shared with the caller because
/// `run_native` gives us no other way to report it.
type ScreenshotOutcome = Arc<Mutex<Option<Result<PathBuf, String>>>>;

/// Opens the viewer window. Blocks until the window closes.
pub(crate) fn run(
    title: String,
    document: Document,
    start_page: Option<usize>,
    screenshot: Option<ScreenshotRequest>,
    benchmark_frames: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let wanted_screenshot = screenshot.is_some();
    let outcome: ScreenshotOutcome = Arc::new(Mutex::new(None));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([320.0, 240.0])
            .with_title(format!("{title} — Porpoise PDF")),
        ..Default::default()
    };

    let app_document = Arc::new(document);
    let app_outcome = Arc::clone(&outcome);
    eframe::run_native(
        "porpoise",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(Viewer::new(
                app_document,
                start_page,
                screenshot,
                benchmark_frames,
                app_outcome,
            )))
        }),
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
    document: Arc<Document>,
    layout: ScrollLayout,
    pool: RenderPool,

    cache: PageCache<egui::TextureHandle>,
    /// Requests submitted but not yet returned, so a page is not queued twice.
    in_flight: Vec<CacheKey>,
    /// Failures keyed by rasterization, not by page, so a different zoom is still
    /// attempted — and a transient timeout does not blacklist a page forever.
    failures: HashMap<CacheKey, String>,

    /// The zoom rung currently being rendered for.
    bucket: ZoomBucket,
    /// The unquantized display zoom, which is what pages are laid out at.
    zoom: f32,
    zoom_mode: ZoomMode,
    scroll_mode: ScrollMode,

    /// A scroll position requested by navigation, in points, applied next frame.
    ///
    /// Held in points rather than pixels so it stays correct if the zoom changes
    /// in the same frame.
    pending_scroll_pt: Option<f64>,
    /// Top of the viewport as of the last frame, in points.
    scroll_top_pt: f64,
    /// Height of the viewport as of the last frame, in points.
    viewport_height_pt: f64,

    current_page: usize,

    last_ui_ms: f32,
    last_logic_ms: f32,
    last_frame_ms: f32,
    benchmark: Option<ScrollBenchmark>,

    /// Scroll here on the first frame, then leave the user in control.
    start_page: Option<usize>,
    applied_start_page: bool,

    frame: u32,
    screenshot: Option<ScreenshotRequest>,
    screenshot_sent: bool,
    outcome: ScreenshotOutcome,
}

impl Viewer {
    fn new(
        document: Arc<Document>,
        start_page: Option<usize>,
        screenshot: Option<ScreenshotRequest>,
        benchmark_frames: Option<u32>,
        outcome: ScreenshotOutcome,
    ) -> Self {
        let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);

        // Spread the scripted scroll across the whole document, so the benchmark
        // exercises every page rather than thrashing one spot.
        let benchmark = benchmark_frames.map(|frames| ScrollBenchmark {
            frames_left: frames.max(1),
            warmup_left: BENCHMARK_WARMUP_FRAMES,
            step_pt: layout.content_height_pt() / f64::from(frames.max(1)),
            ui_ms: Vec::with_capacity(frames as usize),
            logic_ms: Vec::with_capacity(frames as usize),
            frame_ms: Vec::with_capacity(frames as usize),
            warmup_worst_ms: 0.0,
        });
        let pool = RenderPool::new(
            Arc::clone(&document),
            HayroRenderer::new(),
            RenderPool::recommended_workers(),
            JOB_TIMEOUT,
        );

        Self {
            document,
            layout,
            pool,
            cache: PageCache::new(TEXTURE_BUDGET_BYTES),
            in_flight: Vec::new(),
            failures: HashMap::new(),
            bucket: ZoomBucket::enclosing(1.0),
            zoom: 1.0,
            zoom_mode: ZoomMode::FitWidth,
            scroll_mode: ScrollMode::Free,
            pending_scroll_pt: None,
            scroll_top_pt: 0.0,
            viewport_height_pt: 0.0,
            current_page: 0,
            last_ui_ms: 0.0,
            last_logic_ms: 0.0,
            last_frame_ms: 0.0,
            benchmark,
            start_page,
            applied_start_page: false,
            frame: 0,
            screenshot,
            screenshot_sent: false,
            outcome,
        }
    }

    // --- Navigation ---------------------------------------------------------

    /// Requests a scroll to `top_pt`, clamped to the document.
    fn scroll_to_pt(&mut self, top_pt: f64) {
        let furthest = (self.layout.content_height_pt() - self.viewport_height_pt).max(0.0);
        let target = if top_pt.is_finite() {
            top_pt.clamp(0.0, furthest)
        } else {
            0.0
        };
        self.pending_scroll_pt = Some(target);
    }

    fn scroll_by_pt(&mut self, delta_pt: f64) {
        self.scroll_to_pt(self.scroll_top_pt + delta_pt);
    }

    fn go_to_page(&mut self, page: usize) {
        let last = self.layout.page_count().saturating_sub(1);
        if let Some(top_pt) = self.layout.page_top_pt(page.min(last)) {
            self.scroll_to_pt(top_pt);
        }
    }

    /// Moves one step forward or back, meaning a page or a viewport depending on
    /// the scroll mode.
    fn advance(&mut self, forward: bool) {
        match self.scroll_mode {
            ScrollMode::Paged => {
                let target = if forward {
                    self.current_page.saturating_add(1)
                } else {
                    self.current_page.saturating_sub(1)
                };
                self.go_to_page(target);
            }
            ScrollMode::Free => {
                let step = self.viewport_height_pt * VIEWPORT_STEP_FRACTION;
                self.scroll_by_pt(if forward { step } else { -step });
            }
        }
    }

    /// Switches zoom mode while keeping the current page in view.
    ///
    /// Without this a zoom change scrolls somewhere arbitrary, because the scroll
    /// offset is in pixels and the same pixel offset means a different place in
    /// the document at a different zoom.
    fn set_zoom_mode(&mut self, mode: ZoomMode) {
        self.zoom_mode = mode;
        let anchor = self.current_page;
        self.go_to_page(anchor);
    }

    fn step_zoom(&mut self, rungs: i16) {
        let current = ZoomBucket::enclosing(self.zoom);
        let stepped = current.step(rungs).scale();
        self.set_zoom_mode(ZoomMode::Fixed(stepped));
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Collect first, then act: the closure borrows egui's input state, and the
        // handlers need `&mut self`.
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
            // Applied as a continuous factor rather than a rung step, so a pinch
            // feels proportional. Bucketing still bounds how often we re-render.
            let target = (self.zoom * zoom_delta).clamp(MIN_SCALE, MAX_SCALE);
            self.set_zoom_mode(ZoomMode::Fixed(target));
        }

        for (key, modifiers) in pressed {
            self.on_key(key, modifiers);
        }
    }

    fn on_key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
        if modifiers.command || modifiers.ctrl {
            match key {
                egui::Key::Plus | egui::Key::Equals => self.step_zoom(1),
                egui::Key::Minus => self.step_zoom(-1),
                egui::Key::Num0 => self.set_zoom_mode(ZoomMode::FitWidth),
                egui::Key::Num1 => self.set_zoom_mode(ZoomMode::Fixed(1.0)),
                egui::Key::Num2 => self.set_zoom_mode(ZoomMode::FitPage),
                _ => {}
            }
            return;
        }

        match key {
            egui::Key::PageDown => self.advance(true),
            egui::Key::PageUp => self.advance(false),
            // Space is the reader's page-down; shift reverses it.
            egui::Key::Space => self.advance(!modifiers.shift),
            egui::Key::Home => self.go_to_page(0),
            egui::Key::End => self.go_to_page(self.layout.page_count().saturating_sub(1)),
            egui::Key::ArrowDown => self.scroll_by_pt(ARROW_STEP_PT),
            egui::Key::ArrowUp => self.scroll_by_pt(-ARROW_STEP_PT),
            _ => {}
        }
    }

    /// Advances the scripted scroll, and reports once it finishes.
    fn drive_benchmark(&mut self, ctx: &egui::Context) {
        let Some(benchmark) = &mut self.benchmark else {
            return;
        };

        if benchmark.warmup_left > 0 {
            benchmark.warmup_left -= 1;
            benchmark.warmup_worst_ms = benchmark.warmup_worst_ms.max(self.last_frame_ms);
        } else {
            benchmark.ui_ms.push(self.last_ui_ms);
            benchmark.logic_ms.push(self.last_logic_ms);
            benchmark.frame_ms.push(self.last_frame_ms);
            benchmark.frames_left = benchmark.frames_left.saturating_sub(1);
        }

        if benchmark.frames_left == 0 {
            benchmark.report();
            self.benchmark = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let step = benchmark.step_pt;
        // Keep frames coming at full rate; without this the app idles.
        ctx.request_repaint();
        self.scroll_by_pt(step);
    }

    /// Absorbs finished renders into the cache. Never blocks.
    fn collect_renders(&mut self, ctx: &egui::Context) {
        for _ in 0..MAX_RESULTS_PER_FRAME {
            let Some(outcome) = self.pool.try_recv() else {
                break;
            };

            // The tag is the zoom rung we asked for. A result whose rung is no
            // longer current is still worth keeping: it is a valid render of that
            // page and serves as a fallback until the current rung arrives.
            let Ok(rung) = i16::try_from(outcome.tag) else {
                continue;
            };
            let key = CacheKey::new(outcome.page_index, ZoomBucket::from_rung(rung));
            self.in_flight.retain(|pending| *pending != key);

            match outcome.result {
                Ok(page) => {
                    let bytes = page.rgba.len();
                    if let Some(image) = to_color_image(&page) {
                        let handle = ctx.load_texture(
                            format!("page-{}-r{rung}", outcome.page_index),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.cache.insert(key, handle, bytes);
                        self.failures.remove(&key);
                    } else {
                        self.failures.insert(
                            key,
                            format!(
                                "renderer returned {bytes} bytes for a {}x{} page",
                                page.width, page.height
                            ),
                        );
                    }
                }
                Err(error) => {
                    self.failures.insert(key, error.to_string());
                }
            }
        }
    }

    /// Queues anything visible or nearby that is not already cached or in flight.
    fn request_missing(&mut self, visible: &Range<usize>, pixels_per_point: f32) {
        let order = request_order(visible.clone(), PREFETCH_PAGES, self.layout.page_count());
        let scale = self.bucket.scale() * pixels_per_point;
        let tag = i64::from(self.bucket.rung());

        for page in order {
            let key = CacheKey::new(page, self.bucket);
            if self.cache.contains(key)
                || self.in_flight.contains(&key)
                || self.failures.contains_key(&key)
            {
                continue;
            }
            if self.pool.submit(page, scale, tag) {
                self.in_flight.push(key);
            }
        }
    }

    /// The texture to draw for a page: the current rung, else the nearest cached
    /// rung, else nothing.
    fn texture_for(&mut self, page: usize) -> Option<egui::TextureId> {
        let key = CacheKey::new(page, self.bucket);
        if let Some(texture) = self.cache.get(key) {
            return Some(texture.id());
        }
        // Deliberately a second statement rather than `or_else`: the first borrow
        // is mutable (it touches LRU order) and the second is not.
        self.cache
            .best_for_page(page, self.bucket)
            .map(|(_, texture)| texture.id())
    }

    fn paint_page(
        &self,
        painter: &egui::Painter,
        page: usize,
        rect: egui::Rect,
        texture: Option<egui::TextureId>,
    ) {
        if let Some(id) = texture {
            painter.image(id, rect, FULL_UV, egui::Color32::WHITE);
            return;
        }

        if self
            .failures
            .contains_key(&CacheKey::new(page, self.bucket))
        {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(52, 30, 30));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("page {} could not be rendered", page + 1),
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(230, 140, 140),
            );
            return;
        }

        // Not rasterized yet. A correct-aspect tile means scrolling never jumps
        // when the real page arrives.
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(232));
    }

    fn draw_pages(&mut self, ui: &mut egui::Ui) {
        if self.layout.page_count() == 0 {
            ui.label("This document has no pages.");
            return;
        }

        // Cloning the context is cheap (an `Arc` inside) and avoids holding an
        // immutable borrow of `ui` across the mutable calls below.
        let ctx = ui.ctx().clone();
        let pixels_per_point = ctx.pixels_per_point();

        self.zoom = match self.zoom_mode {
            ZoomMode::FitWidth => self.layout.fit_width_scale(ui.available_width()),
            ZoomMode::FitPage => self
                .layout
                .fit_page_scale(ui.available_width(), ui.available_height()),
            ZoomMode::Fixed(scale) => scale,
        };
        let wanted = ZoomBucket::enclosing(self.zoom);
        if wanted != self.bucket {
            // Queued work is for the old rung and no longer worth doing. Cached
            // textures are kept deliberately — they are the fallback that stops a
            // resize from flashing grey.
            self.pool.cancel_pending();
            self.in_flight.clear();
            self.failures.clear();
            self.bucket = wanted;
        }

        #[expect(
            clippy::cast_possible_truncation,
            reason = "content extents are page dimensions; f32 is what egui works in"
        )]
        let content_size = egui::vec2(
            self.layout.content_width_pt() as f32 * self.zoom,
            self.layout.content_height_pt() as f32 * self.zoom,
        );

        let mut scroll_area = egui::ScrollArea::vertical();

        // Honour --start-page once, then hand control back to the user.
        if let (Some(page), false) = (self.start_page, self.applied_start_page) {
            if let Some(top_pt) = self.layout.page_top_pt(page) {
                self.pending_scroll_pt = Some(top_pt);
            }
            self.applied_start_page = true;
        }

        // Only override the offset on frames where navigation asked for it, or the
        // user could never scroll by hand.
        if let Some(top_pt) = self.pending_scroll_pt.take() {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "scroll offsets are bounded by content height"
            )]
            let offset = top_pt as f32 * self.zoom;
            scroll_area = scroll_area.vertical_scroll_offset(offset);
        }

        scroll_area.show_viewport(ui, |ui, viewport| {
            let (content_rect, _response) =
                ui.allocate_exact_size(content_size, egui::Sense::hover());

            // `viewport` is in content coordinates, so dividing by zoom converts
            // the scroll window back into PDF points.
            let top_pt = f64::from(viewport.min.y / self.zoom);
            let height_pt = f64::from(viewport.height() / self.zoom);
            let visible = self.layout.visible_pages(top_pt, height_pt);

            // Remembered for navigation, which runs before the next frame's layout
            // and so has no viewport of its own to consult.
            self.scroll_top_pt = top_pt;
            self.viewport_height_pt = height_pt;

            self.current_page = self
                .layout
                .page_at_pt(top_pt + height_pt / 2.0)
                .unwrap_or(0);

            self.request_missing(&visible, pixels_per_point);

            for page in visible.clone() {
                let (Some(top_pt), Some(geometry)) = (
                    self.layout.page_top_pt(page),
                    self.document.geometry().get(page).copied(),
                ) else {
                    continue;
                };

                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "page offsets are bounded by content height"
                )]
                let page_rect = {
                    let size = egui::vec2(
                        geometry.width_pt * self.zoom,
                        geometry.height_pt * self.zoom,
                    );
                    // Centre each page in the column, so a narrow page among wide
                    // ones does not sit flush left.
                    let x = (content_size.x - size.x) * 0.5;
                    egui::Rect::from_min_size(
                        content_rect.min + egui::vec2(x, top_pt as f32 * self.zoom),
                        size,
                    )
                };

                let texture = self.texture_for(page);
                self.paint_page(ui.painter(), page, page_rect, texture);
            }

            // Keep memory proportional to the viewport rather than the document.
            let low = visible.start.saturating_sub(RETAIN_PAGES);
            let high = visible.end.saturating_add(RETAIN_PAGES);
            self.cache.retain_pages(|page| (low..high).contains(&page));
        });

        // Keep frames coming while anything is still being drawn.
        if self.pool.is_busy() || !self.in_flight.is_empty() {
            ctx.request_repaint();
        }
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui) {
        // Collected rather than applied inline, because each handler needs
        // `&mut self` while `ui` is borrowed.
        let mut requested_zoom: Option<ZoomMode> = None;
        let mut requested_page: Option<usize> = None;
        let mut zoom_step: i16 = 0;

        ui.horizontal(|ui| {
            if ui.button("⏮").on_hover_text("First page (Home)").clicked() {
                requested_page = Some(0);
            }
            if ui.button("⏭").on_hover_text("Last page (End)").clicked() {
                requested_page = Some(self.layout.page_count().saturating_sub(1));
            }
            ui.separator();

            if ui.button("−").on_hover_text("Zoom out (Ctrl+-)").clicked() {
                zoom_step -= 1;
            }
            if ui.button("+").on_hover_text("Zoom in (Ctrl++)").clicked() {
                zoom_step += 1;
            }

            let fit_width = self.zoom_mode == ZoomMode::FitWidth;
            if ui
                .selectable_label(fit_width, "Width")
                .on_hover_text("Fit width (Ctrl+0)")
                .clicked()
            {
                requested_zoom = Some(ZoomMode::FitWidth);
            }
            let fit_page = self.zoom_mode == ZoomMode::FitPage;
            if ui
                .selectable_label(fit_page, "Page")
                .on_hover_text("Fit page (Ctrl+2)")
                .clicked()
            {
                requested_zoom = Some(ZoomMode::FitPage);
            }
            ui.separator();

            // Paged versus free changes what PageDown and Space mean.
            let paged = self.scroll_mode == ScrollMode::Paged;
            if ui
                .selectable_label(paged, "Paged")
                .on_hover_text("Page-by-page instead of continuous scrolling")
                .clicked()
            {
                self.scroll_mode = if paged {
                    ScrollMode::Free
                } else {
                    ScrollMode::Paged
                };
            }
        });

        if zoom_step != 0 {
            self.step_zoom(zoom_step);
        }
        if let Some(mode) = requested_zoom {
            self.set_zoom_mode(mode);
        }
        if let Some(page) = requested_page {
            self.go_to_page(page);
        }
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!(
                "page {} of {}",
                self.current_page + 1,
                self.layout.page_count()
            ));
            ui.separator();
            ui.label(format!(
                "{:.0}% {}",
                self.zoom * 100.0,
                self.zoom_mode.label()
            ));
            ui.separator();
            ui.label(self.scroll_mode.label());
            ui.separator();
            // Proof of virtualization: both stay small however long the document.
            ui.label(format!(
                "{} cached, {:.1} MB",
                self.cache.len(),
                self.cache.used_bytes() as f64 / (1024.0 * 1024.0)
            ));
            ui.separator();
            ui.label(format!(
                "{} workers, {} in flight",
                self.pool.worker_count(),
                self.in_flight.len()
            ));
            ui.separator();
            ui.label(format!(
                "ui {:.1} ms, frame {:.1} ms",
                self.last_ui_ms, self.last_frame_ms
            ));
            if !self.failures.is_empty() {
                ui.separator();
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!("{} failed", self.failures.len()),
                );
            }
        });
    }

    /// Drives the capture-and-exit sequence when a screenshot was requested.
    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        if self.screenshot.is_none() {
            return;
        }

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
            let path = self
                .screenshot
                .as_ref()
                .map(|request| request.path.clone())
                .unwrap_or_default();
            let result = save_screenshot(&image, &path);
            self.finish_screenshot(ctx, result);
            return;
        }

        let Some(request) = &self.screenshot else {
            return;
        };

        // Wait for the pipeline to drain as well as the window to appear,
        // otherwise the capture shows placeholders rather than pages.
        let settled = self.frame >= request.warmup_frames
            && !self.cache.is_empty()
            && self.in_flight.is_empty();
        if !self.screenshot_sent && settled {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.screenshot_sent = true;
        }

        if self.frame > request.budget_frames {
            let budget = request.budget_frames;
            self.finish_screenshot(
                ctx,
                Err(format!("no screenshot arrived within {budget} frames")),
            );
        }
    }

    fn finish_screenshot(&mut self, ctx: &egui::Context, result: Result<PathBuf, String>) {
        if let Ok(mut slot) = self.outcome.lock() {
            *slot = Some(result);
        }
        self.screenshot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        self.last_frame_ms = ctx.input(|input| input.stable_dt) * 1000.0;

        let started = std::time::Instant::now();
        self.collect_renders(ctx);
        self.handle_input(ctx);
        self.last_logic_ms = started.elapsed().as_secs_f32() * 1000.0;

        // Deliberately after the timing: these only bookkeep and would otherwise
        // charge benchmark overhead to the pipeline.
        self.drive_benchmark(ctx);
        self.drive_screenshot(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = std::time::Instant::now();

        // `TopBottomPanel` and `SidePanel` were unified into `Panel` in 0.34. The
        // root `ui` is the central area, so there is no CentralPanel here.
        egui::Panel::top("toolbar").show(ui, |ui| self.draw_toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.draw_status(ui));
        self.draw_pages(ui);

        // Our own cost, as distinct from the frame interval. If this stays well
        // under the frame budget, the pipeline has headroom.
        self.last_ui_ms = started.elapsed().as_secs_f32() * 1000.0;
    }
}

/// The whole texture, for `Painter::image`.
const FULL_UV: egui::Rect = egui::Rect {
    min: egui::Pos2 { x: 0.0, y: 0.0 },
    max: egui::Pos2 { x: 1.0, y: 1.0 },
};

/// Converts a rasterized page into an egui image, or `None` if the buffer does
/// not match its stated dimensions.
///
/// The check matters: `ColorImage::from_rgba_unmultiplied` *panics* on a length
/// mismatch, and a panic on the UI thread takes down the window.
fn to_color_image(page: &RenderedPage) -> Option<egui::ColorImage> {
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
