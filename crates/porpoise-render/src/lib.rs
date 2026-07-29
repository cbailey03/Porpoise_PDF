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
//! Every function here treats its input as hostile, and there are three distinct
//! failure modes to defend against. Only the first is solved by choosing a
//! memory-safe language.
//!
//! - **Memory corruption.** Not possible: hayro is pure Rust. This is the whole
//!   reason we are not using PDFium.
//! - **Panics.** hayro has open panic bugs today. [`HayroRenderer`] wraps
//!   rasterization in [`catch_unwind`], so a malformed page degrades to
//!   [`RenderError::Panicked`] rather than ending the process.
//! - **Resource exhaustion.** A crafted or merely awkward document can ask for
//!   an absurd allocation, or send the interpreter into a very long loop.
//!   [`RenderLimits`] bounds the allocation before it happens, and
//!   [`render_with_timeout`] bounds the time.
//!
//! Note that hayro is `!UnwindSafe` because it holds interior-mutable caches, so
//! [`AssertUnwindSafe`] is required rather than merely convenient. The assertion
//! is sound for the way we use it — a [`Document`] whose render panicked is
//! still safe to *read*, and we discard the per-call cache — but resuming a
//! partially-unwound cache is exactly the kind of thing worth staying nervous
//! about, so prefer discarding a document that has panicked repeatedly.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use hayro::hayro_interpret::InterpreterSettings;
use porpoise_doc::Document;

/// hayro's viewport dimensions are `u16`, so no single rasterized page can
/// exceed this along either axis regardless of the limits we choose.
pub const BACKEND_MAX_DIMENSION: u32 = u16::MAX as u32;

/// Bounds on what one render request may consume.
///
/// These exist to make a hostile or merely enormous page fail fast and legibly
/// instead of exhausting memory. Checking dimensions alone is not enough: a
/// 65535x65535 page is within the backend's per-axis limit and still asks for
/// roughly 17 GB of RGBA, which is why [`Self::max_total_pixels`] exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    /// Largest permitted width or height in pixels. Effectively capped at
    /// [`BACKEND_MAX_DIMENSION`] whatever is set here.
    pub max_pixel_dimension: u32,
    /// Largest permitted `width * height`. Multiply by four for the byte cost.
    pub max_total_pixels: u64,
}

impl RenderLimits {
    /// 64 megapixels, or 256 MB as RGBA.
    ///
    /// Chosen to leave room for a legitimately large render — a tabloid page at
    /// 600 DPI is about 42 Mpx — while refusing anything that could not plausibly
    /// be wanted. A viewport-driven caller should set something much smaller;
    /// Goal 1's memory target for a whole document is 500 MB.
    pub const DEFAULT_MAX_TOTAL_PIXELS: u64 = 64 << 20;

    /// The effective per-axis cap, accounting for the backend's own hard limit.
    #[must_use]
    pub fn effective_max_dimension(&self) -> u32 {
        self.max_pixel_dimension.min(BACKEND_MAX_DIMENSION)
    }
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_pixel_dimension: BACKEND_MAX_DIMENSION,
            max_total_pixels: Self::DEFAULT_MAX_TOTAL_PIXELS,
        }
    }
}

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

impl RenderedPage {
    /// Encodes the page as a PNG.
    ///
    /// Returns bytes rather than writing a file, so this stays usable from tests
    /// and keeps filesystem policy in the caller.
    pub fn encode_png(&self) -> Result<Vec<u8>, EncodePngError> {
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(4));
        if expected != Some(self.rgba.len()) {
            return Err(EncodePngError::Malformed {
                width: self.width,
                height: self.height,
                len: self.rgba.len(),
            });
        }

        let mut out = Vec::new();
        // Scoped so the writer is flushed and drops its borrow of `out`.
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&self.rgba)?;
            writer.finish()?;
        }
        Ok(out)
    }
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

/// PNG encoding failed.
#[derive(Debug, thiserror::Error)]
pub enum EncodePngError {
    /// The buffer length does not match the stated dimensions.
    #[error("buffer of {len} bytes does not match {width}x{height} RGBA")]
    Malformed {
        /// Stated width.
        width: u32,
        /// Stated height.
        height: u32,
        /// Actual buffer length.
        len: usize,
    },
    /// The PNG encoder rejected the image.
    #[error("PNG encoder failed")]
    Encoder(#[from] png::EncodingError),
}

/// A failure while rasterizing a page.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The requested page index is past the end of the document.
    #[error("page index {index} is out of range (document has {count} pages)")]
    NoSuchPage {
        /// The requested index.
        index: usize,
        /// The document's page count.
        count: usize,
    },
    /// The scaled page is empty or its size is not a finite number.
    #[error("page index {index} does not rasterize to a usable size ({width}x{height} px)")]
    UnusableSize {
        /// The requested index.
        index: usize,
        /// Computed pixel width.
        width: f64,
        /// Computed pixel height.
        height: f64,
    },
    /// The scaled page exceeds the per-axis limit.
    #[error(
        "page index {index} at this scale is {width}x{height} px, \
         over the {max} px per-axis limit"
    )]
    DimensionTooLarge {
        /// The requested index.
        index: usize,
        /// Computed pixel width.
        width: u32,
        /// Computed pixel height.
        height: u32,
        /// The per-axis limit that was exceeded.
        max: u32,
    },
    /// The scaled page exceeds the total-pixel limit.
    ///
    /// Distinct from [`Self::DimensionTooLarge`] because a page can be within the
    /// per-axis limit on both axes and still be an unreasonable allocation.
    #[error(
        "page index {index} at this scale is {width}x{height} px = {total_pixels} pixels, \
         over the limit of {max_total_pixels}"
    )]
    AreaTooLarge {
        /// The requested index.
        index: usize,
        /// Computed pixel width.
        width: u32,
        /// Computed pixel height.
        height: u32,
        /// Computed `width * height`.
        total_pixels: u64,
        /// The limit that was exceeded.
        max_total_pixels: u64,
    },
    /// The backend panicked. Treated as a recoverable per-page error.
    #[error("renderer panicked while rasterizing page index {index}")]
    Panicked {
        /// The requested index.
        index: usize,
    },
    /// The backend did not finish within the allotted time.
    #[error("rasterizing page index {index} exceeded the {timeout_ms} ms budget")]
    TimedOut {
        /// The requested index.
        index: usize,
        /// The budget that was exceeded, in milliseconds.
        timeout_ms: u128,
    },
}

/// A PDF page rasterizer.
pub trait Renderer {
    /// Rasterizes a single page.
    ///
    /// Implementations must return an error rather than panicking, and must
    /// respect their configured [`RenderLimits`].
    fn render(
        &self,
        document: &Document,
        request: RenderRequest,
    ) -> Result<RenderedPage, RenderError>;
}

/// The default backend: pure-Rust rasterization via [`hayro`].
#[derive(Debug, Clone, Copy, Default)]
pub struct HayroRenderer {
    limits: RenderLimits,
}

impl HayroRenderer {
    /// A renderer with [`RenderLimits::default`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A renderer with explicit limits.
    #[must_use]
    pub fn with_limits(limits: RenderLimits) -> Self {
        Self { limits }
    }

    /// The limits in force.
    #[must_use]
    pub fn limits(&self) -> RenderLimits {
        self.limits
    }

    /// Validates the target raster size without allocating anything.
    ///
    /// Split out from [`Renderer::render`] so the arithmetic can be tested
    /// directly, and because rejecting a bad size is the cheapest possible
    /// defence — it happens before the backend sees the request at all.
    fn target_size(
        &self,
        index: usize,
        page: &porpoise_doc::PageGeometry,
        scale: f32,
    ) -> Result<(u32, u32), RenderError> {
        let scale = f64::from(scale);
        let width = f64::from(page.width_pt) * scale;
        let height = f64::from(page.height_pt) * scale;

        if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
            return Err(RenderError::UnusableSize {
                index,
                width,
                height,
            });
        }

        // Reject before casting. `as` saturates rather than wrapping, so a huge
        // float would silently become u32::MAX and pass as merely large.
        let max = self.limits.effective_max_dimension();
        if width > f64::from(max) || height > f64::from(max) {
            return Err(RenderError::DimensionTooLarge {
                index,
                width: width.min(f64::from(u32::MAX)) as u32,
                height: height.min(f64::from(u32::MAX)) as u32,
                max,
            });
        }

        let width = width.round() as u32;
        let height = height.round() as u32;

        // Rounding can push a value one over the cap.
        if width > max || height > max {
            return Err(RenderError::DimensionTooLarge {
                index,
                width,
                height,
                max,
            });
        }

        let total_pixels = u64::from(width) * u64::from(height);
        if total_pixels > self.limits.max_total_pixels {
            return Err(RenderError::AreaTooLarge {
                index,
                width,
                height,
                total_pixels,
                max_total_pixels: self.limits.max_total_pixels,
            });
        }

        Ok((width, height))
    }
}

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

        // Bound the allocation before the backend is involved.
        self.target_size(index, geometry, request.scale)?;

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

        // See the crate docs on why AssertUnwindSafe is required here.
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

/// Rasterizes a page, giving up after `timeout`.
///
/// [`catch_unwind`] handles a backend that *crashes*; this handles one that
/// simply does not come back. hayro can loop for a very long time on pathological
/// input, and no amount of memory safety helps with that.
///
/// # The thread is abandoned, not cancelled
///
/// Rust cannot cancel a running thread, so on timeout the worker is left to
/// finish on its own while this function returns [`RenderError::TimedOut`]. The
/// caller regains control immediately, which is the point, but be clear about
/// what that costs: a genuinely infinite loop will occupy one core until the
/// process exits, and repeated timeouts accumulate threads.
///
/// That is acceptable for a one-shot CLI render. It is *not* an adequate answer
/// for the viewer, which needs a bounded worker pool so timeouts cannot pile up,
/// and eventually a separate process so a hung render can actually be killed.
/// Tracked in `docs/goal-1-plan.md`, section 2.
pub fn render_with_timeout<R>(
    renderer: R,
    document: Arc<Document>,
    request: RenderRequest,
    timeout: Duration,
) -> Result<RenderedPage, RenderError>
where
    R: Renderer + Send + 'static,
{
    let index = request.page_index;
    let (sender, receiver) = mpsc::channel();

    std::thread::spawn(move || {
        let result = renderer.render(&document, request);
        // A send error means we already timed out and the receiver is gone.
        // Nothing to report to, and nothing worth logging.
        drop(sender.send(result));
    });

    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(
                page = index,
                timeout_ms = timeout.as_millis(),
                "render exceeded its time budget; abandoning the worker"
            );
            Err(RenderError::TimedOut {
                index,
                timeout_ms: timeout.as_millis(),
            })
        }
        // The worker vanished without sending. `render` catches panics itself, so
        // reaching here means something stranger happened — report it as a panic
        // rather than inventing a new case.
        Err(RecvTimeoutError::Disconnected) => Err(RenderError::Panicked { index }),
    }
}
