//! Direct coverage of this crate's own surface.
//!
//! Everything here was already exercised indirectly, through the render pipeline's
//! tests — which meant `cargo test -p porpoise-doc` reported "0 passed" while the
//! behaviour was in fact well covered somewhere else. A green zero reads as "nothing
//! to check", so these assert the things this crate promises on its own terms:
//! geometry is available immediately on open, and every failure mode is an error
//! rather than a panic.
//!
//! Damaged-input coverage at scale lives in `porpoise-render/tests/malformed.rs`,
//! which needs the renderer as well and so cannot live here without inverting the
//! layering.

// Panicking is how a test reports failure, and clippy's `allow-*-in-tests` does not
// reach plain helpers in an integration-test crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use porpoise_doc::{Document, DocumentError};
use porpoise_testkit::{minimal_pdf, multi_page_pdf, single_page_pdf};

#[test]
fn a_synthesized_pdf_opens_and_reports_its_geometry() {
    let document = Document::from_bytes(single_page_pdf(200, 300)).expect("should open");
    assert_eq!(document.page_count(), 1);

    let geometry = document.geometry();
    assert_eq!(geometry.len(), 1, "geometry must cover every page");
    assert!((geometry[0].width_pt - 200.0).abs() < 0.5, "{geometry:?}");
    assert!((geometry[0].height_pt - 300.0).abs() < 0.5, "{geometry:?}");
}

#[test]
fn geometry_is_available_immediately_rather_than_lazily() {
    // The reason this crate computes it eagerly: the scrolling viewport cannot be
    // laid out until every page's size is known, so a lazy `geometry()` would make
    // the first frame either wrong or blocking.
    let document = Document::from_bytes(multi_page_pdf(6, 200, 300)).expect("should open");
    assert_eq!(document.page_count(), 6);
    assert_eq!(document.geometry().len(), 6);
}

#[test]
fn page_count_and_geometry_agree() {
    // They are the same fact reported two ways, and layout indexes one by the other.
    for pages in [1, 2, 9] {
        let document = Document::from_bytes(multi_page_pdf(pages, 200, 300)).expect("should open");
        assert_eq!(document.page_count(), document.geometry().len(), "{pages}");
    }
}

#[test]
fn a_minimal_pdf_is_still_a_pdf() {
    let document = Document::from_bytes(minimal_pdf()).expect("should open");
    assert!(document.page_count() >= 1);
}

#[test]
fn bytes_that_are_not_a_pdf_are_refused_rather_than_panicking() {
    let error =
        Document::from_bytes(b"this is not a PDF at all".to_vec()).expect_err("should be refused");
    // Either is acceptable; a panic escaping is not. `ParserPanicked` means
    // `catch_unwind` did its job, which is the interesting half.
    assert!(
        matches!(
            error,
            DocumentError::Parse { .. } | DocumentError::ParserPanicked
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn no_bytes_at_all_are_refused() {
    assert!(Document::from_bytes(Vec::new()).is_err());
}

#[test]
fn a_missing_file_names_the_path_it_tried() {
    // The message reaches the status bar since Goal 3, so it has to say which file.
    let error = Document::open("definitely-not-here-9f3a.pdf").expect_err("should be refused");
    assert!(
        matches!(error, DocumentError::Io { .. }),
        "unexpected error: {error:?}"
    );
    assert!(
        error.to_string().contains("definitely-not-here-9f3a"),
        "unhelpful: {error}"
    );
}

#[test]
fn debug_reports_the_page_count_without_dumping_the_document() {
    // `Document` holds a whole parsed PDF; a derived `Debug` would be unusable in a
    // log line, which is the only place this gets printed.
    let document = Document::from_bytes(multi_page_pdf(3, 200, 300)).expect("should open");
    let shown = format!("{document:?}");
    assert!(shown.contains("page_count"), "{shown}");
    assert!(shown.contains('3'), "{shown}");
}
