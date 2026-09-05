//! Proof that a reordered save means what it says.
//!
//! Reordering is the one operation in this project that writes to disk, so "the file
//! parsed afterwards" is nowhere near enough — a scrambled page tree parses fine. Every
//! test here **rasterizes the saved document and compares pixels** against the pages it
//! came from. If page 1 of the saved file does not look exactly like page 3 of the
//! original, the bytes did not mean what we intended.
//!
//! Lives in `porpoise-render`'s tests because it needs the renderer to check the
//! result, `porpoise-doc` to write it, and `porpoise-testkit` for fixtures — and
//! `porpoise-render` is the one crate where all three are in scope without inverting
//! the layering.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use porpoise_doc::{Document, Overwrite, PageOrder, SaveError, save_reordered};
use porpoise_render::{HayroRenderer, RenderRequest, RenderedPage, Renderer};
use porpoise_testkit::{multi_page_pdf, nested_page_tree_pdf, pixel_diff, single_page_pdf};

/// Pages in the fixture. Each one's rectangle is inset differently, so any two
/// rasterize to visibly different images — without that, a reorder test could pass on
/// a document whose pages are indistinguishable.
const PAGES: usize = 4;

fn scratch(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    path.push(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// Writes the fixture to disk, since saving reads from a path.
fn fixture(name: &str) -> PathBuf {
    let path = scratch(name);
    std::fs::write(&path, multi_page_pdf(PAGES, 200, 100)).expect("should write the fixture");
    path
}

fn render(path: &Path, page_index: usize) -> RenderedPage {
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

/// Asserts that two rasterizations are pixel-for-pixel the same.
fn assert_same(left: &RenderedPage, right: &RenderedPage, what: &str) {
    let diff = pixel_diff(left, right, 0).expect("same dimensions");
    assert!(diff.is_clean(), "{what}: {diff:?}");
}

/// Asserts that two rasterizations differ, so a test comparing them proves something.
fn assert_different(left: &RenderedPage, right: &RenderedPage, what: &str) {
    let diff = pixel_diff(left, right, 0).expect("same dimensions");
    assert!(
        !diff.is_clean(),
        "{what}: these pages are identical, so this test cannot detect a wrong order"
    );
}

/// Writes the nested-page-tree fixture, whose pages inherit their geometry.
fn nested_fixture(name: &str) -> PathBuf {
    let path = scratch(name);
    std::fs::write(&path, nested_page_tree_pdf()).expect("should write the fixture");
    path
}

/// The page sizes a document reports, rounded, for naming a geometry change directly.
///
/// `pixel_diff` refuses two rasterizations of different sizes, so without this a page
/// that came back the wrong size would fail as "same dimensions" from inside a helper,
/// which says nothing about what actually went wrong.
fn sizes(path: &Path) -> Vec<(i32, i32)> {
    Document::open(path)
        .expect("should open")
        .geometry()
        .iter()
        .map(|page| (page.width_pt.round() as i32, page.height_pt.round() as i32))
        .collect()
}

#[test]
fn the_fixture_pages_are_distinguishable() {
    // Everything below rests on this. If the fixture's pages looked the same, every
    // reorder assertion would pass regardless of what was written.
    let source = fixture("reorder-distinct.pdf");
    let first = render(&source, 0);
    for page in 1..PAGES {
        assert_different(&first, &render(&source, page), &format!("page 0 vs {page}"));
    }
}

#[test]
fn reversing_a_document_puts_the_last_page_first() {
    let source = fixture("reorder-reverse.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();

    let mut order = PageOrder::identity(PAGES);
    // Repeatedly move the last page to the front: [0,1,2,3] -> [3,2,1,0].
    for step in 0..PAGES {
        order.move_page(PAGES - 1, step);
    }
    assert_eq!(order.as_slice(), &[3, 2, 1, 0]);

    let saved = scratch("reorder-reverse-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("the saved file should open");
    assert_eq!(document.page_count(), PAGES, "lost or gained pages");

    for position in 0..PAGES {
        let expected = &before[PAGES - 1 - position];
        assert_same(
            &render(&saved, position),
            expected,
            &format!(
                "saved position {position} should be original page {}",
                PAGES - 1 - position
            ),
        );
    }
}

#[test]
fn moving_one_page_leaves_the_others_alone() {
    let source = fixture("reorder-one.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();

    // [0,1,2,3] -> [1,2,0,3]: page 0 moves to third place.
    let mut order = PageOrder::identity(PAGES);
    assert!(order.move_page(0, 2));

    let saved = scratch("reorder-one-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    for (position, source_page) in [(0, 1), (1, 2), (2, 0), (3, 3)] {
        assert_same(
            &render(&saved, position),
            &before[source_page],
            &format!("position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn a_deleted_page_is_gone_and_the_rest_close_up() {
    let source = fixture("reorder-delete.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();

    let mut order = PageOrder::identity(PAGES);
    assert!(order.remove(1));

    let saved = scratch("reorder-delete-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), PAGES - 1);

    for (position, source_page) in [(0, 0), (1, 2), (2, 3)] {
        assert_same(
            &render(&saved, position),
            &before[source_page],
            &format!("position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn saving_over_the_original_replaces_it() {
    // What "Save" does. Checked separately from Save As because overwriting the file
    // being read from is the case most likely to go wrong.
    let source = fixture("reorder-in-place.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();

    let mut order = PageOrder::identity(PAGES);
    assert!(order.move_page(0, 3));

    save_reordered(&source, &order, &source, Overwrite::Allow).expect("should save in place");

    let document = Document::open(&source).expect("the original should still open");
    assert_eq!(document.page_count(), PAGES);
    for (position, source_page) in [(0, 1), (1, 2), (2, 3), (3, 0)] {
        assert_same(
            &render(&source, position),
            &before[source_page],
            &format!("position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn save_as_refuses_to_replace_an_existing_file() {
    // Overwriting is what Save is for. A Save As that silently replaces something is
    // how people lose a file they were not thinking about.
    let source = fixture("reorder-refuse.pdf");
    let occupied = scratch("reorder-occupied.pdf");
    std::fs::write(&occupied, b"not to be clobbered").expect("should write");

    let order = PageOrder::identity(PAGES);
    let error =
        save_reordered(&source, &order, &occupied, Overwrite::Refuse).expect_err("should refuse");
    assert!(
        matches!(error, SaveError::WouldOverwrite { .. }),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        std::fs::read(&occupied).expect("should read"),
        b"not to be clobbered",
        "refused and still wrote"
    );
}

#[test]
fn a_page_count_the_writer_disagrees_with_is_refused() {
    // The guard against the two parsers seeing different documents. Simulated by
    // handing the writer an order that claims more source pages than exist, which is
    // exactly the shape of the disagreement that would scramble a real document.
    let source = fixture("reorder-mismatch.pdf");
    let order = PageOrder::identity(PAGES + 3);
    let saved = scratch("reorder-mismatch-out.pdf");

    let error =
        save_reordered(&source, &order, &saved, Overwrite::Refuse).expect_err("should refuse");
    assert!(
        matches!(error, SaveError::PageCountMismatch { .. }),
        "unexpected error: {error:?}"
    );
    assert!(!saved.exists(), "refused and still created a file");
}

#[test]
fn a_refused_save_leaves_no_partial_file_behind() {
    // The atomic write puts a temporary file beside the destination. A failure must not
    // leave it there for somebody to find and mistake for their document.
    let source = fixture("reorder-partial.pdf");
    let saved = scratch("reorder-partial-out.pdf");
    let order = PageOrder::identity(PAGES + 1);

    let _ = save_reordered(&source, &order, &saved, Overwrite::Refuse);

    let mut partial = saved.as_os_str().to_owned();
    partial.push(".porpoise-partial");
    assert!(!Path::new(&partial).exists(), "left a partial file behind");
}

#[test]
fn a_single_page_document_saves_unchanged() {
    // The degenerate case: nothing to reorder, and deleting the only page is refused,
    // so saving must still produce a readable one-page document.
    let path = scratch("reorder-single.pdf");
    std::fs::write(&path, single_page_pdf(200, 100)).expect("should write");
    let before = render(&path, 0);

    let order = PageOrder::identity(1);
    let saved = scratch("reorder-single-out.pdf");
    save_reordered(&path, &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), 1);
    assert_same(&render(&saved, 0), &before, "the only page changed");
}
#[test]
fn the_nested_fixture_really_does_inherit_its_geometry() {
    // Everything below rests on this. The fixture's pages define no `/MediaBox` and no
    // `/Rotate`; both come from the branch above them, and the two branches disagree.
    // If these sizes ever matched, a save that dropped inheritance entirely would still
    // pass every test that follows.
    let source = nested_fixture("nested-inherits.pdf");
    let sizes = sizes(&source);

    assert_eq!(sizes.len(), PAGES);
    assert_eq!(sizes[0], sizes[1], "the first branch should be uniform");
    assert_eq!(sizes[2], sizes[3], "the second branch should be uniform");
    assert_ne!(
        sizes[0], sizes[2],
        "the branches must disagree or this fixture proves nothing"
    );
}

#[test]
fn a_nested_document_reorders_across_its_branches() {
    // The case that used to be refused outright. Reversing this document moves every
    // page out of the branch it was inheriting from, so any page that did not take its
    // attributes with it comes back the wrong size, the wrong way up, or both.
    let source = nested_fixture("nested-reverse.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();
    let before_sizes = sizes(&source);

    let mut order = PageOrder::identity(PAGES);
    for step in 0..PAGES {
        order.move_page(PAGES - 1, step);
    }
    assert_eq!(order.as_slice(), &[3, 2, 1, 0]);

    let saved = scratch("nested-reverse-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("the saved file should open");
    assert_eq!(document.page_count(), PAGES, "lost or gained pages");

    // Sizes first, so a dropped `/MediaBox` is reported as a wrong size rather than as
    // a dimension mismatch from inside `pixel_diff`.
    let expected_sizes: Vec<(i32, i32)> = (0..PAGES).map(|p| before_sizes[PAGES - 1 - p]).collect();
    assert_eq!(sizes(&saved), expected_sizes, "page geometry changed");

    for position in 0..PAGES {
        let source_page = PAGES - 1 - position;
        assert_same(
            &render(&saved, position),
            &before[source_page],
            &format!("saved position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn a_page_moved_between_branches_keeps_its_own_geometry() {
    // The narrowest version of the bug: one page crosses from the second branch to the
    // first. If it picked up its new neighbours' attributes, it renders at their size.
    let source = nested_fixture("nested-cross.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();
    let before_sizes = sizes(&source);

    // [0,1,2,3] -> [2,0,1,3]: page 2 leaves the second branch for the front.
    let mut order = PageOrder::identity(PAGES);
    assert!(order.move_page(2, 0));
    assert_eq!(order.as_slice(), &[2, 0, 1, 3]);

    let saved = scratch("nested-cross-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    assert_eq!(
        sizes(&saved)[0],
        before_sizes[2],
        "the moved page took its new neighbours' size"
    );
    for (position, source_page) in [(0, 2), (1, 0), (2, 1), (3, 3)] {
        assert_same(
            &render(&saved, position),
            &before[source_page],
            &format!("position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn deleting_from_a_nested_document_leaves_the_rest_intact() {
    // Deleting empties one branch completely, which is the case where a flatten is most
    // likely to take a still-needed attribute with it.
    let source = nested_fixture("nested-delete.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();
    let before_sizes = sizes(&source);

    let mut order = PageOrder::identity(PAGES);
    assert!(order.remove(0));
    assert!(order.remove(0));
    assert_eq!(order.as_slice(), &[2, 3], "the first branch should be gone");

    let saved = scratch("nested-delete-out.pdf");
    save_reordered(&source, &order, &saved, Overwrite::Refuse).expect("should save");

    let document = Document::open(&saved).expect("should open");
    assert_eq!(document.page_count(), 2);
    assert_eq!(sizes(&saved), vec![before_sizes[2], before_sizes[3]]);

    for (position, source_page) in [(0, 2), (1, 3)] {
        assert_same(
            &render(&saved, position),
            &before[source_page],
            &format!("position {position} should be original page {source_page}"),
        );
    }
}

#[test]
fn a_nested_document_saved_twice_is_stable() {
    // The second save reads a document this code produced, so this catches a flatten
    // that is correct once but not idempotent.
    let source = nested_fixture("nested-twice.pdf");
    let before: Vec<RenderedPage> = (0..PAGES).map(|page| render(&source, page)).collect();

    let once = scratch("nested-twice-a.pdf");
    save_reordered(
        &source,
        &PageOrder::identity(PAGES),
        &once,
        Overwrite::Refuse,
    )
    .expect("should save");

    let twice = scratch("nested-twice-b.pdf");
    save_reordered(
        &once,
        &PageOrder::identity(PAGES),
        &twice,
        Overwrite::Refuse,
    )
    .expect("should save the saved document");

    assert_eq!(sizes(&twice), sizes(&source), "geometry drifted on re-save");
    for (position, expected) in before.iter().enumerate() {
        assert_same(
            &render(&twice, position),
            expected,
            &format!("page {position} changed after a second save"),
        );
    }
}
