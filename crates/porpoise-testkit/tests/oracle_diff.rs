//! Differential rendering: hayro against PDFium, pixel for pixel.
//!
//! This is the test that decides whether the central bet of the project holds —
//! that a pure-Rust renderer is accurate enough to replace PDFium. Everything
//! else about hayro's accuracy is inference.
//!
//! Requires the `oracle` feature *and* a PDFium shared library at runtime. When
//! the library is absent these tests report a skip rather than failing: a missing
//! tool is not a broken build, and CI has no PDFium.
//!
//! ```text
//! cargo test -p porpoise-testkit --features oracle --test oracle_diff -- --nocapture
//! ```

#![cfg(feature = "oracle")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderRequest, Renderer};
use porpoise_testkit::oracle::{OracleError, PdfiumOracle};
use porpoise_testkit::{minimal_pdf, pixel_diff, single_page_pdf};

/// Per-channel difference treated as agreement.
///
/// Two independent rasterizers will never agree exactly on an antialiased edge,
/// and neither is wrong for it. This tolerance is about edge softness, not about
/// forgiving genuinely different output.
const TOLERANCE: u8 = 24;

/// Fraction of pixels allowed to exceed the tolerance.
///
/// Antialiasing differences concentrate on outlines, so a small fraction of a page
/// legitimately disagrees. A real rendering *bug* — a missing glyph, a wrong fill,
/// a misplaced image — moves far more than this.
const MAX_DIFFERING_FRACTION: f64 = 0.02;

/// Builds an oracle, or returns `None` having explained the skip.
fn oracle_or_skip(bytes: Vec<u8>) -> Option<PdfiumOracle> {
    match PdfiumOracle::new(bytes) {
        Ok(oracle) => Some(oracle),
        Err(error @ OracleError::Unavailable { .. }) => {
            println!("SKIPPED: {error}");
            None
        }
        Err(other) => panic!("oracle could not be constructed: {other}"),
    }
}

fn compare(bytes: Vec<u8>, scale: f32, label: &str) {
    let Some(oracle) = oracle_or_skip(bytes.clone()) else {
        return;
    };
    let document = Document::from_bytes(bytes).expect("fixture should parse");

    for page_index in 0..document.page_count() {
        let request = RenderRequest { page_index, scale };

        let ours = HayroRenderer::new()
            .render(&document, request)
            .expect("hayro should rasterize the fixture");
        let theirs = match oracle.render(&document, request) {
            Ok(page) => page,
            Err(error) => panic!("PDFium failed on {label} page {page_index}: {error}"),
        };

        assert_eq!(
            (ours.width, ours.height),
            (theirs.width, theirs.height),
            "{label} page {page_index}: dimensions disagree, so a pixel diff would be meaningless"
        );

        let diff = pixel_diff(&ours, &theirs, TOLERANCE).expect("same dimensions");
        let fraction = diff.fraction_differing();
        println!(
            "{label} page {page_index} at {scale}x: {:.4}% of pixels differ beyond \
             tolerance {TOLERANCE} (max channel delta {})",
            fraction * 100.0,
            diff.max_channel_delta
        );

        assert!(
            fraction <= MAX_DIFFERING_FRACTION,
            "{label} page {page_index}: {:.2}% of pixels disagree with PDFium, over the \
             {:.2}% budget — this is a rendering difference, not antialiasing",
            fraction * 100.0,
            MAX_DIFFERING_FRACTION * 100.0
        );
    }
}

#[test]
fn the_synthesized_fixture_agrees_with_pdfium() {
    compare(minimal_pdf(), 1.0, "minimal");
}

#[test]
fn agreement_holds_at_higher_resolution() {
    // Antialiasing differences shrink relative to page area as resolution rises,
    // so a diff that grows with scale indicates a real disagreement.
    compare(minimal_pdf(), 4.0, "minimal@4x");
}

#[test]
fn agreement_holds_on_a_letter_sized_page() {
    compare(single_page_pdf(612, 792), 1.0, "letter");
}

#[test]
fn both_renderers_agree_the_page_is_not_blank() {
    // Guards the whole comparison against the degenerate case where both engines
    // produce nothing and agree perfectly about it.
    let bytes = minimal_pdf();
    let Some(oracle) = oracle_or_skip(bytes.clone()) else {
        return;
    };
    let document = Document::from_bytes(bytes).expect("should parse");
    let request = RenderRequest {
        page_index: 0,
        scale: 1.0,
    };

    let count_blue = |rgba: &[u8]| {
        rgba.chunks_exact(4)
            .filter(|pixel| matches!(pixel, [r, g, b, _] if *b > 200 && *r < 80 && *g < 80))
            .count()
    };

    let ours = HayroRenderer::new()
        .render(&document, request)
        .expect("hayro render");
    let theirs = oracle.render(&document, request).expect("pdfium render");

    let (mine, other) = (count_blue(&ours.rgba), count_blue(&theirs.rgba));
    println!("blue pixels — hayro {mine}, pdfium {other}");
    assert!(mine > 8000, "hayro drew nothing: {mine} blue pixels");
    assert!(other > 8000, "pdfium drew nothing: {other} blue pixels");
}
