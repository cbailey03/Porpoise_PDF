//! Test fixtures and comparison helpers.
//!
//! This crate exists so that corpus files, comparison logic, and eventually the
//! PDFium reference backend cannot leak into the shipped binary. Nothing here is
//! reachable from `porpoise-app`.
//!
//! Two things live here today:
//!
//! - [`single_page_pdf`], which synthesizes a valid PDF in memory, so the core
//!   pipeline can be tested in CI without vendoring any fixture files.
//! - [`pixel_diff`], the comparison primitive the differential-testing harness
//!   will be built on. See `docs/goal-1-plan.md`, section 1: no published
//!   hayro-vs-PDFium accuracy comparison exists, so we build the oracle
//!   ourselves at M6.

use porpoise_render::RenderedPage;

/// A minimal but valid single-page PDF, 200x100 points, containing one filled
/// blue rectangle.
#[must_use]
pub fn minimal_pdf() -> Vec<u8> {
    single_page_pdf(200, 100)
}

/// Synthesizes a valid single-page PDF of the given size in points, containing
/// one filled blue rectangle inset 20 points from each edge.
///
/// Offsets in the cross-reference table are computed from the assembled bytes
/// rather than hardcoded, so this stays correct as the object bodies change.
#[must_use]
pub fn single_page_pdf(width_pt: u32, height_pt: u32) -> Vec<u8> {
    let content = format!(
        "0 0 1 rg\n20 20 {} {} re\nf\n",
        width_pt.saturating_sub(40),
        height_pt.saturating_sub(40)
    )
    .into_bytes();

    let mut stream_object = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream_object.extend_from_slice(&content);
    stream_object.extend_from_slice(b"\nendstream");

    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width_pt} {height_pt}] \
             /Contents 4 0 R /Resources << >> >>"
        )
        .into_bytes(),
        stream_object,
    ];

    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.7\n");
    // A comment line with high bytes marks the file as binary for tools that
    // sniff it, and is conventional in real PDFs.
    pdf.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    let mut offsets = Vec::with_capacity(objects.len());
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }

    // Every cross-reference entry is exactly 20 bytes, including the trailing
    // "n \n". Getting this wrong is the classic way to hand a parser a file it
    // has to guess at.
    let startxref = pdf.len();
    let size = objects.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{startxref}\n%%EOF\n")
            .as_bytes(),
    );

    pdf
}

/// Two rasterized pages could not be compared.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// The two images are different sizes.
    #[error("dimension mismatch: {left_width}x{left_height} vs {right_width}x{right_height}")]
    DimensionMismatch {
        /// Width of the left image.
        left_width: u32,
        /// Height of the left image.
        left_height: u32,
        /// Width of the right image.
        right_width: u32,
        /// Height of the right image.
        right_height: u32,
    },
}

/// How far apart two rasterizations of the same page are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelDiff {
    /// Pixels where at least one channel differs by more than the tolerance.
    pub differing_pixels: usize,
    /// Total pixels compared.
    pub total_pixels: usize,
    /// Largest single-channel difference seen anywhere.
    pub max_channel_delta: u8,
}

impl PixelDiff {
    /// Fraction of pixels that differ, in `0.0..=1.0`. Zero for an empty image.
    #[must_use]
    pub fn fraction_differing(&self) -> f64 {
        if self.total_pixels == 0 {
            return 0.0;
        }
        // Both counts are pixel counts, so the precision loss is irrelevant at
        // any image size we can actually rasterize.
        #[allow(clippy::cast_precision_loss)]
        {
            self.differing_pixels as f64 / self.total_pixels as f64
        }
    }

    /// Whether the two images are identical within the tolerance used.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.differing_pixels == 0
    }
}

/// Compares two rasterizations of the same page channel by channel.
///
/// `tolerance` is the largest per-channel difference treated as equal, which
/// matters because two independent rasterizers will disagree slightly on
/// antialiased edges even when both are correct.
pub fn pixel_diff(
    left: &RenderedPage,
    right: &RenderedPage,
    tolerance: u8,
) -> Result<PixelDiff, DiffError> {
    if left.width != right.width || left.height != right.height {
        return Err(DiffError::DimensionMismatch {
            left_width: left.width,
            left_height: left.height,
            right_width: right.width,
            right_height: right.height,
        });
    }

    let mut differing_pixels = 0_usize;
    let mut max_channel_delta = 0_u8;

    for (left_pixel, right_pixel) in left.rgba.chunks_exact(4).zip(right.rgba.chunks_exact(4)) {
        let mut differs = false;
        for (left_channel, right_channel) in left_pixel.iter().zip(right_pixel.iter()) {
            let delta = left_channel.abs_diff(*right_channel);
            max_channel_delta = max_channel_delta.max(delta);
            if delta > tolerance {
                differs = true;
            }
        }
        if differs {
            differing_pixels += 1;
        }
    }

    Ok(PixelDiff {
        differing_pixels,
        total_pixels: left.rgba.chunks_exact(4).len(),
        max_channel_delta,
    })
}
