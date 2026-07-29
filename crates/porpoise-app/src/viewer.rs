//! The eframe window.
//!
//! M3 scrolls continuously through every page, at one shared zoom, drawing only
//! the pages that intersect the viewport. Rasterization is still synchronous on
//! the UI thread — deliberately, because a geometry bug should show up as a
//! visibly wrong layout rather than hiding behind async timing. Moving it to a
//! worker pool is M4. See `docs/goal-1-plan.md`, section 4.
//!
//! The render pipeline is `hayro -> CPU pixmap -> GPU texture`, because hayro
//! rasterizes on the CPU. So this needs no custom wgpu render pass; that only
//! becomes relevant if we implement hayro's `Device` trait ourselves.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui;
use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderRequest, RenderedPage, Renderer};
use porpoise_view::ScrollLayout;

/// Vertical gap between pages, in PDF points.
const PAGE_GAP_PT: f64 = 12.0;

/// Re-rasterize only when the zoom moves by more than this fraction, so dragging
/// a window edge does not throw away every texture on every pixel.
const ZOOM_CHANGE_THRESHOLD: f32 = 0.01;

/// Pages rasterized per frame.
///
/// Rasterization is synchronous here, so this is what keeps a frame bounded when
/// many pages become visible at once. Anything not rendered this frame draws as a
/// placeholder and is picked up next frame.
const MAX_RENDERS_PER_FRAME: usize = 2;

/// Pages either side of the visible range whose textures are kept.
///
/// Without eviction, scrolling a 400-page document would accumulate 400 textures
/// and break Goal 1's bounded-memory criterion.
const RETAIN_MARGIN_PAGES: usize = 2;

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
    renderer: HayroRenderer,
    layout: ScrollLayout,

    /// Rasterized pages, all at [`Self::cached_zoom`].
    textures: HashMap<usize, egui::TextureHandle>,
    /// Pages that failed, so they are not retried every frame.
    failures: HashMap<usize, String>,
    cached_zoom: f32,

    /// Page whose middle is nearest the viewport centre, for the status bar.
    current_page: usize,
    /// Pages held after the last eviction pass, for the status bar.
    cached_count: usize,

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
        Self {
            document,
            renderer: HayroRenderer::new(),
            layout,
            textures: HashMap::new(),
            failures: HashMap::new(),
            cached_zoom: 0.0,
            current_page: 0,
            cached_count: 0,
            start_page,
            applied_start_page: false,
            frame: 0,
            screenshot,
            screenshot_sent: false,
            outcome,
        }
    }

    /// Rasterizes one page at the given device-pixel scale and uploads it.
    fn rasterize(&mut self, ctx: &egui::Context, index: usize, device_scale: f32) {
        let request = RenderRequest {
            page_index: index,
            scale: device_scale,
        };

        match self.renderer.render(&self.document, request) {
            Ok(page) => match to_color_image(&page) {
                Some(image) => {
                    let handle = ctx.load_texture(
                        format!("page-{index}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(index, handle);
                }
                None => {
                    self.failures.insert(
                        index,
                        format!(
                            "renderer returned {} bytes for a {}x{} page",
                            page.rgba.len(),
                            page.width,
                            page.height
                        ),
                    );
                }
            },
            Err(error) => {
                self.failures.insert(index, error.to_string());
            }
        }
    }

    /// Drops textures far from the viewport so memory tracks the viewport rather
    /// than the document length.
    fn evict_outside(&mut self, visible: &Range<usize>) {
        let low = visible.start.saturating_sub(RETAIN_MARGIN_PAGES);
        let high = visible.end.saturating_add(RETAIN_MARGIN_PAGES);
        self.textures.retain(|index, _| (low..high).contains(index));
        // Failures are dropped too, so a page gets another chance if the user
        // scrolls back to it — the cause may have been transient, like a timeout.
        self.failures.retain(|index, _| (low..high).contains(index));
        self.cached_count = self.textures.len();
    }

    fn paint_page(&self, painter: &egui::Painter, index: usize, rect: egui::Rect) {
        if let Some(texture) = self.textures.get(&index) {
            painter.image(texture.id(), rect, FULL_UV, egui::Color32::WHITE);
            return;
        }

        if self.failures.contains_key(&index) {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(52, 30, 30));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("page {} could not be rendered", index + 1),
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

        let zoom = self.layout.fit_width_scale(ui.available_width());
        if (zoom - self.cached_zoom).abs()
            > self.cached_zoom.max(f32::EPSILON) * ZOOM_CHANGE_THRESHOLD
        {
            // Every cached texture was rasterized for the old zoom.
            self.textures.clear();
            self.failures.clear();
            self.cached_zoom = zoom;
        }

        #[expect(
            clippy::cast_possible_truncation,
            reason = "content extents are page dimensions; f32 is what egui works in"
        )]
        let content_size = egui::vec2(
            self.layout.content_width_pt() as f32 * zoom,
            self.layout.content_height_pt() as f32 * zoom,
        );

        let mut scroll_area = egui::ScrollArea::vertical();

        // Honour --start-page once, then hand control back to the user.
        if let (Some(page), false) = (self.start_page, self.applied_start_page) {
            if let Some(top_pt) = self.layout.page_top_pt(page) {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "scroll offsets are bounded by content height"
                )]
                let offset = top_pt as f32 * zoom;
                scroll_area = scroll_area.vertical_scroll_offset(offset);
            }
            self.applied_start_page = true;
        }

        scroll_area.show_viewport(ui, |ui, viewport| {
            let (content_rect, _response) =
                ui.allocate_exact_size(content_size, egui::Sense::hover());

            // `viewport` is in content coordinates, so dividing by zoom converts
            // the scroll window back into PDF points.
            let top_pt = f64::from(viewport.min.y / zoom);
            let height_pt = f64::from(viewport.height() / zoom);
            let visible = self.layout.visible_pages(top_pt, height_pt);

            self.current_page = self
                .layout
                .page_at_pt(top_pt + height_pt / 2.0)
                .unwrap_or(0);

            let mut rendered = 0_usize;
            let mut deferred = false;

            for index in visible.clone() {
                let (Some(top_pt), Some(geometry)) = (
                    self.layout.page_top_pt(index),
                    self.document.geometry().get(index).copied(),
                ) else {
                    continue;
                };

                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "page offsets are bounded by content height"
                )]
                let page_rect = {
                    let size = egui::vec2(geometry.width_pt * zoom, geometry.height_pt * zoom);
                    // Centre each page in the column, so a narrow page among wide
                    // ones does not sit flush left.
                    let x = (content_size.x - size.x) * 0.5;
                    egui::Rect::from_min_size(
                        content_rect.min + egui::vec2(x, top_pt as f32 * zoom),
                        size,
                    )
                };

                let missing =
                    !self.textures.contains_key(&index) && !self.failures.contains_key(&index);
                if missing {
                    if rendered < MAX_RENDERS_PER_FRAME {
                        self.rasterize(&ctx, index, zoom * pixels_per_point);
                        rendered += 1;
                    } else {
                        deferred = true;
                    }
                }

                self.paint_page(ui.painter(), index, page_rect);
            }

            // Something is still a placeholder, so come back and finish it.
            if deferred {
                ctx.request_repaint();
            }

            self.evict_outside(&visible);
        });
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!(
                "page {} of {}",
                self.current_page + 1,
                self.layout.page_count()
            ));
            ui.separator();
            ui.label(format!("{:.0}% ", self.cached_zoom * 100.0));
            ui.separator();
            ui.label(format!(
                "{:.0} pt of scroll",
                self.layout.content_height_pt()
            ));
            ui.separator();
            // Proof of virtualization: this stays small however long the document.
            ui.label(format!("{} page(s) cached", self.cached_count));
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

        // Wait for the placeholders to fill in as well as the window to appear,
        // otherwise the capture shows grey tiles rather than pages.
        let settled = self.frame >= request.warmup_frames && !self.textures.is_empty();
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
    // this optional pre-pass. `logic` may not paint, which makes it exactly the
    // right place to drive the screenshot state machine.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frame = self.frame.saturating_add(1);
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
