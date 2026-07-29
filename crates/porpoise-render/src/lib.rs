//! Rasterizing PDF pages to RGBA pixels.
//!
//! Rendering sits behind the [`Renderer`] trait for two reasons. The first is
//! that the differential-testing oracle (see `porpoise-testkit`) is a second
//! implementation of the same trait, so hayro's output can be pixel-diffed
//! against PDFium's without the rest of the app knowing. The second is that a
//! future GPU backend, built on hayro's `Device` trait, slots in at the same
//! seam. See `docs/goal-1-plan.md`, sections 1 and 5.
//!
//! # Untrusted input
//!
//! Every method here treats its input as hostile. hayro is pure Rust, so a
//! malformed PDF cannot corrupt memory — but it can still panic, and hayro has
//! open panic bugs today. A panic must degrade to one broken page, never take
//! down the process, so rasterization is wrapped in [`catch_unwind`].
//!
//! Resource limits and per-job timeouts are the other half of this and arrive at
//! M1; see `docs/goal-1-plan.md`, section 2.

use std::panic::{AssertUnwindSafe, catch_unwind};

use hayro::hayro_interpret::InterpreterSettings;
use porpoise_doc::Document;

/// hayro's viewport dimensions are `u16`, so this is a hard ceiling on the pixel
/// size of a single rasterized page regardless of zoom.
const MAX_PIXEL_DIMENSION: u32 = u16::MAX as u32;

/// A request to rasterize one page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderRequest {
    /// Zero-based page index.
    pub page_index: usize,
    /// Scale factor applied to the page's point dimensions. `1.0` yields 72 DPI.
    pub scale: f32,
}

/// A rasterized page, as tightly packed non-premultiplied RGBA8.
#[derive(Clone, PartialEq, Eq)]
pub struct RenderedPage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes of RGBA8.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for RenderedPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderedPage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .finish()
    }
}

/// A failure while rasterizing a page.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The requested page index is past the end of the document.
    #[error("page {index} is out of range (document has {count} pages)")]
    NoSuchPage {
        /// The requested index.
        index: usize,
        /// The document's page count.
        count: usize,
    },
    /// The scaled page is empty, non-finite, or larger than the backend can
    /// represent.
    #[error("page {index} does not rasterize to a usable size ({width}x{height} px)")]
    UnusableSize {
        /// The requested index.
        index: usize,
        /// Computed pixel width.
        width: f64,
        /// Computed pixel height.
        height: f64,
    },
    /// The backend panicked. Treated as a recoverable per-page error.
    #[error("renderer panicked while rasterizing page {index}")]
    Panicked {
        /// The requested index.
        index: usize,
    },
}

/// A PDF page rasterizer.
pub trait Renderer {
    /// Rasterizes a single page.
    fn render(
        &self,
        document: &Document,
        request: RenderRequest,
    ) -> Result<RenderedPage, RenderError>;
}

/// The default backend: pure-Rust rasterization via [`hayro`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HayroRenderer;

impl Renderer for HayroRenderer {
    fn render(
        &self,
        document: &Document,
        request: RenderRequest,
    ) -> Result<RenderedPage, RenderError> {
        let index = request.page_index;

        let geometry = document
            .geometry()
            .get(index)
            .ok_or(RenderError::NoSuchPage {
                index,
                count: document.page_count(),
            })?;

        // Validate the target size before handing anything to the backend: a
        // degenerate or absurd scale is the cheapest possible denial-of-service
        // vector, and rejecting it here keeps the failure legible.
        let scale = f64::from(request.scale);
        let width = f64::from(geometry.width_pt) * scale;
        let height = f64::from(geometry.height_pt) * scale;
        let unusable = || RenderError::UnusableSize {
            index,
            width,
            height,
        };
        if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
            return Err(unusable());
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (width, height) = (width.round() as u32, height.round() as u32);
        if width > MAX_PIXEL_DIMENSION || height > MAX_PIXEL_DIMENSION {
            return Err(unusable());
        }

        let pages = document.inner().pages();
        let page = pages.get(index).ok_or(RenderError::NoSuchPage {
            index,
            count: document.page_count(),
        })?;

        let render_settings = hayro::RenderSettings {
            x_scale: request.scale,
            y_scale: request.scale,
            ..Default::default()
        };
        let cache = hayro::RenderCache::new();
        let interpreter_settings = InterpreterSettings::default();

        // hayro is pure Rust, so the worst case here is a panic rather than
        // memory corruption. Contain it: one malformed page must not end the
        // process.
        let pixmap = catch_unwind(AssertUnwindSafe(|| {
            hayro::render(page, &cache, &interpreter_settings, &render_settings)
        }))
        .map_err(|_| {
            tracing::warn!(page = index, "renderer panicked; treating page as broken");
            RenderError::Panicked { index }
        })?;

        let (width, height) = (pixmap.width(), pixmap.height());

        // hayro hands back `Vec<Rgba8>`; flattening it costs one copy of the
        // page. Removable later with a bytemuck reinterpret if it shows up in a
        // profile — but the upload to a GPU texture is the interesting cost here,
        // not this.
        let rgba = pixmap
            .take_unpremultiplied()
            .into_iter()
            .flat_map(|pixel| pixel.to_u8_array())
            .collect();

        Ok(RenderedPage {
            width: u32::from(width),
            height: u32::from(height),
            rgba,
        })
    }
}
