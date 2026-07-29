//! Choosing a zoom factor so a page fits its viewport.
//!
//! Split from the layout it serves because these are two different questions:
//! [`crate::ScrollLayout`] answers *where does each page sit*, and this answers
//! *how big should everything be*. The only interesting content here is the
//! degenerate cases — a page or viewport of zero, `NaN`, or a negative size — all
//! of which have to produce a usable zoom rather than a division by zero.

use porpoise_doc::PageGeometry;

/// How a page should be sized relative to the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FitMode {
    /// Scale so the page's width exactly fills the viewport width.
    Width,
    /// Scale so the entire page is visible, letterboxing the shorter axis.
    Page,
    /// An explicit zoom factor, ignoring the viewport.
    Fixed(f32),
}

/// Smallest zoom [`fit_scale`] will return.
pub const MIN_SCALE: f32 = 0.01;

/// Largest zoom [`fit_scale`] will return.
///
/// This is a usability bound, not a safety one — `porpoise-render` enforces the
/// limits that actually protect memory.
pub const MAX_SCALE: f32 = 64.0;

/// The zoom factor that satisfies `mode` for one page in a given viewport.
///
/// Viewport dimensions are in points, so a caller working in physical pixels
/// should divide by its device pixel ratio first. The result is always finite and
/// within [`MIN_SCALE`]..=[`MAX_SCALE`], including when the page or viewport is
/// degenerate — a malformed document should letterbox oddly, not crash the view.
#[must_use]
pub fn fit_scale(
    mode: FitMode,
    page: PageGeometry,
    viewport_width_pt: f32,
    viewport_height_pt: f32,
) -> f32 {
    let usable = |value: f32| value.is_finite() && value > 0.0;

    let scale = match mode {
        FitMode::Fixed(scale) => scale,
        FitMode::Width => {
            if usable(viewport_width_pt) && usable(page.width_pt) {
                viewport_width_pt / page.width_pt
            } else {
                1.0
            }
        }
        FitMode::Page => {
            let fits_width = usable(viewport_width_pt) && usable(page.width_pt);
            let fits_height = usable(viewport_height_pt) && usable(page.height_pt);
            match (fits_width, fits_height) {
                // The whole page is visible only if both axes fit, so take the
                // more restrictive of the two.
                (true, true) => {
                    (viewport_width_pt / page.width_pt).min(viewport_height_pt / page.height_pt)
                }
                (true, false) => viewport_width_pt / page.width_pt,
                (false, true) => viewport_height_pt / page.height_pt,
                (false, false) => 1.0,
            }
        }
    };

    if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(width_pt: f32, height_pt: f32) -> PageGeometry {
        PageGeometry {
            width_pt,
            height_pt,
        }
    }

    /// US Letter at 72 DPI.
    fn letter() -> PageGeometry {
        page(612.0, 792.0)
    }

    #[test]
    fn fit_width_scales_the_page_to_the_viewport_width() {
        // A 612 pt page in a 1224 pt viewport is exactly 2x.
        assert_eq!(fit_scale(FitMode::Width, letter(), 1224.0, 400.0), 2.0);
        assert_eq!(fit_scale(FitMode::Width, letter(), 306.0, 400.0), 0.5);
    }

    #[test]
    fn fit_width_ignores_the_viewport_height() {
        // Fit-to-width deliberately overflows vertically; that is what scrolling
        // is for.
        let wide = fit_scale(FitMode::Width, letter(), 1224.0, 10.0);
        let tall = fit_scale(FitMode::Width, letter(), 1224.0, 10_000.0);
        assert_eq!(wide, tall);
    }

    #[test]
    fn fit_page_takes_the_more_restrictive_axis() {
        // Width alone would allow 2x, but the height only allows 1x, so the whole
        // page fits only at 1x.
        let scale = fit_scale(FitMode::Page, letter(), 1224.0, 792.0);
        assert_eq!(scale, 1.0);

        // And the other way round.
        let scale = fit_scale(FitMode::Page, letter(), 612.0, 1584.0);
        assert_eq!(scale, 1.0);
    }

    #[test]
    fn fit_page_on_a_landscape_page_in_a_portrait_viewport() {
        let landscape = page(792.0, 612.0);
        // 396/792 = 0.5 by width, 612/612 = 1.0 by height. Width wins.
        assert_eq!(fit_scale(FitMode::Page, landscape, 396.0, 612.0), 0.5);
    }

    #[test]
    fn fixed_mode_passes_the_scale_through_but_still_clamps() {
        assert_eq!(fit_scale(FitMode::Fixed(3.0), letter(), 100.0, 100.0), 3.0);
        assert_eq!(
            fit_scale(FitMode::Fixed(1000.0), letter(), 100.0, 100.0),
            MAX_SCALE
        );
        assert_eq!(
            fit_scale(FitMode::Fixed(0.0), letter(), 100.0, 100.0),
            MIN_SCALE
        );
    }

    #[test]
    fn degenerate_page_or_viewport_falls_back_rather_than_dividing_by_zero() {
        let zero = page(0.0, 0.0);
        let nan = page(f32::NAN, f32::NAN);

        for mode in [FitMode::Width, FitMode::Page] {
            for geometry in [zero, nan] {
                let scale = fit_scale(mode, geometry, 1000.0, 1000.0);
                assert!(
                    scale.is_finite() && scale > 0.0,
                    "{mode:?} on {geometry:?} gave {scale}"
                );
            }
            // And a degenerate viewport against a good page.
            for (width, height) in [(0.0, 0.0), (f32::NAN, f32::NAN), (-5.0, -5.0)] {
                let scale = fit_scale(mode, letter(), width, height);
                assert!(
                    scale.is_finite() && scale > 0.0,
                    "{mode:?} in {width}x{height} gave {scale}"
                );
            }
        }
    }

    #[test]
    fn fit_scale_is_always_within_the_clamp_range() {
        // A tiny page in a huge viewport would otherwise ask for an enormous zoom.
        let scale = fit_scale(FitMode::Width, page(1.0, 1.0), 100_000.0, 100_000.0);
        assert_eq!(scale, MAX_SCALE);

        let scale = fit_scale(FitMode::Width, page(100_000.0, 100_000.0), 1.0, 1.0);
        assert_eq!(scale, MIN_SCALE);
    }
}
