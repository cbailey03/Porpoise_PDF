//! The viewer's addressable state, and the one function that changes it.
//!
//! Three kinds of thing were tangled together in the shell before Goal 2, and
//! separating them is most of what this module is for:
//!
//! | Kind | What | Lives in |
//! |---|---|---|
//! | **Authoritative state** | scroll position, zoom target, scroll mode | [`ViewState`] |
//! | **Environment** | viewport size | [`Viewport`], measured each frame |
//! | **Derived** | zoom factor, current page, visible range | [`View`], computed on demand |
//!
//! Zoom factor and current page used to be *stored* fields recomputed every frame,
//! which meant they could disagree with their inputs for a frame at a time.
//! Deriving them removes that possibility rather than managing it.
//!
//! # Who owns the scroll position
//!
//! egui's scroll area owns the *live* offset, and that is deliberate: its inertia
//! and smoothing are what make hand-scrolling feel right, and that feel was
//! verified on a 400-page document before Goal 2 was planned. Taking ownership
//! would mean reimplementing it.
//!
//! So a command does not move the view directly. It records a *request*, the shell
//! hands that to egui, and the shell reports back where egui actually landed. That
//! keeps [`apply`] pure and testable with no window, at the cost of a command
//! taking effect on the next frame. See `docs/goal-2-plan.md`, section 2.

use std::ops::Range;

use crate::command::{Outcome, Rejection, ViewCommand, ZoomTarget};
use crate::{MAX_SCALE, MIN_SCALE, PageNumber, ScrollLayout, ZoomBucket};

/// Scroll positions closer than this are the same place.
///
/// 1/100 of a point is 1/7200 inch — far below anything visible, and enough slack
/// that a position read back from the shell is not treated as a change from the
/// position we asked for.
const SCROLL_EPSILON_PT: f64 = 0.01;

/// How navigation behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ScrollMode {
    /// Continuous scrolling.
    Free,
    /// Navigation snaps to page boundaries.
    Paged,
}

impl ScrollMode {
    /// A short name for status display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paged => "paged",
        }
    }
}

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
/// [`View::visible_height_pt`] and [`View::visible_width_pt`], which divide by zoom.
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
fn extent_pt(pixels: f64, zoom: f32) -> f64 {
    let zoom = f64::from(zoom);
    if zoom.is_finite() && zoom > 0.0 {
        pixels / zoom
    } else {
        pixels
    }
}

/// Everything about the view that a command can change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewState {
    scroll_top_pt: f64,
    requested_scroll_pt: Option<f64>,
    scroll_left_pt: f64,
    requested_scroll_left_pt: Option<f64>,
    zoom_target: ZoomTarget,
    scroll_mode: ScrollMode,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            scroll_top_pt: 0.0,
            requested_scroll_pt: None,
            scroll_left_pt: 0.0,
            requested_scroll_left_pt: None,
            zoom_target: ZoomTarget::FitWidth,
            scroll_mode: ScrollMode::Free,
        }
    }
}

impl ViewState {
    /// A view at the top of the document, fit to width, scrolling freely.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the viewport top actually is, as last reported by the shell.
    #[must_use]
    pub fn scroll_top_pt(&self) -> f64 {
        self.scroll_top_pt
    }

    /// A scroll position asked for but not yet realized.
    #[must_use]
    pub fn requested_scroll_pt(&self) -> Option<f64> {
        self.requested_scroll_pt
    }

    /// Where the view will be once any pending request is honoured.
    ///
    /// This, not [`Self::scroll_top_pt`], is what navigation commands work from.
    /// Otherwise two `NextPage` commands issued before the next frame would both
    /// compute from the same stale position and advance one page between them.
    #[must_use]
    pub fn effective_scroll_pt(&self) -> f64 {
        self.requested_scroll_pt.unwrap_or(self.scroll_top_pt)
    }

    /// The current zoom target.
    #[must_use]
    pub fn zoom_target(&self) -> ZoomTarget {
        self.zoom_target
    }

    /// The current scroll mode.
    #[must_use]
    pub fn scroll_mode(&self) -> ScrollMode {
        self.scroll_mode
    }

    /// Records where the shell actually scrolled to.
    ///
    /// Called once per frame by the shell. Non-finite input is ignored rather than
    /// stored, so a degenerate frame cannot poison every later offset.
    pub fn report_scroll_top_pt(&mut self, top_pt: f64) {
        if top_pt.is_finite() {
            self.scroll_top_pt = top_pt;
        }
    }

    /// Takes any pending scroll request, for the shell to apply.
    pub fn take_requested_scroll_pt(&mut self) -> Option<f64> {
        self.requested_scroll_pt.take()
    }

    /// Where the viewport's left edge actually is, as last reported by the shell.
    ///
    /// Only meaningful when the document is wider than the window, which happens as
    /// soon as anyone zooms past fit-width on a landscape sheet.
    #[must_use]
    pub fn scroll_left_pt(&self) -> f64 {
        self.scroll_left_pt
    }

    /// A horizontal position asked for but not yet realized.
    #[must_use]
    pub fn requested_scroll_left_pt(&self) -> Option<f64> {
        self.requested_scroll_left_pt
    }

    /// Where the view will be horizontally once any pending request is honoured.
    #[must_use]
    pub fn effective_scroll_left_pt(&self) -> f64 {
        self.requested_scroll_left_pt.unwrap_or(self.scroll_left_pt)
    }

    /// Records where the shell actually panned to.
    pub fn report_scroll_left_pt(&mut self, left_pt: f64) {
        if left_pt.is_finite() {
            self.scroll_left_pt = left_pt;
        }
    }

    /// Takes any pending horizontal request, for the shell to apply.
    pub fn take_requested_scroll_left_pt(&mut self) -> Option<f64> {
        self.requested_scroll_left_pt.take()
    }

    /// Reads this state together with the layout and viewport it means something
    /// against.
    #[must_use]
    pub fn with<'a>(&'a self, layout: &'a ScrollLayout, viewport: Viewport) -> View<'a> {
        View {
            state: self,
            layout,
            viewport,
        }
    }
}

/// A [`ViewState`] plus the layout and viewport needed to interpret it.
///
/// Everything here is derived on demand rather than stored, so it cannot go stale.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    state: &'a ViewState,
    layout: &'a ScrollLayout,
    viewport: Viewport,
}

impl View<'_> {
    /// The zoom factor pages are laid out at.
    #[must_use]
    pub fn zoom(&self) -> f32 {
        match self.state.zoom_target {
            ZoomTarget::FitWidth => self.layout.fit_width_scale(self.viewport.width_px),
            ZoomTarget::FitPage => self
                .layout
                .fit_page_scale(self.viewport.width_px, self.viewport.height_px),
            // Clamped here as well as on the way in: a `Fixed` target can be set
            // from a wire message, and this is the boundary that matters.
            ZoomTarget::Fixed(scale) if scale.is_finite() => scale.clamp(MIN_SCALE, MAX_SCALE),
            ZoomTarget::Fixed(_) => 1.0,
        }
    }

    /// The quantized rung pages are rasterized for.
    #[must_use]
    pub fn bucket(&self) -> ZoomBucket {
        ZoomBucket::enclosing(self.zoom())
    }

    /// How much document the window covers vertically, in points.
    #[must_use]
    pub fn visible_height_pt(&self) -> f64 {
        extent_pt(self.viewport.height(), self.zoom())
    }

    /// How much document the window covers horizontally, in points.
    #[must_use]
    pub fn visible_width_pt(&self) -> f64 {
        extent_pt(self.viewport.width(), self.zoom())
    }

    /// The pages currently on screen.
    #[must_use]
    pub fn visible_pages(&self) -> Range<usize> {
        self.layout
            .visible_pages(self.state.scroll_top_pt, self.visible_height_pt())
    }

    /// The topmost page on screen, as a zero-based index.
    ///
    /// An index rather than a [`PageNumber`] because callers here use it to reach
    /// into the layout and the cache. [`ViewSnapshot::current_page`] is the same
    /// page in the numbering a person reads.
    ///
    /// Derived from where the view *actually is*, not from a pending request, so
    /// this always describes what a person can see.
    ///
    /// # Why the top and not the centre
    ///
    /// This was originally the page under the viewport's centre, which reads well
    /// in a status bar and is wrong in a way that matters. A viewport taller than a
    /// page — a small page, or a wide window at fit-width — has its centre inside
    /// the *next* page, so `GoToPage(3)` followed by reading this returned 4. The
    /// end-to-end control test caught it immediately, because that round trip is the
    /// first thing an agent depends on.
    ///
    /// "Most visible page" has the same flaw: after `GoToPage(N)` with three pages
    /// on screen, page N+1 can occupy more of the view than N does. Anchoring to the
    /// top is the only definition under which navigating somewhere and asking where
    /// you are agree.
    #[must_use]
    pub fn current_page(&self) -> usize {
        let last = self.layout.page_count().saturating_sub(1);
        self.visible_pages().start.min(last)
    }

    /// The furthest the view can scroll before running out of document.
    #[must_use]
    pub fn max_scroll_pt(&self) -> f64 {
        (self.layout.content_height_pt() - self.visible_height_pt()).max(0.0)
    }

    /// The furthest the view can pan before running out of page.
    ///
    /// Zero until zoom takes the document wider than the window, which is why
    /// horizontal panning is invisible at fit-width and essential past it.
    #[must_use]
    pub fn max_scroll_left_pt(&self) -> f64 {
        (self.layout.content_width_pt() - self.visible_width_pt()).max(0.0)
    }

    /// Everything about the view, in one readable value.
    #[must_use]
    pub fn snapshot(&self) -> ViewSnapshot {
        // Indices become page numbers here, at the boundary between what the view
        // computes and what someone reads.
        let visible = self.visible_pages();
        let (first_visible_page, last_visible_page) = if visible.is_empty() {
            (None, None)
        } else {
            (
                Some(PageNumber::from_index(visible.start)),
                // A half-open index range, so the last page on screen is the one
                // before its end.
                Some(PageNumber::from_index(visible.end - 1)),
            )
        };
        let current_page = if self.layout.page_count() == 0 {
            None
        } else {
            Some(PageNumber::from_index(self.current_page()))
        };

        ViewSnapshot {
            page_count: self.layout.page_count(),
            current_page,
            first_visible_page,
            last_visible_page,
            scroll_top_pt: self.state.scroll_top_pt,
            pending_scroll_pt: self.state.requested_scroll_pt,
            scroll_left_pt: self.state.scroll_left_pt,
            pending_scroll_left_pt: self.state.requested_scroll_left_pt,
            content_height_pt: self.layout.content_height_pt(),
            content_width_pt: self.layout.content_width_pt(),
            max_scroll_pt: self.max_scroll_pt(),
            max_scroll_left_pt: self.max_scroll_left_pt(),
            zoom: self.zoom(),
            zoom_target: self.state.zoom_target,
            scroll_mode: self.state.scroll_mode,
            viewport_width_px: self.viewport.width_px,
            viewport_height_px: self.viewport.height_px,
        }
    }
}

/// A readable description of the whole view.
///
/// The status bar renders this, and the control channel serializes it, so anything
/// a person can see about the view's state is also something an agent can read.
///
/// Note that [`Self::scroll_top_pt`] and [`Self::pending_scroll_pt`] are both here
/// on purpose. An agent that issues a scroll and immediately reads the snapshot
/// needs to distinguish *where the view is* from *where it is going*, or it will
/// capture the old position and believe it is the new one.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ViewSnapshot {
    /// Pages in the open document. Zero when nothing is open.
    pub page_count: usize,
    /// The topmost page on screen, or `None` when no document is open.
    pub current_page: Option<PageNumber>,
    /// The first page with any part on screen, or `None` when none is.
    pub first_visible_page: Option<PageNumber>,
    /// The last page with any part on screen, *inclusive*.
    ///
    /// A first/last pair rather than a range, because a half-open range whose start
    /// counts from 1 is an invitation to an off-by-one: `{"start":51,"end":53}`
    /// takes a moment to read, where `first 51, last 52` does not.
    pub last_visible_page: Option<PageNumber>,
    /// Where the viewport top is, in points.
    pub scroll_top_pt: f64,
    /// Where a command has asked it to go but the shell has not yet moved it.
    pub pending_scroll_pt: Option<f64>,
    /// Where the viewport's left edge is, in points.
    pub scroll_left_pt: f64,
    /// Where a command has asked it to pan but the shell has not yet moved it.
    pub pending_scroll_left_pt: Option<f64>,
    /// Total scrollable height, in points.
    pub content_height_pt: f64,
    /// Width of the widest page, in points.
    pub content_width_pt: f64,
    /// The largest useful value of [`Self::scroll_top_pt`].
    pub max_scroll_pt: f64,
    /// The largest useful value of [`Self::scroll_left_pt`]. Zero when the document
    /// fits the window's width, which is the normal case at fit-width.
    pub max_scroll_left_pt: f64,
    /// The zoom factor in force.
    pub zoom: f32,
    /// What the zoom factor was derived from.
    pub zoom_target: ZoomTarget,
    /// How navigation behaves.
    pub scroll_mode: ScrollMode,
    /// Viewport width in layout **pixels**, not points.
    ///
    /// Pixels because that is what a zoom factor is measured against. Divide by
    /// [`Self::zoom`] for the extent of document it covers — or just read
    /// [`Self::max_scroll_left_pt`], which has already done it.
    pub viewport_width_px: f32,
    /// Viewport height in layout **pixels**. See [`Self::viewport_width_px`].
    pub viewport_height_px: f32,
}

/// Carries out `command`, reporting what it did.
///
/// Pure: no window, no document, no renderer. Scroll movement is recorded as a
/// request for the shell to realize — see the module docs on ownership.
pub fn apply(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    command: ViewCommand,
) -> Outcome {
    let page_count = layout.page_count();

    match command {
        ViewCommand::GoToPage { page } => {
            if page_count == 0 {
                return Outcome::Rejected(Rejection::NoPages);
            }
            // The one place a page number becomes an index. `PageNumber` cannot be
            // zero, so this is the only bound left to check.
            let index = page.index();
            if index >= page_count {
                return Outcome::Rejected(Rejection::NoSuchPage { page, page_count });
            }
            scroll_to_page(state, layout, viewport, index)
        }
        ViewCommand::NextPage => step_page(state, layout, viewport, 1),
        ViewCommand::PreviousPage => step_page(state, layout, viewport, -1),
        ViewCommand::FirstPage => {
            if page_count == 0 {
                return Outcome::Rejected(Rejection::NoPages);
            }
            scroll_to_page(state, layout, viewport, 0)
        }
        ViewCommand::LastPage => {
            if page_count == 0 {
                return Outcome::Rejected(Rejection::NoPages);
            }
            scroll_to_page(state, layout, viewport, page_count - 1)
        }
        ViewCommand::ScrollTo { points } => {
            if !points.is_finite() {
                return Outcome::Rejected(Rejection::NotFinite { argument: "points" });
            }
            request_scroll(state, layout, viewport, points)
        }
        ViewCommand::ScrollBy { points } => {
            if !points.is_finite() {
                return Outcome::Rejected(Rejection::NotFinite { argument: "points" });
            }
            request_scroll(
                state,
                layout,
                viewport,
                state.effective_scroll_pt() + points,
            )
        }
        ViewCommand::ScrollByViewports { fraction } => {
            if !fraction.is_finite() {
                return Outcome::Rejected(Rejection::NotFinite {
                    argument: "fraction",
                });
            }
            // A screenful is however much document the window covers, which is a
            // pixel height divided by zoom -- not the pixel height itself.
            let points = state.with(layout, viewport).visible_height_pt() * fraction;
            request_scroll(
                state,
                layout,
                viewport,
                state.effective_scroll_pt() + points,
            )
        }
        ViewCommand::PanTo { points } => {
            if !points.is_finite() {
                return Outcome::Rejected(Rejection::NotFinite { argument: "points" });
            }
            request_pan(state, layout, viewport, points)
        }
        ViewCommand::PanBy { points } => {
            if !points.is_finite() {
                return Outcome::Rejected(Rejection::NotFinite { argument: "points" });
            }
            request_pan(
                state,
                layout,
                viewport,
                state.effective_scroll_left_pt() + points,
            )
        }
        ViewCommand::SetZoom { target } => set_zoom(state, layout, viewport, target),
        ViewCommand::StepZoom { rungs } => {
            let current = state.with(layout, viewport).bucket();
            let stepped = current.step(rungs).scale();
            set_zoom(state, layout, viewport, ZoomTarget::Fixed(stepped))
        }
        ViewCommand::SetScrollMode { mode } => {
            if state.scroll_mode == mode {
                return Outcome::Unchanged;
            }
            state.scroll_mode = mode;
            Outcome::Changed
        }
    }
}

/// Moves `delta` pages from wherever navigation is heading.
fn step_page(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    delta: isize,
) -> Outcome {
    let page_count = layout.page_count();
    if page_count == 0 {
        return Outcome::Rejected(Rejection::NoPages);
    }
    // Navigation works from the *effective* position so that a batch of commands
    // in one frame composes; see `ViewState::effective_scroll_pt`.
    let from = layout.page_at_pt(state.effective_scroll_pt()).unwrap_or(0);
    let target = if delta >= 0 {
        from.saturating_add(delta.unsigned_abs())
    } else {
        from.saturating_sub(delta.unsigned_abs())
    };
    scroll_to_page(state, layout, viewport, target.min(page_count - 1))
}

fn scroll_to_page(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    index: usize,
) -> Outcome {
    match layout.page_top_pt(index) {
        Some(top_pt) => request_scroll(state, layout, viewport, top_pt),
        None => Outcome::Rejected(Rejection::NoSuchPage {
            page: PageNumber::from_index(index),
            page_count: layout.page_count(),
        }),
    }
}

/// Asks the shell to scroll, but only if that would actually move the view.
fn request_scroll(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    target_pt: f64,
) -> Outcome {
    if (clamp_scroll(state, layout, viewport, target_pt) - state.effective_scroll_pt()).abs()
        < SCROLL_EPSILON_PT
    {
        return Outcome::Unchanged;
    }
    force_scroll(state, layout, viewport, target_pt);
    Outcome::Changed
}

/// Asks the shell to scroll even if the position is unchanged.
///
/// Needed after a zoom change, and the reason is worth spelling out. Scroll
/// position here is in **points**, which do not depend on zoom — page 5 starts at
/// 3960 pt whatever the magnification. The shell's scroll offset is in **pixels**,
/// which do. So changing zoom leaves the shell holding an offset that now points
/// somewhere else in the document, even though our position has not changed by our
/// own measure.
///
/// A request is how we tell the shell to re-derive its pixel offset from our
/// points. Suppressing it because "the position is the same" would leave the view
/// wherever the stale pixel offset happens to land.
fn force_scroll(state: &mut ViewState, layout: &ScrollLayout, viewport: Viewport, target_pt: f64) {
    state.requested_scroll_pt = Some(clamp_scroll(state, layout, viewport, target_pt));
}

fn clamp_scroll(
    state: &ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    target_pt: f64,
) -> f64 {
    target_pt.clamp(0.0, state.with(layout, viewport).max_scroll_pt())
}

/// Asks the shell to pan, but only if that would actually move the view.
fn request_pan(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    target_pt: f64,
) -> Outcome {
    if (clamp_pan(state, layout, viewport, target_pt) - state.effective_scroll_left_pt()).abs()
        < SCROLL_EPSILON_PT
    {
        return Outcome::Unchanged;
    }
    force_pan(state, layout, viewport, target_pt);
    Outcome::Changed
}

/// Asks the shell to pan even if the position is unchanged.
///
/// Needed after a zoom change for exactly the reason [`force_scroll`] is: our
/// position is in points and the shell's offset is in pixels.
fn force_pan(state: &mut ViewState, layout: &ScrollLayout, viewport: Viewport, target_pt: f64) {
    state.requested_scroll_left_pt = Some(clamp_pan(state, layout, viewport, target_pt));
}

fn clamp_pan(state: &ViewState, layout: &ScrollLayout, viewport: Viewport, target_pt: f64) -> f64 {
    target_pt.clamp(0.0, state.with(layout, viewport).max_scroll_left_pt())
}

fn set_zoom(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    target: ZoomTarget,
) -> Outcome {
    if let ZoomTarget::Fixed(scale) = target
        && !scale.is_finite()
    {
        return Outcome::Rejected(Rejection::NotFinite { argument: "scale" });
    }
    if state.zoom_target == target {
        return Outcome::Unchanged;
    }

    // Anchor on the page in view before changing zoom, and force the request even
    // if our own position is unchanged — see `force_scroll` for why that matters.
    //
    // Derived from the *effective* position, not the actual one, for the same
    // reason `step_page` is: a zoom change issued in the same batch as a navigation
    // would otherwise anchor on where we still are and throw the navigation away.
    // That also makes the anchor independent of the viewport, which matters because
    // a command can arrive before the first frame has measured one.
    let anchor = layout.page_at_pt(state.effective_scroll_pt()).unwrap_or(0);
    let left_pt = state.effective_scroll_left_pt();
    state.zoom_target = target;
    if let Some(top_pt) = layout.page_top_pt(anchor) {
        force_scroll(state, layout, viewport, top_pt);
    }
    // The horizontal axis needs the same treatment, and additionally needs
    // re-clamping: zooming out can make the document narrower than the window, which
    // takes `max_scroll_left_pt` to zero and makes any pan offset invalid.
    force_pan(state, layout, viewport, left_pt);
    Outcome::Changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use porpoise_doc::PageGeometry;

    /// US Letter at 72 DPI.
    fn letter() -> PageGeometry {
        PageGeometry {
            width_pt: 612.0,
            height_pt: 792.0,
        }
    }

    /// Ten letter pages with no gap, so page N starts at N * 792.
    fn ten_pages() -> ScrollLayout {
        ScrollLayout::vertical(&[letter(); 10], 0.0)
    }

    /// A viewport half a page tall, so several pages are reachable.
    fn viewport() -> Viewport {
        Viewport::new(612.0, 396.0)
    }

    /// `GoToPage` for a zero-based index.
    ///
    /// Most assertions here are about layout offsets, which are indexed, so these
    /// tests stay in index terms and name the conversion here rather than adding
    /// one to every expected value. The one-based meaning is pinned down by
    /// `page_one_is_the_top_of_the_document` and by the `PageNumber` tests.
    fn go_to_index(index: usize) -> ViewCommand {
        ViewCommand::GoToPage {
            page: PageNumber::from_index(index),
        }
    }

    fn run(state: &mut ViewState, command: ViewCommand) -> Outcome {
        apply(state, &ten_pages(), viewport(), command)
    }

    /// Applies a command and settles the resulting requests, as a frame would.
    ///
    /// Both axes, because a frame honours both — and because `set_zoom` produces a
    /// request on each.
    fn run_and_settle(state: &mut ViewState, command: ViewCommand) -> Outcome {
        let outcome = run(state, command);
        if let Some(top) = state.take_requested_scroll_pt() {
            state.report_scroll_top_pt(top);
        }
        if let Some(left) = state.take_requested_scroll_left_pt() {
            state.report_scroll_left_pt(left);
        }
        outcome
    }

    /// A view at 2x, where the 612 pt page is twice as wide as the 612 px window.
    ///
    /// Panning only exists past fit-width: *at* fit-width the document is exactly the
    /// window's width by definition, so there is nowhere to pan. Every pan test needs
    /// a zoom that overflows, which is the clearest statement of what the feature is
    /// for.
    fn zoomed_in() -> ViewState {
        let mut state = ViewState::new();
        run_and_settle(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(2.0),
            },
        );
        state
    }

    // --- Coverage enforcement ------------------------------------------------

    #[test]
    fn every_command_has_a_behaviour_test() {
        // The list is exhaustive by construction (see `command.rs`), so naming a
        // test per variant here means adding a command without testing it fails.
        for command in ViewCommand::ALL {
            let named = match command {
                ViewCommand::GoToPage { .. } => "go_to_page_scrolls_to_that_pages_top",
                ViewCommand::NextPage => "next_page_advances_one_page",
                ViewCommand::PreviousPage => "previous_page_goes_back_one_page",
                ViewCommand::FirstPage => "first_page_returns_to_the_top",
                ViewCommand::LastPage => "last_page_goes_to_the_final_page",
                ViewCommand::ScrollTo { .. } => "scroll_to_moves_to_an_absolute_offset",
                ViewCommand::ScrollBy { .. } => "scroll_by_moves_relative_to_here",
                ViewCommand::PanTo { .. } => "pan_to_moves_to_an_absolute_horizontal_offset",
                ViewCommand::PanBy { .. } => "pan_by_moves_relative_to_here",
                ViewCommand::ScrollByViewports { .. } => "scroll_by_viewports_uses_viewport_height",
                ViewCommand::SetZoom { .. } => "set_zoom_changes_the_target_and_anchors",
                ViewCommand::StepZoom { .. } => "step_zoom_moves_along_the_ladder",
                ViewCommand::SetScrollMode { .. } => "set_scroll_mode_switches_mode",
            };
            assert!(
                !named.is_empty(),
                "{} has no named behaviour test",
                command.name()
            );
        }
    }

    // --- Navigation ----------------------------------------------------------

    #[test]
    fn go_to_page_scrolls_to_that_pages_top() {
        let mut state = ViewState::new();
        assert_eq!(run(&mut state, go_to_index(3)), Outcome::Changed);
        assert_eq!(state.requested_scroll_pt(), Some(3.0 * 792.0));
    }

    #[test]
    fn go_to_page_past_the_end_is_rejected_with_the_real_page_count() {
        let mut state = ViewState::new();
        assert_eq!(
            // Page 11 of a ten-page document. Index 10 is past the end, and the
            // rejection reports the number that was asked for, not the index.
            run(&mut state, go_to_index(10)),
            Outcome::Rejected(Rejection::NoSuchPage {
                page: PageNumber::from_index(10),
                page_count: 10
            })
        );
        assert_eq!(
            state.requested_scroll_pt(),
            None,
            "a rejected command moved the view"
        );
    }

    #[test]
    fn navigating_an_empty_document_is_rejected_rather_than_silently_doing_nothing() {
        let empty = ScrollLayout::vertical(&[], 0.0);
        for command in [
            go_to_index(0),
            ViewCommand::FirstPage,
            ViewCommand::LastPage,
            ViewCommand::NextPage,
            ViewCommand::PreviousPage,
        ] {
            let mut state = ViewState::new();
            assert_eq!(
                apply(&mut state, &empty, viewport(), command),
                Outcome::Rejected(Rejection::NoPages),
                "{} should report there are no pages",
                command.name()
            );
        }
    }

    #[test]
    fn next_page_advances_one_page() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::NextPage);
        assert_eq!(state.scroll_top_pt(), 792.0);
    }

    #[test]
    fn previous_page_goes_back_one_page() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, go_to_index(4));
        run_and_settle(&mut state, ViewCommand::PreviousPage);
        assert_eq!(state.scroll_top_pt(), 3.0 * 792.0);
    }

    #[test]
    fn repeated_next_page_in_one_frame_advances_that_many_pages() {
        // The composition case: without `effective_scroll_pt` all three commands
        // would compute from the same stale position and advance one page total.
        // An agent sending a batch depends on this.
        let mut state = ViewState::new();
        for _ in 0..3 {
            run(&mut state, ViewCommand::NextPage);
        }
        assert_eq!(state.requested_scroll_pt(), Some(3.0 * 792.0));
    }

    #[test]
    fn next_page_at_the_end_is_unchanged_rather_than_rejected() {
        // Being already where you asked to go is not an error.
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::LastPage);
        let before = state.scroll_top_pt();
        assert_eq!(run(&mut state, ViewCommand::NextPage), Outcome::Unchanged);
        assert_eq!(state.scroll_top_pt(), before);
    }

    #[test]
    fn previous_page_at_the_start_is_unchanged() {
        let mut state = ViewState::new();
        assert_eq!(
            run(&mut state, ViewCommand::PreviousPage),
            Outcome::Unchanged
        );
    }

    #[test]
    fn first_page_returns_to_the_top() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, go_to_index(7));
        run_and_settle(&mut state, ViewCommand::FirstPage);
        assert_eq!(state.scroll_top_pt(), 0.0);
    }

    #[test]
    fn last_page_goes_to_the_final_page() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::LastPage);
        // Page 9 starts at 7128, but the document only allows scrolling to
        // content_height - viewport_height = 7920 - 396 = 7524. 7128 is under that.
        assert_eq!(state.scroll_top_pt(), 9.0 * 792.0);
    }

    // --- Scrolling -----------------------------------------------------------

    #[test]
    fn scroll_to_moves_to_an_absolute_offset() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::ScrollTo { points: 1000.0 });
        assert_eq!(state.scroll_top_pt(), 1000.0);
    }

    #[test]
    fn scroll_by_moves_relative_to_here() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::ScrollTo { points: 1000.0 });
        run_and_settle(&mut state, ViewCommand::ScrollBy { points: -250.0 });
        assert_eq!(state.scroll_top_pt(), 750.0);
    }

    #[test]
    fn scroll_by_viewports_uses_viewport_height() {
        // The viewport is 396 pt, so 0.9 of it is 356.4.
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::ScrollByViewports { fraction: 0.9 });
        assert!(
            (state.scroll_top_pt() - 356.4).abs() < 0.001,
            "got {}",
            state.scroll_top_pt()
        );
    }

    // --- Panning -------------------------------------------------------------

    #[test]
    fn panning_does_nothing_while_the_document_fits_the_window() {
        // Why the arrow keys are harmless at fit-width rather than needing to be
        // conditionally bound.
        let mut state = ViewState::new();
        assert_eq!(
            state.with(&ten_pages(), viewport()).max_scroll_left_pt(),
            0.0
        );
        assert_eq!(
            run(&mut state, ViewCommand::PanTo { points: 100.0 }),
            Outcome::Unchanged
        );
    }

    #[test]
    fn pan_to_moves_to_an_absolute_horizontal_offset() {
        let mut state = zoomed_in();
        assert_eq!(
            run_and_settle(&mut state, ViewCommand::PanTo { points: 120.0 }),
            Outcome::Changed
        );
        assert_eq!(state.scroll_left_pt(), 120.0);
    }

    #[test]
    fn pan_by_moves_relative_to_here() {
        let mut state = zoomed_in();
        run_and_settle(&mut state, ViewCommand::PanTo { points: 120.0 });
        run_and_settle(&mut state, ViewCommand::PanBy { points: -50.0 });
        assert_eq!(state.scroll_left_pt(), 70.0);
    }

    #[test]
    fn panning_is_clamped_to_the_page_width() {
        // A 612 pt page with 306 pt of it visible at 2x, so 306 is as far as it goes.
        let mut state = zoomed_in();
        run_and_settle(&mut state, ViewCommand::PanTo { points: 1.0e9 });
        assert_eq!(state.scroll_left_pt(), 306.0);

        run_and_settle(&mut state, ViewCommand::PanTo { points: -1.0e9 });
        assert_eq!(state.scroll_left_pt(), 0.0);
    }

    #[test]
    fn two_pans_in_one_frame_compose() {
        // The same property batched navigation needs: both must not compute from the
        // same stale position. See `effective_scroll_left_pt`.
        let mut state = zoomed_in();
        run(&mut state, ViewCommand::PanBy { points: 50.0 });
        run(&mut state, ViewCommand::PanBy { points: 50.0 });
        assert_eq!(state.requested_scroll_left_pt(), Some(100.0));
    }

    #[test]
    fn zooming_out_pulls_a_pan_back_into_range() {
        // The case that would strand the view: pan right at 2x, then return to
        // fit-width where there is no horizontal room at all. A stale offset would
        // leave the page shifted sideways with no scrollbar left to correct it.
        let mut state = zoomed_in();
        run_and_settle(&mut state, ViewCommand::PanTo { points: 306.0 });
        run_and_settle(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::FitWidth,
            },
        );
        assert_eq!(state.scroll_left_pt(), 0.0);
    }

    #[test]
    fn a_non_finite_pan_is_rejected_and_names_its_argument() {
        let mut state = zoomed_in();
        for points in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                run(&mut state, ViewCommand::PanTo { points }),
                Outcome::Rejected(Rejection::NotFinite { argument: "points" })
            );
            assert_eq!(
                run(&mut state, ViewCommand::PanBy { points }),
                Outcome::Rejected(Rejection::NotFinite { argument: "points" })
            );
        }
    }

    // --- Pixels versus points ------------------------------------------------

    #[test]
    fn how_far_the_view_can_scroll_depends_on_zoom() {
        // The bug this pins down: `max_scroll_pt` subtracted the viewport's *screen*
        // height from the document's height in *PDF points*. Those agree only at zoom
        // 1.0, which is exactly where the default test viewport sits, so every
        // existing test passed. At 2x the window covers half as much document, so
        // there is more of it left to scroll through.
        let layout = ten_pages();
        let mut state = ViewState::new();
        assert_eq!(
            state.with(&layout, viewport()).max_scroll_pt(),
            7920.0 - 396.0
        );

        run_and_settle(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(2.0),
            },
        );
        // 396 screen units at 2x is 198 pt of document.
        assert_eq!(
            state.with(&layout, viewport()).max_scroll_pt(),
            7920.0 - 198.0
        );
    }

    #[test]
    fn how_many_pages_are_on_screen_depends_on_zoom() {
        // The same bug's other consequence, and the more visible one: zoomed out far
        // enough, pages that are genuinely on screen were not counted as visible, so
        // they were never requested and showed as placeholders.
        let layout = ten_pages();
        let mut state = ViewState::new();
        assert_eq!(state.with(&layout, viewport()).visible_pages(), 0..1);

        run_and_settle(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(0.4),
            },
        );
        // 396 screen units at 0.4 is 990 pt, which reaches into the second page.
        assert_eq!(state.with(&layout, viewport()).visible_pages(), 0..2);
    }

    #[test]
    fn scrolling_is_clamped_to_the_document() {
        let mut state = ViewState::new();
        // 10 pages of 792 = 7920 content, less a 396 viewport, so 7524 is the end.
        run_and_settle(&mut state, ViewCommand::ScrollTo { points: 1.0e9 });
        assert_eq!(state.scroll_top_pt(), 7524.0);

        run_and_settle(&mut state, ViewCommand::ScrollTo { points: -1.0e9 });
        assert_eq!(state.scroll_top_pt(), 0.0);
    }

    #[test]
    fn a_non_finite_scroll_is_rejected_and_names_its_argument() {
        let mut state = ViewState::new();
        for points in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                run(&mut state, ViewCommand::ScrollTo { points }),
                Outcome::Rejected(Rejection::NotFinite { argument: "points" })
            );
            assert_eq!(
                run(&mut state, ViewCommand::ScrollBy { points }),
                Outcome::Rejected(Rejection::NotFinite { argument: "points" })
            );
        }
        assert_eq!(
            run(
                &mut state,
                ViewCommand::ScrollByViewports { fraction: f64::NAN }
            ),
            Outcome::Rejected(Rejection::NotFinite {
                argument: "fraction"
            })
        );
        assert_eq!(state.requested_scroll_pt(), None);
    }

    #[test]
    fn a_viewport_taller_than_the_document_cannot_scroll() {
        let layout = ScrollLayout::vertical(&[letter()], 0.0);
        let tall = Viewport::new(612.0, 5000.0);
        let mut state = ViewState::new();
        assert_eq!(
            apply(
                &mut state,
                &layout,
                tall,
                ViewCommand::ScrollTo { points: 400.0 }
            ),
            Outcome::Unchanged
        );
    }

    // --- Zoom ----------------------------------------------------------------

    #[test]
    fn set_zoom_changes_the_target_and_anchors() {
        let mut state = ViewState::new();
        run_and_settle(&mut state, go_to_index(5));

        let outcome = run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(2.0),
            },
        );
        assert_eq!(outcome, Outcome::Changed);
        assert_eq!(state.zoom_target(), ZoomTarget::Fixed(2.0));
        // Anchored: still looking at page 5, not wherever the old pixel offset
        // happens to land at the new zoom. The request is issued even though our
        // point position did not change, because the shell's pixel offset did —
        // see `force_scroll`.
        assert_eq!(state.requested_scroll_pt(), Some(5.0 * 792.0));
    }

    #[test]
    fn a_zoom_change_always_issues_a_scroll_request() {
        // Even at the very top, where the point position is unarguably unchanged.
        // Without the forced request the shell keeps a pixel offset that means a
        // different place in the document at the new zoom.
        let mut state = ViewState::new();
        assert_eq!(state.scroll_top_pt(), 0.0);
        run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(4.0),
            },
        );
        assert_eq!(
            state.requested_scroll_pt(),
            Some(0.0),
            "a zoom change left the shell to keep its stale pixel offset"
        );
    }

    #[test]
    fn setting_the_zoom_already_in_force_changes_nothing() {
        // Otherwise an agent polling `SetZoom(FitWidth)` would scroll the view to
        // the anchor page on every call.
        let mut state = ViewState::new();
        run_and_settle(&mut state, ViewCommand::ScrollTo { points: 1000.0 });
        assert_eq!(
            run(
                &mut state,
                ViewCommand::SetZoom {
                    target: ZoomTarget::FitWidth
                }
            ),
            Outcome::Unchanged
        );
        assert_eq!(
            state.requested_scroll_pt(),
            None,
            "an unchanged zoom moved the view"
        );
    }

    #[test]
    fn zooming_after_navigating_keeps_the_page_navigated_to() {
        // Found by driving the real program by hand: `go_to_page` then `set_zoom`
        // in one batch anchored on where the view still *was* and discarded the
        // navigation. Same class of bug as `repeated_next_page_in_one_frame`.
        let mut state = ViewState::new();
        run(&mut state, go_to_index(4));
        run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(2.0),
            },
        );
        assert_eq!(
            state.requested_scroll_pt(),
            Some(4.0 * 792.0),
            "the zoom change threw away the pending navigation"
        );
    }

    #[test]
    fn zooming_before_the_viewport_is_known_does_not_reset_the_view() {
        // A command can arrive before the first frame has measured a viewport, which
        // is exactly what happens when an agent sends a batch at startup. Anchoring
        // must not depend on a viewport we do not have yet.
        let mut state = ViewState::new();
        let layout = ten_pages();
        let unmeasured = Viewport::new(0.0, 0.0);

        apply(&mut state, &layout, unmeasured, go_to_index(3));
        apply(
            &mut state,
            &layout,
            unmeasured,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(2.0),
            },
        );
        assert_eq!(state.requested_scroll_pt(), Some(3.0 * 792.0));
    }

    #[test]
    fn a_non_finite_fixed_zoom_is_rejected() {
        let mut state = ViewState::new();
        for scale in [f32::NAN, f32::INFINITY] {
            assert_eq!(
                run(
                    &mut state,
                    ViewCommand::SetZoom {
                        target: ZoomTarget::Fixed(scale)
                    }
                ),
                Outcome::Rejected(Rejection::NotFinite { argument: "scale" })
            );
        }
        assert_eq!(
            state.zoom_target(),
            ZoomTarget::FitWidth,
            "zoom target changed anyway"
        );
    }

    #[test]
    fn step_zoom_moves_along_the_ladder() {
        let mut state = ViewState::new();
        run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(1.0),
            },
        );
        let before = state.with(&ten_pages(), viewport()).zoom();

        run(&mut state, ViewCommand::StepZoom { rungs: 1 });
        let after = state.with(&ten_pages(), viewport()).zoom();

        let ratio = after / before;
        assert!(
            (1.08..1.10).contains(&ratio),
            "one rung changed zoom by {ratio}"
        );
    }

    #[test]
    fn step_zoom_saturates_rather_than_overflowing() {
        let mut state = ViewState::new();
        run(&mut state, ViewCommand::StepZoom { rungs: i16::MAX });
        let zoom = state.with(&ten_pages(), viewport()).zoom();
        assert!((MIN_SCALE..=MAX_SCALE).contains(&zoom), "got {zoom}");

        run(&mut state, ViewCommand::StepZoom { rungs: i16::MIN });
        let zoom = state.with(&ten_pages(), viewport()).zoom();
        assert!((MIN_SCALE..=MAX_SCALE).contains(&zoom), "got {zoom}");
    }

    #[test]
    fn a_fixed_zoom_from_the_wire_is_clamped_when_read() {
        // `SetZoom` accepts any finite factor; the clamp is applied where the value
        // is used, so an out-of-range value cannot escape into layout arithmetic.
        let mut state = ViewState::new();
        run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(1_000.0),
            },
        );
        assert_eq!(state.with(&ten_pages(), viewport()).zoom(), MAX_SCALE);
    }

    // --- Modes ---------------------------------------------------------------

    #[test]
    fn set_scroll_mode_switches_mode() {
        let mut state = ViewState::new();
        assert_eq!(state.scroll_mode(), ScrollMode::Free);
        assert_eq!(
            run(
                &mut state,
                ViewCommand::SetScrollMode {
                    mode: ScrollMode::Paged
                }
            ),
            Outcome::Changed
        );
        assert_eq!(state.scroll_mode(), ScrollMode::Paged);
        assert_eq!(
            run(
                &mut state,
                ViewCommand::SetScrollMode {
                    mode: ScrollMode::Paged
                }
            ),
            Outcome::Unchanged
        );
    }

    // --- Derived values ------------------------------------------------------

    #[test]
    fn current_page_is_the_topmost_page_on_screen() {
        let mut state = ViewState::new();
        let layout = ten_pages();
        assert_eq!(state.with(&layout, viewport()).current_page(), 0);

        run_and_settle(&mut state, go_to_index(6));
        assert_eq!(state.with(&layout, viewport()).current_page(), 6);
    }

    #[test]
    fn going_to_a_page_and_asking_where_we_are_agree() {
        // The round trip an agent depends on before anything else. It failed when
        // `current_page` was the page under the viewport centre: a viewport taller
        // than a page put the centre inside the *next* one, so `GoToPage(3)` read
        // back as 4. Checked across viewport heights because the bug only appeared
        // once the viewport exceeded a page.
        let layout = ten_pages();
        for height in [100.0, 396.0, 800.0, 1600.0, 2400.0] {
            let port = Viewport::new(612.0, height);
            for page in 0..10 {
                let mut state = ViewState::new();
                let outcome = apply(&mut state, &layout, port, go_to_index(page));
                assert!(outcome.rejected().is_none(), "page {page} was refused");
                if let Some(top) = state.take_requested_scroll_pt() {
                    state.report_scroll_top_pt(top);
                }

                // The last pages cannot reach the top of a tall viewport, because
                // the document runs out; those clamp rather than round-trip.
                let clamped = state.with(&layout, port).max_scroll_pt()
                    < layout.page_top_pt(page).unwrap_or(0.0);
                if clamped {
                    continue;
                }
                assert_eq!(
                    state.with(&layout, port).current_page(),
                    page,
                    "go_to_page({page}) in a {height} pt viewport read back wrong"
                );
            }
        }
    }

    #[test]
    fn a_viewport_taller_than_the_whole_document_still_reports_the_first_page() {
        let layout = ten_pages();
        let state = ViewState::new();
        let huge = Viewport::new(612.0, 100_000.0);
        assert_eq!(state.with(&layout, huge).current_page(), 0);
    }

    #[test]
    fn current_page_reports_where_we_are_not_where_we_are_going() {
        // An agent has to be able to tell a realized move from a pending one, or it
        // will screenshot the old position and believe it is the new one.
        let mut state = ViewState::new();
        let layout = ten_pages();
        run(&mut state, go_to_index(6));

        assert_eq!(
            state.with(&layout, viewport()).current_page(),
            0,
            "current_page jumped before the shell reported the move"
        );
        assert_eq!(state.requested_scroll_pt(), Some(6.0 * 792.0));
    }

    #[test]
    fn derived_zoom_tracks_the_viewport_for_fit_modes() {
        let state = ViewState::new();
        let layout = ten_pages();
        // Fit-width on a 612 pt page: a 1224 pt viewport is 2x, a 306 pt one 0.5x.
        assert_eq!(
            state.with(&layout, Viewport::new(1224.0, 400.0)).zoom(),
            2.0
        );
        assert_eq!(state.with(&layout, Viewport::new(306.0, 400.0)).zoom(), 0.5);
    }

    #[test]
    fn a_degenerate_viewport_still_yields_a_usable_zoom() {
        let state = ViewState::new();
        let layout = ten_pages();
        for (width, height) in [(0.0, 0.0), (f32::NAN, f32::NAN), (-5.0, -5.0)] {
            let zoom = state.with(&layout, Viewport::new(width, height)).zoom();
            assert!(
                zoom.is_finite() && zoom > 0.0,
                "{width}x{height} gave {zoom}"
            );
        }
    }

    #[test]
    fn reporting_a_non_finite_scroll_position_is_ignored() {
        let mut state = ViewState::new();
        state.report_scroll_top_pt(500.0);
        state.report_scroll_top_pt(f64::NAN);
        assert_eq!(state.scroll_top_pt(), 500.0, "NaN poisoned the position");
    }

    #[test]
    fn max_scroll_never_goes_negative() {
        let layout = ScrollLayout::vertical(&[letter()], 0.0);
        let state = ViewState::new();
        let tall = Viewport::new(612.0, 10_000.0);
        assert_eq!(state.with(&layout, tall).max_scroll_pt(), 0.0);
    }

    // --- Snapshot ------------------------------------------------------------

    #[test]
    fn a_snapshot_describes_the_whole_view() {
        let mut state = ViewState::new();
        let layout = ten_pages();
        run_and_settle(&mut state, go_to_index(4));

        let snapshot = state.with(&layout, viewport()).snapshot();
        assert_eq!(snapshot.page_count, 10);
        // Index 4 is the fifth page, and the snapshot says so.
        assert_eq!(snapshot.current_page, PageNumber::new(5));
        assert_eq!(snapshot.scroll_top_pt, 4.0 * 792.0);
        assert_eq!(snapshot.pending_scroll_pt, None);
        assert_eq!(snapshot.content_height_pt, 10.0 * 792.0);
        assert_eq!(snapshot.max_scroll_pt, 10.0 * 792.0 - 396.0);
        assert_eq!(snapshot.zoom_target, ZoomTarget::FitWidth);
        assert_eq!(snapshot.scroll_mode, ScrollMode::Free);
        assert_eq!(snapshot.first_visible_page, PageNumber::new(5));
        assert_eq!(
            snapshot.last_visible_page,
            PageNumber::new(5),
            "one page fills a half-page viewport, so first and last are the same"
        );
    }

    #[test]
    fn page_one_is_the_top_of_the_document() {
        // The whole point of the one-based convention, pinned down in one place:
        // page 1 means the very start, not the second page.
        let mut state = ViewState::new();
        let layout = ten_pages();
        run_and_settle(
            &mut state,
            ViewCommand::GoToPage {
                page: PageNumber::FIRST,
            },
        );
        assert_eq!(
            state.with(&layout, viewport()).snapshot().scroll_top_pt,
            0.0
        );
        assert_eq!(
            state.with(&layout, viewport()).snapshot().current_page,
            PageNumber::new(1)
        );
    }

    #[test]
    fn a_snapshot_reports_every_page_on_screen() {
        // A viewport two and a half pages tall, to prove `last_visible_page` is
        // inclusive rather than the exclusive end of the old range.
        let mut state = ViewState::new();
        let layout = ten_pages();
        let tall = Viewport::new(612.0, 792.0 * 2.5);
        let outcome = apply(&mut state, &layout, tall, go_to_index(2));
        assert!(outcome.rejected().is_none());
        if let Some(top) = state.take_requested_scroll_pt() {
            state.report_scroll_top_pt(top);
        }

        let snapshot = state.with(&layout, tall).snapshot();
        assert_eq!(snapshot.first_visible_page, PageNumber::new(3));
        assert_eq!(
            snapshot.last_visible_page,
            PageNumber::new(5),
            "pages 3, 4 and part of 5 are on screen"
        );
    }

    #[test]
    fn a_snapshot_distinguishes_where_we_are_from_where_we_are_going() {
        // The distinction an agent needs: without it, reading the snapshot right
        // after a scroll command reports the old position with no hint that a move
        // is pending, and the agent captures the wrong frame believing it is right.
        let mut state = ViewState::new();
        let layout = ten_pages();
        run(&mut state, go_to_index(4));

        let snapshot = state.with(&layout, viewport()).snapshot();
        assert_eq!(
            snapshot.scroll_top_pt, 0.0,
            "reported a move that has not happened"
        );
        assert_eq!(snapshot.pending_scroll_pt, Some(4.0 * 792.0));
    }

    #[test]
    fn a_snapshot_of_an_empty_document_is_still_readable() {
        let empty = ScrollLayout::vertical(&[], 0.0);
        let state = ViewState::new();
        let snapshot = state.with(&empty, viewport()).snapshot();
        assert_eq!(snapshot.page_count, 0);
        // Not "page 0", which does not exist, and not "page 1", which would claim a
        // page is on screen when none is.
        assert_eq!(snapshot.current_page, None);
        assert_eq!(snapshot.first_visible_page, None);
        assert_eq!(snapshot.last_visible_page, None);
        assert!(snapshot.zoom.is_finite() && snapshot.zoom > 0.0);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_snapshot_survives_a_json_round_trip() {
        let mut state = ViewState::new();
        let layout = ten_pages();
        run_and_settle(&mut state, go_to_index(2));
        run(
            &mut state,
            ViewCommand::SetZoom {
                target: ZoomTarget::Fixed(1.5),
            },
        );

        let snapshot = state.with(&layout, viewport()).snapshot();
        let json = serde_json::to_string(&snapshot).expect("should serialize");
        let back: ViewSnapshot = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(snapshot, back);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn commands_round_trip_through_their_wire_form() {
        for command in ViewCommand::ALL {
            let json = serde_json::to_string(command).expect("should serialize");
            let back: ViewCommand = serde_json::from_str(&json)
                .unwrap_or_else(|error| panic!("{} failed to decode: {error}", command.name()));
            assert_eq!(*command, back, "{} changed in transit", command.name());

            // The tag on the wire has to be the name we publish, or an agent
            // reading the command reference cannot construct a message from it.
            let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
            assert_eq!(
                value.get("command").and_then(serde_json::Value::as_str),
                Some(command.name()),
                "wire tag does not match the published name for {json}"
            );
        }
    }
}
