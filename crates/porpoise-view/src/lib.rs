//! Viewport logic for a scrolling document view: where pages sit, and which of
//! them are worth rasterizing right now.
//!
//! Everything here is pure arithmetic over page geometry, with no windowing or
//! GPU dependency. That is deliberate. It makes the parts most likely to be
//! subtly wrong — scroll offsets across heterogeneous page sizes, visible-set
//! computation, cache eviction — unit-testable without a window, and it caps the
//! cost of swapping the shell to one crate. See `docs/goal-1-plan.md`, section 3.
//!
//! All coordinates are in PDF points (1/72 inch) unless named otherwise. Zoom is
//! applied when converting to physical pixels, not here.
//!
//! One module per question. They are private, and their contents re-exported
//! here, so the split is an internal organizing choice rather than part of the
//! API:
//!
//! | Module | Question |
//! |---|---|
//! | `layout` | Where does each page sit, and which are on screen? |
//! | `fit` | How big should everything be? |
//! | `request` | What should be rasterized next, in what order? |
//! | `zoom` | Which discrete zoom level are we rendering for? |
//! | `cache` | What stays in memory, and what gets dropped? |

mod cache;
mod fit;
mod layout;
mod request;
mod zoom;

pub use cache::{CacheKey, PageCache};
pub use fit::{FitMode, MAX_SCALE, MIN_SCALE, fit_scale};
pub use layout::ScrollLayout;
pub use request::request_order;
pub use zoom::ZoomBucket;
