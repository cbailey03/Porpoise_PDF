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
//! Frame-time measurement and window capture live in [`crate::devtools`], not
//! here.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderError, RenderPool, RenderedPage};
use porpoise_view::{
    CacheKey, MAX_SCALE, MIN_SCALE, PageCache, ScrollLayout, ZoomBucket, request_order,
};

use crate::devtools::{
    FrameTiming, ScreenshotOutcome, ScreenshotRequest, Screenshotter, ScrollBenchmark,
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

/// Hidden entry points used to verify and measure the window from a headless
/// context. Grouped so they are visibly not part of the viewer's real
/// configuration.
#[derive(Default)]
pub(crate) struct DevOptions {
    /// Capture the window to this path and exit.
    pub(crate) screenshot: Option<ScreenshotRequest>,
    /// Scroll the whole document over this many frames, report, and exit.
    pub(crate) benchmark_frames: Option<u32>,
    /// Report time from this instant until the first page is painted, then exit.
    pub(crate) report_first_page_from: Option<Instant>,
}

/// How to open the viewer.
pub(crate) struct ViewerOptions {
    /// Window title, before the application name is appended.
    pub(crate) title: String,
    /// Scroll here on the first frame.
    pub(crate) start_page: Option<usize>,
    /// See [`DevOptions`].
    pub(crate) devtools: DevOptions,
}

/// Opens the viewer window. Blocks until the window closes.
pub(crate) fn run(
    document: Document,
    options: ViewerOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let wanted_screenshot = options.devtools.screenshot.is_some();
    let outcome: ScreenshotOutcome = Arc::new(Mutex::new(None));

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 768.0])
            .with_min_inner_size([320.0, 240.0])
            .with_title(format!("{} — Porpoise PDF", options.title)),
        ..Default::default()
    };

    let app_document = Arc::new(document);
    let app_outcome = Arc::clone(&outcome);
    eframe::run_native(
        "porpoise",
        native_options,
        Box::new(move |_cc| Ok(Box::new(Viewer::new(app_document, options, app_outcome)))),
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
    /// attempted. A timeout keeps a retry budget; see [`Failure::from_error`].
    failures: HashMap<CacheKey, Failure>,

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

    timing: FrameTiming,

    /// Scroll here on the first frame, then leave the user in control.
    start_page: Option<usize>,
    applied_start_page: bool,

    frame: u32,
    benchmark: Option<ScrollBenchmark>,
    screenshot: Option<Screenshotter>,
    /// Set when asked to report launch-to-first-page; cleared once reported.
    first_page_from: Option<Instant>,
}

impl Viewer {
    fn new(document: Arc<Document>, options: ViewerOptions, outcome: ScreenshotOutcome) -> Self {
        let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);
        let devtools = options.devtools;

        let benchmark = devtools
            .benchmark_frames
            .map(|frames| ScrollBenchmark::new(frames, layout.content_height_pt()));
        let screenshot = devtools
            .screenshot
            .map(|request| Screenshotter::new(request, outcome));
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
            first_page_from: devtools.report_first_page_from,
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

    // --- Render pipeline ----------------------------------------------------

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
                Ok(page) => self.accept_page(ctx, key, rung, &page),
                Err(error) => {
                    let failure = Failure::from_error(&error, self.failures.get(&key));
                    tracing::warn!(
                        page = outcome.page_index,
                        rung,
                        retries_left = failure.retries_left,
                        %error,
                        "page failed to rasterize"
                    );
                    self.failures.insert(key, failure);
                }
            }
        }
    }

    fn accept_page(&mut self, ctx: &egui::Context, key: CacheKey, rung: i16, page: &RenderedPage) {
        let bytes = page.rgba.len();
        let Some(image) = to_color_image(page) else {
            tracing::warn!(
                page = key.page,
                width = page.width,
                height = page.height,
                bytes,
                "renderer returned a buffer inconsistent with its dimensions"
            );
            self.failures.insert(
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
        self.cache.insert(key, handle, bytes);
        self.failures.remove(&key);

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
    fn request_missing(&mut self, visible: &Range<usize>, pixels_per_point: f32) {
        let order = request_order(visible.clone(), PREFETCH_PAGES, self.layout.page_count());
        let scale = self.bucket.scale() * pixels_per_point;
        let tag = i64::from(self.bucket.rung());

        for page in order {
            let key = CacheKey::new(page, self.bucket);
            if self.cache.contains(key) || self.in_flight.contains(&key) {
                continue;
            }
            // A failure with retries left earns another attempt, spending one.
            // Without a budget this would re-request a hopeless page every frame.
            if let Some(failure) = self.failures.get_mut(&key)
                && !failure.take_retry()
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

    /// Rasterizations we have stopped trying to produce.
    fn abandoned(&self) -> usize {
        self.failures
            .values()
            .filter(|failure| failure.gave_up())
            .count()
    }

    // --- Painting -----------------------------------------------------------

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

        // A failure with retries left is still pending, so claiming failure would
        // be premature — fall through to the placeholder instead.
        if let Some(failure) = self.failures.get(&CacheKey::new(page, self.bucket))
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
                self.timing.ui_ms, self.timing.frame_ms
            ));
            // Counts only what we have given up on, matching the error tiles. A
            // page still being retried is not a failure yet.
            let abandoned = self.abandoned();
            if abandoned > 0 {
                ui.separator();
                ui.colored_label(ui.visuals().error_fg_color, format!("{abandoned} failed"));
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
        self.scroll_by_pt(step);
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let settled = !self.cache.is_empty() && self.in_flight.is_empty();
        let frame = self.frame;
        let Some(screenshotter) = &mut self.screenshot else {
            return;
        };
        if screenshotter.drive(ctx, frame, settled) {
            self.screenshot = None;
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
        self.handle_input(ctx);
        self.timing.logic_ms = started.elapsed().as_secs_f32() * 1000.0;

        // Deliberately after the timing: these only bookkeep and would otherwise
        // charge instrumentation overhead to the pipeline.
        self.drive_benchmark(ctx);
        self.drive_screenshot(ctx);
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
        // width * height * 4 overflows usize on a 64-bit target only for absurd
        // values, but the multiplication is checked so the guard holds regardless.
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
}
