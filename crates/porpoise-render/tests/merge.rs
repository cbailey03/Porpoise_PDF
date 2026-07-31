//! Proof that saving a document assembled from more than one file means what it
//! says.
//!
//! Same evidentiary bar as `reorder.rs`: a merged file that merely *opens* is not
//! evidence the object graph was combined correctly, so every test here
//! **rasterizes the saved document and compares pixels** against the files it came
//! from. See `docs/goal-5-plan.md` §5 for why this can be trusted to work at all —
//! `lopdf` 0.44 ships the object-renumbering recipe this relies on as its own
//! `examples/merge.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use porpoise_doc::{Document, Overwrite, PageOrder, SaveError, Source, save_reordered};
use porpoise_render::{HayroRenderer, RenderRequest, RenderedPage, Renderer};
use porpoise_testkit::{multi_page_pdf, pixel_diff};

fn scratch(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    let _ = std::fs::remove_file(&path);
    path
}

fn fixture(name: &str, pages: usize) -> PathBuf {
    let path = scratch(name);
    // A different page size per fixture, so a page from "a" cannot be mistaken for
    // one from "b" purely by the rectangle inset `multi_page_pdf` already varies
    // per page within one document.
    std::fs::write(&path, multi_page_pdf(pages, 200, 100)).expect("should write the fixture");
    path
}

fn render(path: &PathBuf, page_index: usize) -> RenderedPage {
    let document = Document::open(path).expect("should open");
    HayroRenderer::new()
        .render(
            &document,
            RenderRequest {
                page_index,
                scale: 1.0,
            },
        )
        .expect("should rasterize")
}

fn assert_same(left: &RenderedPage, right: &RenderedPage, what: &str) {
    let diff = pixel_diff(left, right, 0).expect("same dimensions");
    assert!(diff.is_clean(), "{what}: {diff:?}");
}

fn primary(page: usize) -> Source {
    Source { document: 0, page }
}

fn inserted(page: usize) -> Source {
    Source { document: 1, page }
}

#[test]
fn merging_appends_the_second_documents_pages() {
    let a = fixture("merge-a.pdf", 2);
    let b = fixture("merge-b.pdf", 3);
    let before_a: Vec<RenderedPage> = (0..2).map(|page| render(&a, page)).collect();
    let before_b: Vec<RenderedPage> = (0..3).map(|page| render(&b, page)).collect();

    let mut order = PageOrder::identity(2);
    assert!(order.append(order.document_count(), 3));
    assert_eq!(order.len(), 5);

    let saved = scratch("merge-basic-out.pdf");
    save_reordered(&[a, b], &order, &saved, Overwrite::Refuse).expect("should merge and save");

    let document = Document::open(&saved).expect("the merged file should open");
    assert_eq!(document.page_count(), 5, "lost or gained pages while merging");

    for (position, expected) in before_a.iter().enumerate() {
        assert_same(
            &render(&saved, position),
            expected,
            &format!("merged position {position} should be the first document's page {position}"),
        );
    }
    for (index, expected) in before_b.iter().enumerate() {
        let position = 2 + index;
        assert_same(
            &render(&saved, position),
            expected,
            &format!("merged position {position} should be the second document's page {index}"),
        );
    }
}

#[test]
fn an_inserted_page_can_be_moved_before_saving() {
    // The point of modelling a merge as an ordinary edit: once appended, an
    // inserted page is reorderable with the tools that already exist, with no
    // special casing in the save path.
    let a = fixture("merge-move-a.pdf", 2);
    let b = fixture("merge-move-b.pdf", 2);
    let before_a: Vec<RenderedPage> = (0..2).map(|page| render(&a, page)).collect();
    let before_b: Vec<RenderedPage> = (0..2).map(|page| render(&b, page)).collect();

    let mut order = PageOrder::identity(2);
    assert!(order.append(1, 2));
    // [A0, A1, B0, B1] -> move B0 to the front -> [B0, A0, A1, B1].
    assert!(order.move_page(2, 0));
    assert_eq!(
        order.as_slice(),
        vec![inserted(0), primary(0), primary(1), inserted(1)]
    );

    let saved = scratch("merge-move-out.pdf");
    save_reordered(&[a, b], &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), 4);
    assert_same(&render(&saved, 0), &before_b[0], "position 0 should be B's first page");
    assert_same(&render(&saved, 1), &before_a[0], "position 1 should be A's first page");
    assert_same(&render(&saved, 2), &before_a[1], "position 2 should be A's second page");
    assert_same(&render(&saved, 3), &before_b[1], "position 3 should be B's second page");
}

#[test]
fn deleting_an_inserted_page_before_saving_drops_only_that_page() {
    let a = fixture("merge-delete-a.pdf", 1);
    let b = fixture("merge-delete-b.pdf", 2);
    let before_b: Vec<RenderedPage> = (0..2).map(|page| render(&b, page)).collect();

    let mut order = PageOrder::identity(1);
    assert!(order.append(1, 2));
    // [A0, B0, B1] -> delete B0 -> [A0, B1].
    assert!(order.remove(1));

    let saved = scratch("merge-delete-out.pdf");
    save_reordered(&[a.clone(), b], &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), 2);
    assert_same(&render(&saved, 0), &render(&a, 0), "position 0 should still be A's page");
    assert_same(
        &render(&saved, 1),
        &before_b[1],
        "position 1 should be B's second page, since its first was deleted",
    );
}

#[test]
fn a_page_count_mismatch_in_the_inserted_document_is_refused() {
    // The same guard `reorder.rs` proves for a single document, now checked
    // per-source: a `Source` claiming more pages of the second file than it has
    // would scramble the merge in a way nobody notices until later.
    let a = fixture("merge-mismatch-a.pdf", 2);
    let b = fixture("merge-mismatch-b.pdf", 2);

    let mut order = PageOrder::identity(2);
    assert!(order.append(1, 5)); // claims 5 pages; `b` only has 2.

    let saved = scratch("merge-mismatch-out.pdf");
    let error =
        save_reordered(&[a, b], &order, &saved, Overwrite::Refuse).expect_err("should refuse");
    assert!(
        matches!(error, SaveError::PageCountMismatch { .. }),
        "unexpected error: {error:?}"
    );
    assert!(!saved.exists(), "refused and still created a file");
}

#[test]
fn merging_a_third_document_folds_in_alongside_the_first_two() {
    let a = fixture("merge-three-a.pdf", 1);
    let b = fixture("merge-three-b.pdf", 1);
    let c = fixture("merge-three-c.pdf", 1);
    let before: Vec<RenderedPage> = [&a, &b, &c].iter().map(|path| render(path, 0)).collect();

    let mut order = PageOrder::identity(1);
    assert!(order.append(1, 1));
    assert!(order.append(2, 1));

    let saved = scratch("merge-three-out.pdf");
    save_reordered(&[a, b, c], &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), 3);
    for (position, expected) in before.iter().enumerate() {
        assert_same(&render(&saved, position), expected, &format!("position {position}"));
    }
}
