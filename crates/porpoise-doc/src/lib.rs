//! Document model for a single PDF: opening it, and describing the geometry of
//! its pages.
//!
//! This crate deliberately knows nothing about rendering or windowing. Page
//! geometry has to be known *before* any page is rasterized, because the
//! scrolling viewport cannot be laid out without it — so it lives here rather
//! than in `porpoise-render`.
//!
//! This is also the seam where `lopdf` joins for incremental save once the editor
//! phase begins. See `docs/goal-1-plan.md`, section 1.

mod order;
mod save;

pub use order::{PageOrder, Source};
pub use save::{Overwrite, SaveError, save_reordered};

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use hayro_syntax::Pdf;

/// A failure while opening or parsing a PDF.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    /// The file could not be read from disk.
    #[error("could not read {path}")]
    Io {
        /// The path we tried to read.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The bytes were not a PDF we could parse.
    #[error("could not parse PDF: {detail}")]
    Parse {
        /// Parser-supplied description of the failure.
        detail: String,
    },
    /// The parser panicked.
    ///
    /// Distinct from [`Self::Parse`] because a panic is a *bug*, in hayro or in
    /// our use of it, rather than an ordinary rejection of bad input. Callers
    /// should treat both as "this file cannot be opened", but the distinction
    /// matters when triaging.
    #[error("the PDF parser panicked; the file is likely malformed in a way it mishandles")]
    ParserPanicked,
}

/// The laid-out size of a single page, in PDF points (1/72 inch).
///
/// These dimensions already account for the crop box and the page's `/Rotate`
/// entry, so they are directly usable for layout. Real PDFs mix page sizes and
/// rotations freely within one document, which is why scroll offset cannot be
/// computed as `page_index * page_height`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageGeometry {
    /// Width in PDF points, after cropping and rotation.
    pub width_pt: f32,
    /// Height in PDF points, after cropping and rotation.
    pub height_pt: f32,
}

/// An open PDF document.
pub struct Document {
    pdf: Pdf,
    geometry: Vec<PageGeometry>,
}

impl Document {
    /// Opens and parses the PDF at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DocumentError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| DocumentError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_bytes(bytes)
    }

    /// Parses a PDF already in memory.
    ///
    /// Page geometry for the whole document is computed eagerly here. This is
    /// cheap — it reads the page tree without touching any content stream — and
    /// it means the viewport can be laid out immediately on open.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, DocumentError> {
        // A malformed PDF is ordinary input for a viewer, and hayro has open panic
        // bugs, so parsing is contained the same way rasterization is. Without
        // this a crafted file takes down the whole application rather than failing
        // to open one document.
        //
        // `AssertUnwindSafe` is required because hayro's types hold
        // interior-mutable caches. It is sound here in the strongest possible
        // sense: on a panic every partially built value is dropped and nothing
        // observable survives.
        let parsed = catch_unwind(AssertUnwindSafe(|| {
            let pdf = Pdf::new(bytes).map_err(|err| DocumentError::Parse {
                detail: format!("{err:?}"),
            })?;

            let geometry = pdf
                .pages()
                .iter()
                .map(|page| {
                    let (width_pt, height_pt) = page.render_dimensions();
                    PageGeometry {
                        width_pt,
                        height_pt,
                    }
                })
                .collect();

            Ok(Self { pdf, geometry })
        }));

        parsed.unwrap_or(Err(DocumentError::ParserPanicked))
    }

    /// The number of pages in the document.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.geometry.len()
    }

    /// Laid-out size of every page, in document order.
    #[must_use]
    pub fn geometry(&self) -> &[PageGeometry] {
        &self.geometry
    }

    /// Access to the underlying parsed PDF.
    ///
    /// This leaks `hayro` through our public API, which is a seam we accept for
    /// now: `porpoise-render` needs the parsed page to rasterize it, and
    /// re-parsing would be wasteful. Revisit when the `Device`-trait render
    /// backend lands, at which point rendering can be expressed against our own
    /// types instead.
    #[must_use]
    pub fn inner(&self) -> &Pdf {
        &self.pdf
    }
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("page_count", &self.page_count())
            .finish_non_exhaustive()
    }
}
