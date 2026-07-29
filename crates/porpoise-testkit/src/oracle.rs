//! A PDFium-backed reference renderer, for differential testing only.
//!
//! # Why this exists
//!
//! The central bet of this project is that a pure-Rust renderer is accurate
//! enough to replace PDFium (see `docs/goal-1-plan.md`, section 1). That belief
//! currently rests on circumstantial evidence: Typst ships hayro, it is tested
//! against a 1400-page corpus, and a handful of documents look right to the eye.
//! No published head-to-head comparison exists — hayro's own benchmark harness is
//! unbuilt by its author's admission, and its test suite compares against
//! self-generated baselines, making it a *regression* suite rather than a
//! correctness oracle.
//!
//! So we build the oracle. Rendering the same page through both engines and
//! diffing the pixels converts "looks right" into a number.
//!
//! # Why it is not a `Renderer`
//!
//! It would be tidy for this to implement [`porpoise_render::Renderer`] so the
//! diff harness could be generic. It does not, for a blunt reason: `RenderError`
//! describes the ways *our* renderer can fail, and inventing a variant for
//! "PDFium said no" would put a test-only concern in the shipped crate. The
//! oracle gets its own error type instead.
//!
//! # Runtime requirement
//!
//! PDFium is a C++ library loaded at runtime; this crate does not bundle it.
//! [`PdfiumOracle::new`] returns [`OracleError::Unavailable`] when no library can
//! be found, and callers are expected to skip rather than fail — the absence of a
//! binary is a missing tool, not a broken build.

use pdfium_render::prelude::*;
use porpoise_doc::Document;
use porpoise_render::{RenderRequest, RenderedPage};

/// Why the oracle could not produce a rendering.
#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    /// No PDFium library could be loaded.
    ///
    /// Expected on a machine that has not been given one. Treat as "skip", not
    /// "fail".
    #[error(
        "no PDFium library available ({detail}). \
         Place one beside the binary or on the system library path."
    )]
    Unavailable {
        /// What the binding attempt reported.
        detail: String,
    },
    /// PDFium loaded but refused the document or page.
    #[error("PDFium could not render page index {index}: {detail}")]
    Failed {
        /// The page that failed.
        index: usize,
        /// What PDFium reported.
        detail: String,
    },
    /// The requested page does not exist, or its size is unusable.
    #[error("page index {index} cannot be rendered at scale {scale}")]
    Unrenderable {
        /// The requested page.
        index: usize,
        /// The requested scale.
        scale: f32,
    },
}

/// A reference renderer backed by PDFium.
pub struct PdfiumOracle {
    pdfium: Pdfium,
    /// The document bytes.
    ///
    /// Held separately because PDFium needs the original file, and our
    /// [`Document`] deliberately does not expose its bytes. The caller is
    /// responsible for these being the same file it opened.
    bytes: Vec<u8>,
}

impl PdfiumOracle {
    /// Binds to a PDFium library and prepares to render `bytes`.
    ///
    /// Looks beside the executable first, then on the system library path.
    pub fn new(bytes: Vec<u8>) -> Result<Self, OracleError> {
        let local = Pdfium::pdfium_platform_library_name_at_path("./");
        let bindings = Pdfium::bind_to_library(local)
            .or_else(|_| Pdfium::bind_to_system_library())
            .map_err(|error| OracleError::Unavailable {
                detail: error.to_string(),
            })?;

        Ok(Self {
            pdfium: Pdfium::new(bindings),
            bytes,
        })
    }

    /// Rasterizes a page at the same pixel dimensions our renderer would use.
    ///
    /// Matching dimensions exactly is the whole point: a pixel diff between
    /// differently sized images is meaningless.
    pub fn render(
        &self,
        document: &Document,
        request: RenderRequest,
    ) -> Result<RenderedPage, OracleError> {
        let index = request.page_index;
        let unrenderable = || OracleError::Unrenderable {
            index,
            scale: request.scale,
        };

        let geometry = document.geometry().get(index).ok_or_else(unrenderable)?;
        let scale = f64::from(request.scale);
        let width = (f64::from(geometry.width_pt) * scale).round();
        let height = (f64::from(geometry.height_pt) * scale).round();
        if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
            return Err(unrenderable());
        }
        let (Ok(width), Ok(height)) = (i32::try_from(width as i64), i32::try_from(height as i64))
        else {
            return Err(unrenderable());
        };

        let failed = |detail: String| OracleError::Failed { index, detail };

        let loaded = self
            .pdfium
            .load_pdf_from_byte_slice(&self.bytes, None)
            .map_err(|error| failed(error.to_string()))?;

        let page_index = i32::try_from(index).map_err(|_| unrenderable())?;
        let page = loaded
            .pages()
            .get(page_index)
            .map_err(|error| failed(error.to_string()))?;

        let config = PdfRenderConfig::new()
            .set_target_width(width)
            .set_target_height(height);
        let bitmap = page
            .render_with_config(&config)
            .map_err(|error| failed(error.to_string()))?;

        let rgba = bitmap.as_rgba_bytes();
        let (Ok(width), Ok(height)) = (
            u32::try_from(bitmap.width()),
            u32::try_from(bitmap.height()),
        ) else {
            return Err(unrenderable());
        };

        Ok(RenderedPage {
            width,
            height,
            rgba,
        })
    }
}
