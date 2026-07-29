//! Quantizing zoom so that cached renders survive small changes.
//!
//! A window drag changes the fit-to-width zoom on every pixel. Rasterizing for
//! each distinct value would throw away every texture continuously, so zoom is
//! snapped onto a ladder and pages are rendered per rung.

use crate::{MAX_SCALE, MIN_SCALE};

/// Rungs per doubling of zoom.
///
/// A geometric ladder rather than a linear one, so the *relative* error is
/// constant: eight rungs per octave is about 9% between neighbours, whether you
/// are at 10% zoom or 800%. A linear ladder would be far too coarse at low zoom
/// and pointlessly fine at high zoom.
const RUNGS_PER_OCTAVE: f32 = 8.0;

/// Slack subtracted before rounding up to a rung.
///
/// Without it, `enclosing` is not idempotent: `log2` of a rung's own scale lands
/// a hair below the integer in floating point, so `ceil` climbs to the next rung,
/// and every layout pass would zoom in a little further. The cost is that a zoom
/// up to 0.1% above a rung snaps down to it, so a texture can be up to 0.1%
/// smaller than its display size — invisible, and far better than drift.
const RUNG_SLACK: f32 = 1.0e-3;

/// A zoom level snapped to the ladder.
///
/// Ordering is by magnitude, so bucket comparisons behave as you would expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZoomBucket(i16);

impl ZoomBucket {
    /// The smallest bucket at or above `zoom`.
    ///
    /// Deliberately rounds *up*: a texture rasterized below its display size
    /// looks blurry, while one slightly above merely costs a few percent more
    /// pixels.
    ///
    /// `NaN` and non-positive input snap to [`MIN_SCALE`]. Note that positive
    /// infinity clamps *up* to [`MAX_SCALE`] — it is an absurd zoom, not a
    /// degenerate one, and treating it as the floor would silently render a
    /// hugely magnified page at minimum resolution.
    #[must_use]
    pub fn enclosing(zoom: f32) -> Self {
        if zoom.is_nan() || zoom <= 0.0 {
            return Self::enclosing(MIN_SCALE);
        }

        // `clamp` maps positive infinity to MAX_SCALE, which is what we want.
        let clamped = zoom.clamp(MIN_SCALE, MAX_SCALE);
        let rung = (clamped.log2() * RUNGS_PER_OCTAVE - RUNG_SLACK).ceil();

        // The ladder is bounded by MIN_SCALE..=MAX_SCALE, so the rung index fits
        // in an i16 with room to spare.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "rung is bounded by log2 of the scale clamp range"
        )]
        Self(rung as i16)
    }

    /// The exact zoom this bucket renders at.
    ///
    /// Always within [`MIN_SCALE`]..=[`MAX_SCALE`], and always at or above the
    /// `zoom` that produced it via [`Self::enclosing`].
    #[must_use]
    pub fn scale(self) -> f32 {
        let scale = (f32::from(self.0) / RUNGS_PER_OCTAVE).exp2();
        scale.clamp(MIN_SCALE, MAX_SCALE)
    }

    /// The ladder index, for logging and cache keys.
    #[must_use]
    pub fn rung(self) -> i16 {
        self.0
    }

    /// Moves `rungs` steps along the ladder, saturating at either end.
    ///
    /// One rung is about 9%, which is a comfortable single press of a zoom key or
    /// one notch of a scroll wheel.
    #[must_use]
    pub fn step(self, rungs: i16) -> Self {
        let floor = Self::enclosing(MIN_SCALE).0;
        let ceiling = Self::enclosing(MAX_SCALE).0;
        Self(self.0.saturating_add(rungs).clamp(floor, ceiling))
    }

    /// Reconstructs a bucket from a [`Self::rung`].
    ///
    /// The render pipeline carries the rung through as an opaque integer tag, so
    /// results can be matched back to a cache key without the renderer knowing
    /// anything about zoom bucketing.
    #[must_use]
    pub fn from_rung(rung: i16) -> Self {
        Self(rung)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_never_renders_meaningfully_below_the_requested_zoom() {
        // Rendering below display size is the one outcome that looks bad, so this
        // is the property that matters most. The tolerance is RUNG_SLACK: a zoom a
        // hair above a rung snaps down to it, by design, to keep `enclosing`
        // idempotent.
        let mut zoom = MIN_SCALE;
        while zoom < MAX_SCALE {
            let bucket = ZoomBucket::enclosing(zoom);
            assert!(
                bucket.scale() >= zoom * (1.0 - 2.0 * RUNG_SLACK),
                "bucket for {zoom} renders at {}, which is below it",
                bucket.scale()
            );
            zoom *= 1.03;
        }
    }

    #[test]
    fn a_bucket_never_overshoots_by_more_than_one_rung() {
        // Otherwise we would waste pixels. One rung is about 9%.
        let ratio = (1.0_f32 / RUNGS_PER_OCTAVE).exp2();
        for zoom in [0.1, 0.5, 0.837, 1.0, 1.5, 3.0, 12.0] {
            let bucket = ZoomBucket::enclosing(zoom);
            assert!(
                bucket.scale() <= zoom * ratio * 1.001,
                "bucket for {zoom} renders at {}, more than one rung above",
                bucket.scale()
            );
        }
    }

    #[test]
    fn zooms_within_one_rung_share_a_bucket() {
        // The whole point: a window drag must not invalidate the cache. Rung -2
        // spans (0.7711, 0.8409], so everything in that band is one render.
        let base = ZoomBucket::enclosing(0.837);
        for zoom in [0.775, 0.80, 0.82, 0.837, 0.8409] {
            assert_eq!(
                ZoomBucket::enclosing(zoom),
                base,
                "zoom {zoom} left rung {}",
                base.rung()
            );
        }
        // Just past the top of the band is genuinely a different render. This is
        // inherent to quantizing: some drags do cross a boundary.
        assert_ne!(ZoomBucket::enclosing(0.85), base);
    }

    #[test]
    fn distant_zooms_do_not_share_a_bucket() {
        assert_ne!(
            ZoomBucket::enclosing(0.837),
            ZoomBucket::enclosing(1.4),
            "a 67% zoom change must re-render"
        );
    }

    #[test]
    fn bucketing_is_idempotent() {
        // Re-bucketing a bucket's own scale must not drift upwards, or repeated
        // layout passes would climb the ladder.
        for zoom in [0.1, 0.5, 0.837, 1.0, 2.0, 7.5] {
            let once = ZoomBucket::enclosing(zoom);
            let twice = ZoomBucket::enclosing(once.scale());
            assert_eq!(
                once, twice,
                "zoom {zoom} drifted from {once:?} to {twice:?}"
            );
        }
    }

    #[test]
    fn stepping_up_and_back_down_returns_to_the_same_rung() {
        let start = ZoomBucket::enclosing(1.0);
        assert_eq!(start.step(3).step(-3), start);
    }

    #[test]
    fn one_step_changes_zoom_by_about_nine_percent() {
        let start = ZoomBucket::enclosing(1.0);
        let ratio = start.step(1).scale() / start.scale();
        assert!(
            (1.08..1.10).contains(&ratio),
            "one rung changed zoom by a factor of {ratio}"
        );
    }

    #[test]
    fn stepping_saturates_at_both_ends_of_the_ladder() {
        let floor = ZoomBucket::enclosing(MIN_SCALE);
        let ceiling = ZoomBucket::enclosing(MAX_SCALE);

        assert_eq!(floor.step(-1000), floor, "stepped below the floor");
        assert_eq!(ceiling.step(1000), ceiling, "stepped above the ceiling");
        assert_eq!(floor.step(i16::MIN), floor, "saturating_add overflowed");
        assert_eq!(ceiling.step(i16::MAX), ceiling, "saturating_add overflowed");
    }

    #[test]
    fn a_rung_round_trips_through_an_integer_tag() {
        // This is how the render pipeline carries zoom without knowing about it.
        for zoom in [0.05, 0.5, 0.837, 1.0, 3.3, 40.0] {
            let bucket = ZoomBucket::enclosing(zoom);
            assert_eq!(ZoomBucket::from_rung(bucket.rung()), bucket, "zoom {zoom}");
        }
    }

    #[test]
    fn buckets_are_ordered_by_magnitude() {
        let small = ZoomBucket::enclosing(0.2);
        let medium = ZoomBucket::enclosing(1.0);
        let large = ZoomBucket::enclosing(5.0);
        assert!(small < medium && medium < large);
        assert!(small.scale() < medium.scale() && medium.scale() < large.scale());
    }

    #[test]
    fn degenerate_zoom_snaps_to_the_floor_rather_than_panicking() {
        let floor = ZoomBucket::enclosing(MIN_SCALE);
        for zoom in [0.0, -1.0, f32::NAN, f32::NEG_INFINITY] {
            assert_eq!(ZoomBucket::enclosing(zoom), floor, "zoom {zoom}");
        }
    }

    #[test]
    fn zoom_above_the_ceiling_is_clamped() {
        let ceiling = ZoomBucket::enclosing(MAX_SCALE);
        assert_eq!(ZoomBucket::enclosing(f32::INFINITY), ceiling);
        assert_eq!(ZoomBucket::enclosing(1.0e9), ceiling);
        assert!(ceiling.scale() <= MAX_SCALE);
    }

    #[test]
    fn every_bucket_scale_stays_within_the_clamp_range() {
        for zoom in [MIN_SCALE, 0.3, 1.0, 9.0, MAX_SCALE] {
            let scale = ZoomBucket::enclosing(zoom).scale();
            assert!(
                (MIN_SCALE..=MAX_SCALE).contains(&scale),
                "zoom {zoom} produced {scale}"
            );
        }
    }
}
