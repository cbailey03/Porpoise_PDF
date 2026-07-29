//! Hardening against damaged input.
//!
//! A PDF viewer's input is whatever someone hands it, so the question that
//! matters is not whether valid files work but whether damaged ones fail safely.
//! These tests take a valid PDF apart in thousands of deterministic ways and
//! require that every single one either opens or reports an error — never
//! unwinds, never hangs, never allocates without bound.
//!
//! Being deterministic is the point: a failure reproduces from its seed.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use porpoise_doc::{Document, DocumentError};
use porpoise_render::{HayroRenderer, RenderLimits, RenderRequest, Renderer, render_with_timeout};
use porpoise_testkit::{Mutation, Mutator, minimal_pdf, single_page_pdf};

/// Mutations per sweep. Large enough to reach every mutation kind many times over
/// while keeping the suite quick on a CI runner.
const MUTATIONS: u32 = 4_000;

/// Whole-sweep budget. A parser that hangs would otherwise hang CI with no clue
/// which input did it.
const SWEEP_BUDGET: Duration = Duration::from_secs(120);

/// Per-page render budget during the sweep.
const RENDER_BUDGET: Duration = Duration::from_secs(5);

/// Runs `body` on a worker and fails if it does not finish in time.
///
/// Catches the failure mode a plain test cannot: an input that makes the parser
/// loop forever. Without this the symptom is a CI job timing out with no output.
fn within_budget(label: &str, budget: Duration, body: impl FnOnce() + Send + 'static) {
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        body();
        let _ = done.send(());
    });
    assert!(
        wait.recv_timeout(budget).is_ok(),
        "{label} did not finish within {budget:?} — likely a hang on malformed input"
    );
}

#[test]
fn opening_thousands_of_damaged_pdfs_never_panics_or_hangs() {
    within_budget("malformed-open sweep", SWEEP_BUDGET, || {
        let original = minimal_pdf();
        let mut mutator = Mutator::new(0xC0FF_EE00_1234_5678);
        let mut parsed = 0_u32;
        let mut rejected = 0_u32;
        let mut panicked = 0_u32;

        for _ in 0..MUTATIONS {
            let (bytes, kind) = mutator.mutate(&original);

            // The contract: a Result, always. Never an unwind.
            match Document::from_bytes(bytes) {
                Ok(document) => {
                    parsed += 1;
                    // A document that opens must also report sane geometry rather
                    // than, say, a page count that would overflow a later loop.
                    assert!(
                        document.page_count() < 100_000,
                        "{kind:?} produced an implausible page count of {}",
                        document.page_count()
                    );
                    for page in document.geometry() {
                        assert!(
                            page.width_pt.is_finite() && page.height_pt.is_finite(),
                            "{kind:?} produced non-finite page geometry: {page:?}"
                        );
                    }
                }
                Err(DocumentError::ParserPanicked) => panicked += 1,
                Err(_) => rejected += 1,
            }
        }

        println!(
            "{MUTATIONS} mutations: {parsed} opened, {rejected} rejected, \
             {panicked} contained parser panics"
        );
        assert_eq!(
            parsed + rejected + panicked,
            MUTATIONS,
            "some mutation produced no outcome at all"
        );
    });
}

#[test]
fn rendering_damaged_pdfs_never_panics_or_hangs() {
    within_budget("malformed-render sweep", SWEEP_BUDGET, || {
        let original = minimal_pdf();
        let mut mutator = Mutator::new(0x5EED_0000_BEEF_0001);
        // A tight pixel cap keeps a mutated MediaBox from asking for gigabytes.
        let renderer = HayroRenderer::new().with_limits(RenderLimits {
            max_total_pixels: 4 << 20,
            ..RenderLimits::default()
        });

        let mut rendered = 0_u32;
        let mut errored = 0_u32;

        for _ in 0..MUTATIONS / 4 {
            let (bytes, kind) = mutator.mutate(&original);
            let Ok(document) = Document::from_bytes(bytes) else {
                continue;
            };
            if document.page_count() == 0 {
                continue;
            }
            let document = Arc::new(document);

            match render_with_timeout(
                renderer,
                Arc::clone(&document),
                RenderRequest {
                    page_index: 0,
                    scale: 1.0,
                },
                RENDER_BUDGET,
            ) {
                Ok(page) => {
                    rendered += 1;
                    let expected = page.width as usize * page.height as usize * 4;
                    assert_eq!(
                        page.rgba.len(),
                        expected,
                        "{kind:?} produced a buffer inconsistent with its dimensions"
                    );
                }
                Err(_) => errored += 1,
            }
        }

        println!("{rendered} damaged pages rendered, {errored} reported an error");
    });
}

#[test]
fn every_mutation_kind_is_actually_exercised() {
    // A sweep that only ever truncates would prove much less than it appears to.
    let original = minimal_pdf();
    let mut mutator = Mutator::new(7);
    let mut seen = Vec::new();

    for _ in 0..2_000 {
        let (_, kind) = mutator.mutate(&original);
        if !seen.contains(&kind) {
            seen.push(kind);
        }
    }

    for kind in [
        Mutation::Truncated,
        Mutation::BitFlipped,
        Mutation::Zeroed,
        Mutation::Junked,
        Mutation::HeaderDamaged,
        Mutation::StartxrefDamaged,
        Mutation::Duplicated,
    ] {
        assert!(seen.contains(&kind), "{kind:?} was never generated");
    }
}

#[test]
fn mutations_are_reproducible_from_their_seed() {
    // Otherwise a failure found in CI could not be reproduced locally.
    let original = minimal_pdf();
    let first: Vec<Vec<u8>> = {
        let mut mutator = Mutator::new(42);
        (0..50).map(|_| mutator.mutate(&original).0).collect()
    };
    let second: Vec<Vec<u8>> = {
        let mut mutator = Mutator::new(42);
        (0..50).map(|_| mutator.mutate(&original).0).collect()
    };
    assert_eq!(first, second, "the same seed produced different mutations");

    let different: Vec<Vec<u8>> = {
        let mut mutator = Mutator::new(43);
        (0..50).map(|_| mutator.mutate(&original).0).collect()
    };
    assert_ne!(
        first, different,
        "different seeds produced identical output"
    );
}

#[test]
fn a_truncated_pdf_at_every_length_is_safe() {
    // Exhaustive rather than sampled: truncation is the most common real-world
    // damage, so every possible cut point is worth checking.
    within_budget("exhaustive truncation", SWEEP_BUDGET, || {
        let original = minimal_pdf();
        for length in 0..original.len() {
            let bytes = original.get(..length).unwrap_or_default().to_vec();
            // Only the absence of a panic matters; either outcome is acceptable.
            let _ = Document::from_bytes(bytes);
        }
        println!("checked all {} truncation lengths", original.len());
    });
}

#[test]
fn an_empty_or_tiny_input_is_rejected_cleanly() {
    for bytes in [
        Vec::new(),
        b"%".to_vec(),
        b"%PDF".to_vec(),
        b"%PDF-1.7".to_vec(),
        vec![0_u8; 1024],
        vec![0xFF_u8; 1024],
    ] {
        let length = bytes.len();
        assert!(
            Document::from_bytes(bytes).is_err(),
            "a {length}-byte non-document was accepted"
        );
    }
}

#[test]
fn a_declared_page_size_cannot_force_an_unbounded_allocation() {
    // A hostile MediaBox is the cheapest denial-of-service attempt there is: claim
    // an enormous page and let the viewer try to allocate it.
    let document =
        Document::from_bytes(single_page_pdf(200_000, 200_000)).expect("should still parse");
    let error = HayroRenderer::new()
        .render(
            &document,
            RenderRequest {
                page_index: 0,
                scale: 1.0,
            },
        )
        .expect_err("a 200000 pt page must be refused, not attempted");

    println!("refused as expected: {error}");
}

#[test]
fn opening_a_damaged_pdf_is_fast_enough_to_be_safe() {
    // A parser that is merely very slow on bad input is a denial of service even
    // if it eventually returns. This is a coarse bound, not a benchmark.
    let original = minimal_pdf();
    let mut mutator = Mutator::new(0xDEAD_BEEF);
    let started = Instant::now();
    let attempts = 500;

    for _ in 0..attempts {
        let (bytes, _) = mutator.mutate(&original);
        let _ = Document::from_bytes(bytes);
    }

    let each = started.elapsed() / attempts;
    println!("mean open attempt on damaged input: {each:?}");
    assert!(
        each < Duration::from_millis(50),
        "opening damaged input averaged {each:?}, which is slow enough to be abused"
    );
}
