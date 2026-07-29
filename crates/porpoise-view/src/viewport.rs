//! Screen units, and the one conversion out of them.
//!
//! Its own module because "pixels" and "points" are the sharpest edge in this crate
//! and deserve one place to look. Three bugs have come from confusing them: the
//! pixels-versus-points scroll request in `docs/goal-2-plan.md` section 7a, and the
//! scroll bound and visible-page count in section 7e.

/// The visible window, in the shell's layout pixels.
///
/// Environment rather than state: it comes from the window manager and the user's
/// mouse, so nothing here sets it and no command changes it.
///
/// # Not PDF points
///
/// These fields used to be called `width_pt`/`height_pt`, and the collision is
/// genuinely easy to fall into: egui calls its own device-independent unit a
/// "point" too. But an egui point is a *screen* unit, while everything else in this
/// crate is a *PDF* point of 1/72 inch, and the two are only equal at zoom 1.0.
///
/// The distinction is not cosmetic. `content_height_pt - viewport.height()` reads
/// perfectly, was what the code did, and is wrong at every zoom but 1.0. Use
/// [`crate::View::visible_height_pt`] and [`crate::View::visible_width_pt`], which
/// divide by zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Usable width in layout pixels.
    pub width_px: f32,
    /// Usable height in layout pixels.
    pub height_px: f32,
}

impl Viewport {
    /// A viewport of the given size, treating degenerate values as zero.
    #[must_use]
    pub fn new(width_px: f32, height_px: f32) -> Self {
        let sane = |value: f32| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                0.0
            }
        };
        Self {
            width_px: sane(width_px),
            height_px: sane(height_px),
        }
    }

    /// Width as `f64`, the precision offsets use.
    #[must_use]
    pub fn width(self) -> f64 {
        f64::from(self.width_px)
    }

    /// Height as `f64`, the precision offsets use.
    #[must_use]
    pub fn height(self) -> f64 {
        f64::from(self.height_px)
    }
}

/// Converts a viewport extent in pixels into the document extent it covers.
///
/// The whole reason [`Viewport`] is measured in pixels and not points. A zoom factor
/// is *window pixels per page point*, so fitting needs pixels — but a scroll bound
/// needs points, and this is the only way across.
///
/// Getting this wrong does not look wrong. `content_height_pt - viewport.height()`
/// reads perfectly and is correct at zoom 1.0, which is where most testing happens.
/// At any other zoom it silently understates or overstates how far the document can
/// scroll, and how many pages are on screen. Same family as the pixels-versus-points
/// bug in `force_scroll`; see `docs/goal-2-plan.md` section 7a.
pub(crate) fn extent_pt(pixels: f64, zoom: f32) -> f64 {
    let zoom = f64::from(zoom);
    if zoom.is_finite() && zoom > 0.0 {
        pixels / zoom
    } else {
        pixels
    }
}
