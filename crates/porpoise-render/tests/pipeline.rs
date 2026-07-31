//! End-to-end coverage of the open, rasterize, and encode path.
//!
//! These are the tests that prove the pure-Rust stack actually works on the
//! target platform, rather than merely resolving. They use a synthesized PDF so
//! CI needs no fixture files.

// Panicking is how a test reports failure. clippy's `allow-expect-in-tests`
// exempts `#[test]` functions and `#[cfg(test)]` modules, but not plain helpers
// in an integration-test crate, so opt in for the whole file.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use porpoise_doc::Document;
use porpoise_render::{
    BACKEND_MAX_DIMENSION, Background, EncodePngError, HayroRenderer, RenderError, RenderLimits,
    RenderRequest, RenderedPage, Renderer, render_with_timeout,
};
use porpoise_testkit::{minimal_pdf, pixel_diff, single_page_pdf};

/// The synthesized fixture is 200x100 pt with a blue rectangle inset 20 pt.
fn open_minimal() -> Document {
    Document::from_bytes(minimal_pdf()).expect("synthesized PDF should parse")
}

fn request(page_index: usize, scale: f32) -> RenderRequest {
    RenderRequest { page_index, scale }
}

/// Counts pixels that are recognisably the fixture's blue rectangle.
fn count_blue(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4)
        .filter(|pixel| matches!(pixel, [r, g, b, _] if *b > 200 && *r < 80 && *g < 80))
        .count()
}

// --- Parsing -----------------------------------------------------------------

#[test]
fn synthesized_pdf_parses_with_expected_geometry() {
    let document = open_minimal();
    assert_eq!(document.page_count(), 1);

    let page = document.geometry().first().copied().expect("one page");
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

// --- Rasterizing -------------------------------------------------------------

#[test]
fn renders_at_one_to_one_scale() {
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 1.0))
        .expect("minimal PDF should rasterize");

    assert_eq!((page.width, page.height), (200, 100));
    assert_eq!(page.rgba.len(), 200 * 100 * 4);
}

#[test]
fn scale_multiplies_the_output_resolution() {
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 2.0))
        .expect("should rasterize at 2x");

    assert_eq!((page.width, page.height), (400, 200));
    assert_eq!(page.rgba.len(), 400 * 200 * 4);
}

#[test]
fn rendered_page_contains_the_drawn_rectangle() {
    // Guards against a silent all-white or all-transparent rasterization, which
    // would let every dimension assertion above pass while rendering nothing.
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 1.0))
        .expect("should rasterize");

    // The rectangle is inset 20 pt on each side of a 200x100 page: 160x60 = 9600.
    let blue = count_blue(&page.rgba);
    assert!(
        blue > 8000,
        "expected roughly 9600 blue pixels, found {blue}"
    );
}

#[test]
fn out_of_range_page_is_rejected() {
    let error = HayroRenderer::new()
        .render(&open_minimal(), request(7, 1.0))
        .expect_err("page 7 does not exist");

    assert!(
        matches!(error, RenderError::NoSuchPage { index: 7, count: 1 }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn degenerate_scales_are_rejected_before_rasterizing() {
    let renderer = HayroRenderer::new();
    let document = open_minimal();
    for scale in [0.0, -1.0, f32::NAN] {
        let error = renderer
            .render(&document, request(0, scale))
            .expect_err("a degenerate scale must be refused");
        assert!(
            matches!(error, RenderError::UnusableSize { .. }),
            "scale {scale} gave unexpected error: {error:?}"
        );
    }
}

// --- Limits ------------------------------------------------------------------

#[test]
fn default_limits_reject_the_case_both_dimension_checks_would_pass() {
    // This is the hole the area cap exists to close. A 200x100 pt page at 300x
    // scale is 60000x30000 px. Both axes are under hayro's u16 viewport limit, so
    // per-axis checks alone would wave it through — while asking for 1.8 billion
    // pixels, about 7.2 GB of RGBA.
    let error = HayroRenderer::new()
        .render(&open_minimal(), request(0, 300.0))
        .expect_err("1.8 billion pixels must be refused");

    match error {
        RenderError::AreaTooLarge {
            width,
            height,
            total_pixels,
            ..
        } => {
            assert_eq!((width, height), (60_000, 30_000));
            assert_eq!(total_pixels, 1_800_000_000);
            assert!(
                width < BACKEND_MAX_DIMENSION && height < BACKEND_MAX_DIMENSION,
                "the point of this test is that both axes are individually legal"
            );
        }
        other => panic!("expected AreaTooLarge, got {other:?}"),
    }
}

#[test]
fn area_cap_is_what_rejects_and_not_the_size_itself() {
    // The same request must succeed under default limits and fail under a tight
    // area cap. Otherwise this proves nothing about the cap.
    let document = Document::from_bytes(single_page_pdf(1000, 1000)).expect("should parse");
    let request = request(0, 2.0); // 2000x2000 = 4 Mpx

    HayroRenderer::new()
        .render(&document, request)
        .expect("4 Mpx is fine under the 64 Mpx default");

    let tight = HayroRenderer::new().with_limits(RenderLimits {
        max_total_pixels: 1 << 20, // 1 Mpx
        ..RenderLimits::default()
    });
    let error = tight
        .render(&document, request)
        .expect_err("4 Mpx must exceed a 1 Mpx cap");

    assert!(
        matches!(
            error,
            RenderError::AreaTooLarge {
                total_pixels: 4_000_000,
                ..
            }
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn per_axis_cap_still_applies() {
    let renderer = HayroRenderer::new().with_limits(RenderLimits {
        max_pixel_dimension: 100,
        max_total_pixels: u64::MAX,
    });
    // The fixture is 200 pt wide, so 1:1 already exceeds a 100 px axis cap.
    let error = renderer
        .render(&open_minimal(), request(0, 1.0))
        .expect_err("200 px must exceed a 100 px cap");

    assert!(
        matches!(error, RenderError::DimensionTooLarge { max: 100, .. }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn absurd_scales_do_not_wrap_around_when_cast() {
    // `as u32` saturates rather than wrapping, so an enormous float could
    // silently become u32::MAX and read as merely large. Check it is refused.
    let renderer = HayroRenderer::new();
    for scale in [1.0e6, 1.0e30, f32::INFINITY] {
        let error = renderer
            .render(&open_minimal(), request(0, scale))
            .expect_err("absurd scale must be refused");
        assert!(
            matches!(
                error,
                RenderError::DimensionTooLarge { .. } | RenderError::UnusableSize { .. }
            ),
            "scale {scale} gave unexpected error: {error:?}"
        );
    }
}

#[test]
fn effective_max_dimension_never_exceeds_the_backend_limit() {
    let limits = RenderLimits {
        max_pixel_dimension: u32::MAX,
        max_total_pixels: u64::MAX,
    };
    assert_eq!(limits.effective_max_dimension(), BACKEND_MAX_DIMENSION);
}

// --- Page background ---------------------------------------------------------

/// The top-left pixel of the fixture, which is outside the inset rectangle and so
/// shows the page background.
fn corner_pixel(page: &RenderedPage) -> [u8; 4] {
    let mut pixel = [0_u8; 4];
    pixel.copy_from_slice(page.rgba.get(..4).unwrap_or(&[0, 0, 0, 0]));
    pixel
}

#[test]
fn pages_render_on_opaque_white_by_default() {
    // hayro's own default is transparent, which makes a text document unreadable
    // against a dark UI and makes a PNG of one look blank. Paper is white.
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 1.0))
        .expect("should rasterize");

    assert_eq!(corner_pixel(&page), [255, 255, 255, 255]);
}

#[test]
fn transparent_background_is_available_when_asked_for() {
    let page = HayroRenderer::new()
        .with_background(Background::Transparent)
        .render(&open_minimal(), request(0, 1.0))
        .expect("should rasterize");

    let corner = corner_pixel(&page);
    assert_eq!(
        corner[3], 0,
        "expected a transparent corner, got {corner:?}"
    );
}

#[test]
fn the_background_does_not_paint_over_content() {
    // A white background must sit behind the drawing, not on top of it.
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 1.0))
        .expect("should rasterize");

    let blue = count_blue(&page.rgba);
    assert!(
        blue > 8000,
        "white background hid the rectangle; only {blue} blue pixels left"
    );
}

// --- Timeout -----------------------------------------------------------------

/// A renderer that takes a known amount of time, so the timeout path can be
/// tested deterministically instead of racing a real rasterization.
///
/// `Clone` is required by [`porpoise_render::RenderPool`], which hands each worker
/// its own renderer.
#[derive(Clone)]
struct SleepyRenderer {
    delay: Duration,
}

impl Renderer for SleepyRenderer {
    fn render(
        &self,
        _document: &Document,
        _request: RenderRequest,
    ) -> Result<RenderedPage, RenderError> {
        std::thread::sleep(self.delay);
        Ok(RenderedPage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        })
    }
}

#[test]
fn a_render_that_overruns_its_budget_reports_a_timeout() {
    let document = Arc::new(open_minimal());
    let error = render_with_timeout(
        SleepyRenderer {
            delay: Duration::from_secs(30),
        },
        document,
        request(0, 1.0),
        Duration::from_millis(50),
    )
    .expect_err("a 30 s render under a 50 ms budget must time out");

    assert!(
        matches!(
            error,
            RenderError::TimedOut {
                index: 0,
                timeout_ms: 50
            }
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn a_render_that_finishes_in_time_returns_its_page() {
    let document = Arc::new(open_minimal());
    let page = render_with_timeout(
        SleepyRenderer {
            delay: Duration::from_millis(1),
        },
        document,
        request(0, 1.0),
        Duration::from_secs(30),
    )
    .expect("a 1 ms render under a 30 s budget must succeed");

    assert_eq!((page.width, page.height), (1, 1));
}

#[test]
fn the_timeout_path_produces_the_same_pixels_as_the_direct_path() {
    let document = Arc::new(open_minimal());
    let direct = HayroRenderer::new()
        .render(&document, request(0, 1.0))
        .expect("direct render");
    let threaded = render_with_timeout(
        HayroRenderer::new(),
        Arc::clone(&document),
        request(0, 1.0),
        Duration::from_secs(30),
    )
    .expect("threaded render");

    let diff = pixel_diff(&direct, &threaded, 0).expect("same dimensions");
    assert!(diff.is_clean(), "threading changed the output: {diff:?}");
}

// --- Worker pool -------------------------------------------------------------

/// Collects outcomes until `count` arrive or the deadline passes.
fn drain_outcomes(
    pool: &porpoise_render::RenderPool,
    count: usize,
    within: Duration,
) -> Vec<porpoise_render::RenderOutcome> {
    let deadline = std::time::Instant::now() + within;
    let mut collected = Vec::new();
    while collected.len() < count && std::time::Instant::now() < deadline {
        if let Some(outcome) = pool.try_recv() {
            collected.push(outcome);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    collected
}

#[test]
fn the_pool_rasterizes_every_submitted_page() {
    let document = Arc::new(open_minimal());
    let pool = porpoise_render::RenderPool::new(
        document,
        HayroRenderer::new(),
        2,
        Duration::from_secs(30),
    );

    for tag in 0..4 {
        assert!(pool.submit(0, 0, 1.0, tag), "submit should be accepted");
    }

    let outcomes = drain_outcomes(&pool, 4, Duration::from_secs(30));
    assert_eq!(outcomes.len(), 4, "expected four results");
    for outcome in &outcomes {
        assert_eq!(outcome.page_index, 0);
        let page = outcome.result.as_ref().expect("should rasterize");
        assert_eq!((page.width, page.height), (200, 100));
    }

    // Tags come back untouched, which is what lets the viewer match a result to a
    // cache key without the pool knowing about zoom.
    let mut tags: Vec<i64> = outcomes.iter().map(|outcome| outcome.tag).collect();
    tags.sort_unstable();
    assert_eq!(tags, vec![0, 1, 2, 3]);
}

#[test]
fn the_pool_rasterizes_from_more_than_one_document() {
    // What a merge needs: pages queued against different documents must come back
    // rasterized from the *right* one, not conflated because they share a page
    // index or a tag.
    let first = Arc::new(open_minimal());
    let second = Arc::new(Document::from_bytes(single_page_pdf(300, 150)).expect("should parse"));

    let pool = porpoise_render::RenderPool::new(
        Arc::clone(&first),
        HayroRenderer::new(),
        2,
        Duration::from_secs(30),
    );
    let second_index = pool.add_document(Arc::clone(&second));
    assert_eq!(
        second_index, 1,
        "the second document should land at index 1"
    );

    assert!(pool.submit(0, 0, 1.0, 10));
    assert!(pool.submit(second_index, 0, 1.0, 20));

    let outcomes = drain_outcomes(&pool, 2, Duration::from_secs(30));
    assert_eq!(outcomes.len(), 2);

    for outcome in &outcomes {
        let page = outcome.result.as_ref().expect("should rasterize");
        match outcome.document {
            0 => assert_eq!(
                (page.width, page.height),
                (200, 100),
                "wrong page for document 0"
            ),
            1 => assert_eq!(
                (page.width, page.height),
                (300, 150),
                "wrong page for document 1"
            ),
            other => panic!("unexpected document index {other}"),
        }
    }
}

#[test]
fn submitting_against_an_unregistered_document_is_refused_rather_than_queued() {
    let document = Arc::new(open_minimal());
    let pool = porpoise_render::RenderPool::new(
        document,
        HayroRenderer::new(),
        1,
        Duration::from_secs(30),
    );

    assert!(
        !pool.submit(7, 0, 1.0, 0),
        "accepted a job for a document that was never added"
    );
    assert_eq!(
        pool.queued(),
        0,
        "a refused submission must not sit in the queue"
    );
}

#[test]
fn the_pool_reports_render_errors_rather_than_dropping_them() {
    let document = Arc::new(open_minimal());
    let pool = porpoise_render::RenderPool::new(
        document,
        HayroRenderer::new(),
        1,
        Duration::from_secs(30),
    );

    pool.submit(0, 99, 1.0, 0);

    let outcomes = drain_outcomes(&pool, 1, Duration::from_secs(30));
    let outcome = outcomes.first().expect("an outcome for a bad page");
    assert!(
        matches!(
            outcome.result,
            Err(RenderError::NoSuchPage { index: 99, .. })
        ),
        "unexpected result: {:?}",
        outcome.result
    );
}

#[test]
fn cancel_pending_drops_queued_work() {
    let document = Arc::new(open_minimal());
    // One worker and a slow renderer, so jobs pile up in the queue rather than
    // being served immediately.
    let pool = porpoise_render::RenderPool::new(
        document,
        SleepyRenderer {
            delay: Duration::from_millis(400),
        },
        1,
        Duration::from_secs(30),
    );

    for tag in 0..8 {
        pool.submit(0, 0, 1.0, tag);
    }
    let dropped = pool.cancel_pending();

    assert!(
        dropped >= 6,
        "expected most of 8 jobs to still be queued, dropped {dropped}"
    );
    assert_eq!(pool.queued(), 0, "queue should be empty after cancelling");
}

#[test]
fn a_hung_render_does_not_permanently_consume_a_worker() {
    // The pool would otherwise starve: enough hangs and no page ever renders
    // again. Workers go through render_with_timeout precisely so the worker
    // survives while the hung thread is abandoned.
    let document = Arc::new(open_minimal());
    let pool = porpoise_render::RenderPool::new(
        Arc::clone(&document),
        SleepyRenderer {
            delay: Duration::from_secs(120),
        },
        1,
        Duration::from_millis(50),
    );

    // Three jobs that all hang. With a healthy worker each times out in turn.
    for tag in 0..3 {
        pool.submit(0, 0, 1.0, tag);
    }

    let outcomes = drain_outcomes(&pool, 3, Duration::from_secs(20));
    assert_eq!(
        outcomes.len(),
        3,
        "the worker stopped serving the queue after a hang"
    );
    for outcome in &outcomes {
        assert!(
            matches!(outcome.result, Err(RenderError::TimedOut { .. })),
            "unexpected result: {:?}",
            outcome.result
        );
    }
}

#[test]
fn a_full_queue_is_bounded_rather_than_unbounded() {
    let document = Arc::new(open_minimal());
    let pool = porpoise_render::RenderPool::new(
        document,
        SleepyRenderer {
            delay: Duration::from_secs(30),
        },
        1,
        Duration::from_secs(60),
    );

    // Far more than the internal cap.
    for tag in 0..500 {
        pool.submit(0, 0, 1.0, tag);
    }
    assert!(
        pool.queued() <= 64,
        "queue grew to {} with no bound",
        pool.queued()
    );
}

#[test]
fn recommended_workers_leaves_room_for_the_ui_thread() {
    let workers = porpoise_render::RenderPool::recommended_workers();
    assert!((1..=4).contains(&workers), "got {workers}");
}

// --- PNG ---------------------------------------------------------------------

#[test]
fn encoded_png_decodes_back_to_the_same_image() {
    let page = HayroRenderer::new()
        .render(&open_minimal(), request(0, 1.0))
        .expect("should rasterize");
    let encoded = page.encode_png().expect("should encode");

    assert_eq!(
        encoded.get(..8),
        Some(b"\x89PNG\r\n\x1a\n".as_slice()),
        "missing PNG magic bytes"
    );

    // png 0.18 requires `BufRead + Seek`, and `&[u8]` is not `Seek`.
    let decoder = png::Decoder::new(std::io::Cursor::new(encoded.as_slice()));
    let mut reader = decoder.read_info().expect("should read header");
    assert_eq!(reader.info().width, 200);
    assert_eq!(reader.info().height, 100);
    assert_eq!(reader.info().color_type, png::ColorType::Rgba);

    let mut buffer = vec![0_u8; reader.output_buffer_size().expect("known buffer size")];
    let frame = reader.next_frame(&mut buffer).expect("should decode");
    assert_eq!(frame.buffer_size(), 200 * 100 * 4);

    // The round trip must preserve the drawing, not just the dimensions.
    let blue = count_blue(&buffer);
    assert!(
        blue > 8000,
        "expected roughly 9600 blue pixels after round trip, found {blue}"
    );
}

#[test]
fn encode_png_rejects_a_buffer_that_contradicts_its_dimensions() {
    let malformed = RenderedPage {
        width: 10,
        height: 10,
        rgba: vec![0; 39], // one byte short of 10 * 10 * 4
    };
    let error = malformed
        .encode_png()
        .expect_err("a short buffer must not encode");

    assert!(
        matches!(error, EncodePngError::Malformed { len: 39, .. }),
        "unexpected error: {error:?}"
    );
}

// --- Diffing -----------------------------------------------------------------

#[test]
fn identical_rasterizations_diff_clean() {
    let document = open_minimal();
    let renderer = HayroRenderer::new();
    let left = renderer.render(&document, request(0, 1.0)).expect("render");
    let right = renderer.render(&document, request(0, 1.0)).expect("render");

    let diff = pixel_diff(&left, &right, 0).expect("same dimensions");
    assert!(diff.is_clean(), "rendering is not deterministic: {diff:?}");
    assert_eq!(diff.total_pixels, 200 * 100);
    assert_eq!(diff.max_channel_delta, 0);
}

#[test]
fn differently_sized_rasterizations_cannot_be_diffed() {
    let document = open_minimal();
    let renderer = HayroRenderer::new();
    let small = renderer.render(&document, request(0, 1.0)).expect("render");
    let large = renderer.render(&document, request(0, 2.0)).expect("render");

    assert!(pixel_diff(&small, &large, 0).is_err());
}

#[test]
fn tolerance_masks_differences_at_or_below_it_and_no_further() {
    // The tolerance parameter exists so two rasterizations can be compared
    // without antialiased edges counting as disagreement. Exercised here because
    // every other caller passes zero, which would leave the branch untested.
    let flat = |value: u8| RenderedPage {
        width: 2,
        height: 1,
        rgba: vec![value, value, value, 255, value, value, value, 255],
    };
    let left = flat(100);
    let right = flat(103); // three levels apart on every colour channel

    let exact = pixel_diff(&left, &right, 0).expect("same dimensions");
    assert_eq!(exact.differing_pixels, 2);
    assert_eq!(exact.max_channel_delta, 3);

    // A tolerance below the delta still reports it.
    let strict = pixel_diff(&left, &right, 2).expect("same dimensions");
    assert_eq!(
        strict.differing_pixels, 2,
        "a delta of 3 survives a tolerance of 2"
    );

    // At the delta, the difference is absorbed — but `max_channel_delta` keeps
    // reporting the real distance, which is what makes a clean diff meaningful.
    let forgiving = pixel_diff(&left, &right, 3).expect("same dimensions");
    assert!(
        forgiving.is_clean(),
        "a delta of 3 should pass a tolerance of 3"
    );
    assert_eq!(forgiving.max_channel_delta, 3);
    assert_eq!(forgiving.total_pixels, 2);
}
