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
use porpoise_view::{CacheKey, PageCache, ScrollLayout, ZoomBucket, request_order};

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

/// Results absorbed per frame, so a burst of completions cannot stall a frame.
const MAX_RESULTS_PER_FRAME: usize = 8;

/// How long one page may take before the render is abandoned.
///
/// Shorter than the CLI's budget because an interactive viewer should give up and
/// show an error tile rather than leave a page blank for ten seconds.
const JOB_TIMEOUT: Duration = Duration::from_secs(5);

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

    current_page: usize,

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
        outcome: ScreenshotOutcome,
    ) -> Self {
        let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);
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
            current_page: 0,
            start_page,
            applied_start_page: false,
            frame: 0,
            screenshot,
            screenshot_sent: false,
            outcome,
        }
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

        self.zoom = self.layout.fit_width_scale(ui.available_width());
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
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "scroll offsets are bounded by content height"
                )]
                let offset = top_pt as f32 * self.zoom;
                scroll_area = scroll_area.vertical_scroll_offset(offset);
            }
            self.applied_start_page = true;
        }

        scroll_area.show_viewport(ui, |ui, viewport| {
            let (content_rect, _response) =
                ui.allocate_exact_size(content_size, egui::Sense::hover());

            // `viewport` is in content coordinates, so dividing by zoom converts
            // the scroll window back into PDF points.
            let top_pt = f64::from(viewport.min.y / self.zoom);
            let height_pt = f64::from(viewport.height() / self.zoom);
            let visible = self.layout.visible_pages(top_pt, height_pt);

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
            let low = visible.start.saturating_sub(PREFETCH_PAGES + 1);
            let high = visible.end.saturating_add(PREFETCH_PAGES + 1);
            self.cache.retain_pages(|page| (low..high).contains(&page));
        });

        // Keep frames coming while anything is still being drawn.
        if self.pool.is_busy() || !self.in_flight.is_empty() {
            ctx.request_repaint();
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
            ui.label(format!("{:.0}%", self.zoom * 100.0));
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
        self.collect_renders(ctx);
        self.drive_screenshot(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // `TopBottomPanel` and `SidePanel` were unified into `Panel` in 0.34. The
        // root `ui` is the central area, so there is no CentralPanel here.
        egui::Panel::top("status").show(ui, |ui| self.draw_status(ui));
        self.draw_pages(ui);
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
