//! End-to-end coverage of the open-and-rasterize path.
//!
//! These are the tests that prove the pure-Rust stack actually works on the
//! target platform, rather than merely resolving. They use a synthesized PDF so
//! CI needs no fixture files.

// Panicking is how a test reports failure. clippy's `allow-expect-in-tests`
// exempts `#[test]` functions and `#[cfg(test)]` modules, but not plain helpers
// in an integration-test crate, so opt in for the whole file.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use porpoise_doc::Document;
use porpoise_render::{HayroRenderer, RenderError, RenderRequest, Renderer};
use porpoise_testkit::{minimal_pdf, pixel_diff, single_page_pdf};

fn open_minimal() -> Document {
    Document::from_bytes(minimal_pdf()).expect("synthesized PDF should parse")
}

#[test]
fn synthesized_pdf_parses_with_expected_geometry() {
    let document = open_minimal();
    assert_eq!(document.page_count(), 1);

    let geometry = document.geometry();
    assert_eq!(geometry.len(), 1);
    let page = geometry.first().expect("one page");
    assert!((page.width_pt - 200.0).abs() < 0.01, "width was {page:?}");
    assert!((page.height_pt - 100.0).abs() < 0.01, "height was {page:?}");
}

#[test]
fn page_geometry_reflects_the_declared_media_box() {
    let document = Document::from_bytes(single_page_pdf(612, 792)).expect("should parse");
    let page = document.geometry().first().copied().expect("one page");
    assert!((page.width_pt - 612.0).abs() < 0.01);
    assert!((page.height_pt - 792.0).abs() < 0.01);
}

#[test]
fn garbage_bytes_are_a_parse_error_not_a_panic() {
    let error = Document::from_bytes(b"this is not a PDF at all".to_vec())
        .expect_err("garbage must not parse");
    assert!(
        matches!(error, porpoise_doc::DocumentError::Parse { .. }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn truncated_pdf_does_not_panic() {
    // Whatever the outcome, the requirement is that it returns rather than
    // unwinding: a malformed document is normal input for a PDF viewer.
    let mut bytes = minimal_pdf();
    bytes.truncate(bytes.len() / 2);
    let _ = Document::from_bytes(bytes);
}

#[test]
fn renders_at_one_to_one_scale() {
    let document = open_minimal();
    let page = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 1.0,
            },
        )
        .expect("minimal PDF should rasterize");

    assert_eq!((page.width, page.height), (200, 100));
    assert_eq!(page.rgba.len(), 200 * 100 * 4);
}

#[test]
fn scale_multiplies_the_output_resolution() {
    let document = open_minimal();
    let page = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 2.0,
            },
        )
        .expect("should rasterize at 2x");
    assert_eq!((page.width, page.height), (400, 200));
    assert_eq!(page.rgba.len(), 400 * 200 * 4);
}

#[test]
fn rendered_page_contains_the_drawn_rectangle() {
    // Guards against a silent all-white or all-transparent rasterization, which
    // would let every dimension assertion above pass while rendering nothing.
    let document = open_minimal();
    let page = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 1.0,
            },
        )
        .expect("should rasterize");

    let blue_ish = page
        .rgba
        .chunks_exact(4)
        .filter(|pixel| matches!(pixel, [r, g, b, _] if *b > 200 && *r < 80 && *g < 80))
        .count();

    // The rectangle is inset 20pt on each side of a 200x100 page: 160x60 = 9600.
    assert!(
        blue_ish > 8000,
        "expected roughly 9600 blue pixels, found {blue_ish}"
    );
}

#[test]
fn out_of_range_page_is_rejected() {
    let document = open_minimal();
    let error = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 7,
                scale: 1.0,
            },
        )
        .expect_err("page 7 does not exist");
    assert!(
        matches!(error, RenderError::NoSuchPage { index: 7, count: 1 }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn degenerate_and_absurd_scales_are_rejected_before_rasterizing() {
    let document = open_minimal();
    for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, 1.0e6] {
        let error = HayroRenderer
            .render(
                &document,
                RenderRequest {
                    page_index: 0,
                    scale,
                },
            )
            .expect_err("scale {scale} should be rejected");
        assert!(
            matches!(error, RenderError::UnusableSize { .. }),
            "scale {scale} gave unexpected error: {error:?}"
        );
    }
}

#[test]
fn identical_rasterizations_diff_clean() {
    let document = open_minimal();
    let request = RenderRequest {
        page_index: 0,
        scale: 1.0,
    };
    let left = HayroRenderer.render(&document, request).expect("render");
    let right = HayroRenderer.render(&document, request).expect("render");

    let diff = pixel_diff(&left, &right, 0).expect("same dimensions");
    assert!(diff.is_clean(), "rendering is not deterministic: {diff:?}");
    assert_eq!(diff.total_pixels, 200 * 100);
    assert_eq!(diff.max_channel_delta, 0);
}

#[test]
fn differently_sized_rasterizations_cannot_be_diffed() {
    let document = open_minimal();
    let small = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 1.0,
            },
        )
        .expect("render");
    let large = HayroRenderer
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 2.0,
            },
        )
        .expect("render");

    assert!(pixel_diff(&small, &large, 0).is_err());
}
