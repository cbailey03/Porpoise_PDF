//! What a save does to the page tree itself.
//!
//! `porpoise-render/tests/reorder.rs` proves the saved pages still *look* right, which
//! is the claim that matters. These check the structure underneath it, because the two
//! can come apart: a save that quietly stopped flattening, or one that started writing
//! attributes onto pages that never had them, would keep every pixel test green while
//! producing a different document than intended.
//!
//! This is the only place a test can read the raw tree, since `lopdf` is a dependency
//! of this crate and of nothing above it.

// Panicking is how a test reports failure, and clippy's `allow-*-in-tests` does not
// reach plain helpers in an integration-test crate.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use lopdf::{Document as LoDocument, Object, ObjectId};
use porpoise_doc::{Overwrite, PageOrder, save_reordered};
use porpoise_testkit::{multi_page_pdf, nested_page_tree_pdf};

/// Pages in the nested fixture.
const PAGES: usize = 4;

fn scratch(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// Writes `bytes` where a save can read them, since saving works from a path.
fn fixture(name: &str, bytes: Vec<u8>) -> PathBuf {
    let path = scratch(name);
    std::fs::write(&path, bytes).expect("should write the fixture");
    path
}

/// Reverses a document of `PAGES` pages, which moves every page across a branch.
fn reversed() -> PageOrder {
    let mut order = PageOrder::identity(PAGES);
    for step in 0..PAGES {
        order.move_page(PAGES - 1, step);
    }
    assert_eq!(order.as_slice(), &[3, 2, 1, 0]);
    order
}

/// The object ids in the root's `/Kids`.
fn root_kids(document: &LoDocument) -> Vec<ObjectId> {
    let root = document
        .catalog()
        .expect("should have a catalog")
        .get(b"Pages")
        .expect("should name a page tree")
        .as_reference()
        .expect("should be a reference");
    document
        .get_dictionary(root)
        .expect("should be a dictionary")
        .get(b"Kids")
        .expect("should have kids")
        .as_array()
        .expect("should be an array")
        .iter()
        .map(|kid| kid.as_reference().expect("should be a reference"))
        .collect()
}

#[test]
fn the_saved_page_tree_has_no_branches_left() {
    let source = fixture("save-flatten.pdf", nested_page_tree_pdf());
    let saved = scratch("save-flatten-out.pdf");

    save_reordered(&source, &reversed(), &saved, Overwrite::Refuse).expect("should save");

    let document = LoDocument::load(&saved).expect("the saved file should parse");
    let kids = root_kids(&document);
    assert_eq!(
        kids.len(),
        PAGES,
        "the root should hold every page directly"
    );
    for kid in kids {
        let dictionary = document
            .get_dictionary(kid)
            .expect("should be a dictionary");
        assert!(
            dictionary.has_type(b"Page"),
            "a branch survived the flatten: {dictionary:?}"
        );
    }
}

#[test]
fn every_saved_page_defines_the_attributes_it_used_to_inherit() {
    let source = fixture("save-inherited.pdf", nested_page_tree_pdf());
    let saved = scratch("save-inherited-out.pdf");

    save_reordered(&source, &reversed(), &saved, Overwrite::Refuse).expect("should save");

    let document = LoDocument::load(&saved).expect("the saved file should parse");
    for kid in root_kids(&document) {
        let dictionary = document
            .get_dictionary(kid)
            .expect("should be a dictionary");
        for key in [b"MediaBox".as_slice(), b"Rotate", b"Resources"] {
            assert!(
                dictionary.has(key),
                "page is missing {}, which it used to inherit: {dictionary:?}",
                String::from_utf8_lossy(key)
            );
        }
    }
}

#[test]
fn the_two_branches_geometry_survives_as_two_different_page_sizes() {
    let source = fixture("save-sizes.pdf", nested_page_tree_pdf());
    let saved = scratch("save-sizes-out.pdf");

    save_reordered(&source, &reversed(), &saved, Overwrite::Refuse).expect("should save");

    let document = LoDocument::load(&saved).expect("the saved file should parse");
    let boxes: Vec<Object> = root_kids(&document)
        .into_iter()
        .map(|kid| {
            document
                .get_dictionary(kid)
                .expect("should be a dictionary")
                .get(b"MediaBox")
                .expect("should define a media box")
                .clone()
        })
        .collect();

    // Reversed, so the two pages from the second branch come first. Collapsing all
    // four onto one size is exactly what dropping the inheritance would look like.
    assert_eq!(format!("{:?}", boxes[0]), format!("{:?}", boxes[1]));
    assert_eq!(format!("{:?}", boxes[2]), format!("{:?}", boxes[3]));
    assert_ne!(
        format!("{:?}", boxes[0]),
        format!("{:?}", boxes[2]),
        "both branches ended up the same size"
    );
}

#[test]
fn a_flat_document_is_not_given_attributes_it_never_had() {
    // The push-down must fill gaps, not invent defaults. A flat document has no
    // inherited attributes to carry, so its pages have to come out as they went in.
    let source = fixture("save-flat.pdf", multi_page_pdf(PAGES, 200, 100));
    let saved = scratch("save-flat-out.pdf");

    let before = LoDocument::load(&source).expect("the fixture should parse");
    let before_rotate: Vec<bool> = root_kids(&before)
        .into_iter()
        .map(|kid| {
            before
                .get_dictionary(kid)
                .expect("should be a dictionary")
                .has(b"Rotate")
        })
        .collect();
    assert_eq!(before_rotate, vec![false; PAGES], "fixture changed shape");

    save_reordered(&source, &reversed(), &saved, Overwrite::Refuse).expect("should save");

    let document = LoDocument::load(&saved).expect("the saved file should parse");
    for kid in root_kids(&document) {
        let dictionary = document
            .get_dictionary(kid)
            .expect("should be a dictionary");
        assert!(
            !dictionary.has(b"Rotate"),
            "a rotation was invented for a page that had none: {dictionary:?}"
        );
    }
}
