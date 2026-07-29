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
//! | `command` | What can be asked of the view? |
//! | `state` | What is the view's state, and how does a command change it? |
//! | `page` | What page is that, in the numbering a person uses? |
//!
//! # Page numbers versus page indices
//!
//! Anything a person or an agent reads or writes is a [`PageNumber`], where the first
//! page is 1. Anything that indexes a collection — `ScrollLayout`, the cache, the
//! renderer — is a zero-based `usize`. [`PageNumber::index`] and
//! [`PageNumber::from_index`] are the only way across, so an off-by-one cannot happen
//! quietly.
//!
//! Screen units are a third thing again, and [`Viewport`] documents that trap.
//!
//! The last two are Goal 2's foundation: every effect the viewer can produce is a
//! [`ViewCommand`], and [`apply`] is the only thing that changes [`ViewState`].
//! See `docs/goal-2-plan.md`.
//!
//! # Features
//!
//! `serde` derives `Serialize`/`Deserialize` for the command and snapshot types,
//! so they can cross a process boundary. Off by default: the wire format is the
//! shell's concern, and this crate is worth keeping dependency-light.

mod cache;
mod command;
mod fit;
mod layout;
mod page;
mod request;
mod state;
mod zoom;

pub use cache::{CacheKey, PageCache};
pub use command::{Outcome, Rejection, ViewCommand, ZoomTarget};
pub use fit::{FitMode, MAX_SCALE, MIN_SCALE, fit_scale};
pub use layout::{PAGE_GAP_PT, ScrollLayout};
pub use page::PageNumber;
pub use request::request_order;
pub use state::{ScrollMode, View, ViewSnapshot, ViewState, Viewport, apply};
pub use zoom::ZoomBucket;
