//! The eframe window.
//!
//! M2 shows page 1, fit to width. Rasterization is synchronous, on the UI thread,
//! which means resizing a large page will visibly hitch. That is expected and
//! deliberate: getting pixels on screen is this milestone's job, and moving
//! rasterization to a worker pool is M4's. See `docs/goal-1-plan.md`, section 4.
//!
//! The render pipeline is `hayro -> CPU pixmap -> GPU texture`, because hayro
//! rasterizes on the CPU. So M2 needs no custom wgpu render pass; that only
//! becomes relevant if we implement hayro's `Device` trait ourselves.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui;
use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderRequest, RenderedPage, Renderer};
use porpoise_view::{FitMode, fit_scale};

/// Re-rasterize only when the wanted scale differs from the current one by more
/// than this fraction, so dragging a window edge does not queue a render per
/// pixel.
const SCALE_CHANGE_THRESHOLD: f32 = 0.01;

/// A development request to capture the window and exit.
///
/// This exists because a native window cannot be inspected from a headless
/// context, so without it "the window works" would be an untested claim. It also
/// lays the groundwork for visual regression tests.
pub(crate) struct ScreenshotRequest {
    /// Where to write the PNG.
    pub(crate) path: PathBuf,
    /// Frames to draw before asking, so a real frame is on screen first.
    pub(crate) warmup_frames: u32,
    /// Hard frame budget. Without this a failed capture would leave a window
    /// open on someone's desktop forever.
    pub(crate) budget_frames: u32,
}

/// What the screenshot attempt produced, shared with the caller because
/// `run_native` gives us no other way to report it.
type ScreenshotOutcome = Arc<Mutex<Option<Result<PathBuf, String>>>>;

/// Opens the viewer window. Blocks until the window closes.
pub(crate) fn run(
    title: String,
    document: Document,
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
        Box::new(move |_cc| Ok(Box::new(Viewer::new(app_document, screenshot, app_outcome)))),
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
    texture: Option<egui::TextureHandle>,
    /// Device-pixel scale the current texture was rasterized at.
    texture_scale: f32,
    error: Option<String>,
    frame: u32,
    screenshot: Option<ScreenshotRequest>,
    screenshot_sent: bool,
    outcome: ScreenshotOutcome,
}

impl Viewer {
    fn new(
        document: Arc<Document>,
        screenshot: Option<ScreenshotRequest>,
        outcome: ScreenshotOutcome,
    ) -> Self {
        Self {
            document,
            renderer: HayroRenderer::new(),
            texture: None,
            texture_scale: 0.0,
            error: None,
            frame: 0,
            screenshot,
            screenshot_sent: false,
            outcome,
        }
    }

    fn needs_rasterize(&self, wanted_scale: f32) -> bool {
        if self.texture.is_none() && self.error.is_none() {
            return true;
        }
        // Compare relatively, so the threshold means the same thing at every zoom.
        let reference = self.texture_scale.max(f32::EPSILON);
        (wanted_scale - self.texture_scale).abs() > reference * SCALE_CHANGE_THRESHOLD
    }

    fn rasterize(&mut self, ctx: &egui::Context, scale: f32) {
        let request = RenderRequest {
            page_index: 0,
            scale,
        };
        // Record the attempted scale either way, so a failure is not retried on
        // every frame at the same scale.
        self.texture_scale = scale;

        match self.renderer.render(&self.document, request) {
            Ok(page) => match to_color_image(&page) {
                Some(image) => {
                    self.texture =
                        Some(ctx.load_texture("page", image, egui::TextureOptions::LINEAR));
                    self.error = None;
                }
                None => {
                    self.texture = None;
                    self.error = Some(format!(
                        "renderer returned {} bytes for a {}x{} page",
                        page.rgba.len(),
                        page.width,
                        page.height
                    ));
                }
            },
            Err(error) => {
                self.texture = None;
                self.error = Some(error.to_string());
            }
        }
    }

    fn draw_page(&mut self, ui: &mut egui::Ui) {
        let Some(geometry) = self.document.geometry().first().copied() else {
            ui.label("This document has no pages.");
            return;
        };

        // Cloning the context is cheap (it is an `Arc` internally) and avoids
        // holding an immutable borrow of `ui` across the mutable calls below.
        let ctx = ui.ctx().clone();

        let available = ui.available_size();
        let fit = fit_scale(FitMode::Width, geometry, available.x, available.y);

        // Rasterize at device pixels so the page is crisp on a HiDPI display,
        // but lay it out in points.
        let device_scale = fit * ctx.pixels_per_point();
        if self.needs_rasterize(device_scale) {
            self.rasterize(&ctx, device_scale);
        }

        if let Some(error) = &self.error {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Cannot render: {error}"),
            );
            return;
        }

        if let Some(texture) = &self.texture {
            let size = egui::vec2(geometry.width_pt * fit, geometry.height_pt * fit);
            let sized = egui::load::SizedTexture::new(texture.id(), size);
            ui.add(egui::Image::from_texture(sized));
        }
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("{} page(s)", self.document.page_count()));
            ui.separator();
            if let Some(geometry) = self.document.geometry().first() {
                ui.label(format!(
                    "{:.0}x{:.0} pt",
                    geometry.width_pt, geometry.height_pt
                ));
                ui.separator();
            }
            ui.label(format!("raster scale {:.2}x", self.texture_scale));
            ui.separator();
            ui.label("page 1, fit to width");
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

        if !self.screenshot_sent && self.frame >= request.warmup_frames {
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
        self.draw_page(ui);
    }
}

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
