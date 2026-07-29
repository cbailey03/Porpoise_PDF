//! Test fixtures and comparison helpers.
//!
//! This crate exists so that fixtures and comparison logic cannot leak into the
//! shipped binary. Nothing here is reachable from `porpoise-app`, and a CI job
//! asserts as much.
//!
//! Three things live here:
//!
//! - [`single_page_pdf`], which synthesizes a valid PDF in memory, so the core
//!   pipeline can be tested in CI without vendoring any fixture files.
//! - [`Mutator`], which damages that PDF in thousands of deterministic ways, so
//!   the hardening claims in `docs/goal-1-plan.md` section 6a are measured rather
//!   than asserted.
//! - [`pixel_diff`], which compares two rasterizations channel by channel. Used
//!   to prove rendering is deterministic and that the threaded path produces the
//!   same pixels as the direct one. Comparing against a second *engine* is
//!   explicitly a non-goal; see `docs/goal-1-plan.md`, section 1.

use porpoise_render::RenderedPage;

/// Deterministic pseudo-random mutations of a valid PDF.
///
/// A PDF viewer's input is whatever someone hands it, so the interesting question
/// is not whether valid files work but whether damaged ones fail *safely*. This
/// generates damage of the kinds that occur both accidentally (truncated
/// downloads, bad transfers) and deliberately (crafted files).
///
/// Deterministic on purpose: a failure reproduces from its seed, which is the
/// difference between a useful bug report and a story about a flaky test.
pub struct Mutator {
    state: u64,
}

/// What a mutation did, for reporting which kind found a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Cut the file short.
    Truncated,
    /// Flipped one bit.
    BitFlipped,
    /// Zeroed a run of bytes.
    Zeroed,
    /// Overwrote a run with junk.
    Junked,
    /// Damaged the `%PDF-` header.
    HeaderDamaged,
    /// Damaged the trailing `startxref` offset.
    StartxrefDamaged,
    /// Duplicated a run of bytes, shifting every later offset.
    Duplicated,
}

impl Mutator {
    /// A mutator seeded for reproducibility.
    ///
    /// The seed is run through splitmix64's finalizer rather than combined with a
    /// constant directly. An earlier version used `seed | CONSTANT`, which *loses
    /// information*: any seed bit already set in the constant becomes
    /// indistinguishable, so seeds 42 and 43 produced byte-identical mutation
    /// sequences. That would have quietly collapsed a multi-seed sweep into a
    /// single one. Caught by `mutations_are_reproducible_from_their_seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // xorshift is degenerate at zero and stays there forever.
        Self {
            state: if z == 0 { 1 } else { z },
        }
    }

    /// xorshift64*, which is more than random enough for shuffling bytes and has
    /// no dependencies.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        // `usize` is 64-bit on every target we build for, and the shift keeps the
        // value positive regardless.
        let value = usize::try_from(self.next_u64() >> 1).unwrap_or(0);
        value % limit
    }

    /// Produces one damaged copy of `original`.
    #[must_use]
    pub fn mutate(&mut self, original: &[u8]) -> (Vec<u8>, Mutation) {
        if original.is_empty() {
            return (Vec::new(), Mutation::Truncated);
        }

        let kind = match self.below(7) {
            0 => Mutation::Truncated,
            1 => Mutation::BitFlipped,
            2 => Mutation::Zeroed,
            3 => Mutation::Junked,
            4 => Mutation::HeaderDamaged,
            5 => Mutation::StartxrefDamaged,
            _ => Mutation::Duplicated,
        };

        let mut bytes = original.to_vec();
        match kind {
            Mutation::Truncated => {
                let at = self.below(bytes.len());
                bytes.truncate(at);
            }
            Mutation::BitFlipped => {
                let at = self.below(bytes.len());
                let bit = self.below(8);
                if let Some(byte) = bytes.get_mut(at) {
                    *byte ^= 1 << bit;
                }
            }
            Mutation::Zeroed => {
                let at = self.below(bytes.len());
                let len = self.below(bytes.len() - at).max(1);
                if let Some(range) = bytes.get_mut(at..at + len) {
                    range.fill(0);
                }
            }
            Mutation::Junked => {
                let at = self.below(bytes.len());
                let len = self.below(bytes.len() - at).clamp(1, 64);
                for offset in 0..len {
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "deliberately taking the low byte"
                    )]
                    let junk = self.next_u64() as u8;
                    if let Some(byte) = bytes.get_mut(at + offset) {
                        *byte = junk;
                    }
                }
            }
            Mutation::HeaderDamaged => {
                // The first bytes decide whether this looks like a PDF at all.
                let at = self.below(8);
                if let Some(byte) = bytes.get_mut(at) {
                    *byte = b'X';
                }
            }
            Mutation::StartxrefDamaged => {
                // Point the cross-reference table somewhere useless, which is the
                // classic way a parser is sent hunting.
                if let Some(found) = find_last(&bytes, b"startxref") {
                    let digits = found + b"startxref\n".len();
                    for offset in 0..6 {
                        if let Some(byte) = bytes.get_mut(digits + offset)
                            && byte.is_ascii_digit()
                        {
                            *byte = b'9';
                        }
                    }
                }
            }
            Mutation::Duplicated => {
                let at = self.below(bytes.len());
                let len = self.below(bytes.len() - at).clamp(1, 128);
                let slice: Vec<u8> = bytes.get(at..at + len).unwrap_or_default().to_vec();
                bytes.splice(at..at, slice);
            }
        }

        (bytes, kind)
    }
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|start| haystack.get(*start..start + needle.len()) == Some(needle))
}

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
    multi_page_pdf(1, width_pt, height_pt)
}

/// Synthesizes a valid PDF of `pages` identically sized pages.
///
/// Each page carries a filled rectangle whose height shrinks with the page index,
/// so a rendering of page N is distinguishable from page M. Tests that navigate
/// need that: a fixture whose pages are pixel-identical cannot tell a successful
/// `go_to_page` from a silent no-op.
///
/// `pages` is clamped to at least one.
#[must_use]
pub fn multi_page_pdf(pages: usize, width_pt: u32, height_pt: u32) -> Vec<u8> {
    let pages = pages.max(1);

    // Objects 1 and 2 are the catalog and the page tree; each page then takes two
    // objects, a page dictionary followed by its content stream.
    let first_page_object = 3;
    let kids: Vec<String> = (0..pages)
        .map(|index| format!("{} 0 R", first_page_object + index * 2))
        .collect();

    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        )
        .into_bytes(),
    ];

    for index in 0..pages {
        let page_object = first_page_object + index * 2;
        let content_object = page_object + 1;

        // Shrink the rectangle a little per page so renders differ visibly.
        let inset = 20 + u32::try_from(index).unwrap_or(0) * 5;
        let box_width = width_pt.saturating_sub(inset * 2).max(1);
        let box_height = height_pt.saturating_sub(inset * 2).max(1);
        let content =
            format!("0 0 1 rg\n{inset} {inset} {box_width} {box_height} re\nf\n").into_bytes();

        let mut stream_object = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
        stream_object.extend_from_slice(&content);
        stream_object.extend_from_slice(b"\nendstream");

        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width_pt} {height_pt}] \
                 /Contents {content_object} 0 R /Resources << >> >>"
            )
            .into_bytes(),
        );
        objects.push(stream_object);
    }

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
