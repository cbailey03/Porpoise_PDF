//! Asking a worker for a page, and deciding whether it is worth asking.
//!
//! One decision with two callers — the scrolling page column and the thumbnail grid — and
//! it lives here because it had been written twice and the copies had diverged. The grid's
//! copy was missing the retry budget entirely, so a page that could not be rasterized was
//! re-requested from the grid every frame for as long as it stayed on screen, while the
//! column had given up on the same page after three attempts.
//!
//! Adding the missing check to the second copy would have fixed that symptom and left the
//! shape that produced it, so there is now one copy.
//!
//! # The scale belongs here too
//!
//! Not only the policy. A caller derives the rasterization scale from a zoom rung and tags
//! the job with that rung, and those two have to agree — a job scaled for one rung and
//! tagged with another caches its result under a size it was not drawn at, which shows up
//! as a blurry or oversized page rather than as an error. Deriving both from the one
//! [`CacheKey`] makes disagreeing impossible instead of unlikely.

use std::collections::HashMap;

use porpoise_render::RenderPool;
use porpoise_view::CacheKey;

use crate::failure::Failure;

/// Whether `key` is worth asking a worker for.
///
/// Read-only on purpose: the retry is spent by [`RenderQueue::want`] once a worker has
/// actually taken the job. Deciding and spending used to be a single step, which meant a
/// refused submission — a full queue, a poisoned lock — burned a retry on an attempt no
/// worker ever looked at.
///
/// Does **not** consult the cache. The caller has already had to look there, because it
/// needs to know whether it has a texture in order to paint one.
pub(crate) fn worth_asking_for(
    key: CacheKey,
    in_flight: &[CacheKey],
    failures: &HashMap<CacheKey, Failure>,
) -> bool {
    if in_flight.contains(&key) {
        return false;
    }
    // A page we have stopped trying to render stays stopped. Without this the grid
    // re-requests a hopeless page on every frame it is on screen, which is a worker pinned
    // to work whose answer is already known.
    !failures.get(&key).is_some_and(Failure::gave_up)
}

/// The pieces needed to ask a worker for a page.
///
/// Borrowed rather than owned so both callers can build one from the open document's own
/// fields; see the module docs for why they share it.
///
/// The fields are private, which is the point rather than tidiness: [`Self::want`] is then
/// the *only* way to reach the worker pool, so a caller cannot submit a render without the
/// policy — which is exactly what the thumbnail grid had been doing. A rule nobody can
/// bypass beats a rule everybody has to remember.
pub(crate) struct RenderQueue<'a> {
    pool: &'a RenderPool,
    /// Requests submitted and not yet returned, so a page is not queued twice.
    in_flight: &'a mut Vec<CacheKey>,
    /// Rasterizations that failed, with whatever retry budget each has left.
    failures: &'a mut HashMap<CacheKey, Failure>,
}

impl<'a> RenderQueue<'a> {
    /// Borrows the open document's render bookkeeping for one frame.
    pub(crate) fn new(
        pool: &'a RenderPool,
        in_flight: &'a mut Vec<CacheKey>,
        failures: &'a mut HashMap<CacheKey, Failure>,
    ) -> Self {
        Self {
            pool,
            in_flight,
            failures,
        }
    }

    /// Asks a worker to rasterize `key`, unless it is already queued or has been given up
    /// on.
    ///
    /// Reports whether a job was submitted, which is true only on the frame the work
    /// starts.
    pub(crate) fn want(&mut self, key: CacheKey, pixels_per_point: f32) -> bool {
        if !worth_asking_for(key, self.in_flight, self.failures) {
            return false;
        }
        // Physical pixels, not points: a thumbnail drawn at a fixed size in points needs
        // twice the pixels on a 2x screen or it looks soft.
        let scale = key.bucket.scale() * pixels_per_point;
        if !self
            .pool
            .submit(key.document, key.page, scale, i64::from(key.bucket.rung()))
        {
            return false;
        }
        self.in_flight.push(key);
        // Spent now that a worker has it, and only if this was a retry — a page's first
        // attempt has no failure to spend from.
        if let Some(failure) = self.failures.get_mut(&key) {
            failure.take_retry();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use porpoise_render::RenderError;
    use porpoise_view::ZoomBucket;

    use super::*;
    use crate::failure::MAX_RENDER_RETRIES;

    fn key(page: usize) -> CacheKey {
        CacheKey::new(0, page, ZoomBucket::enclosing(1.0))
    }

    fn timed_out() -> RenderError {
        RenderError::TimedOut {
            index: 3,
            timeout_ms: 5_000,
        }
    }

    #[test]
    fn a_page_nobody_has_asked_for_is_worth_asking_for() {
        assert!(worth_asking_for(key(3), &[], &HashMap::new()));
    }

    #[test]
    fn a_page_already_in_flight_is_not_asked_for_twice() {
        assert!(!worth_asking_for(key(3), &[key(3)], &HashMap::new()));
        assert!(
            worth_asking_for(key(4), &[key(3)], &HashMap::new()),
            "a different page must not be blocked by this one"
        );
    }

    #[test]
    fn a_page_at_another_rung_is_a_different_request() {
        // The grid and the column hold the same page at two sizes, so one being in flight
        // must not block the other — that would leave a panel with a permanent grey box.
        let thumbnail = CacheKey::new(0, 3, ZoomBucket::enclosing(0.1));
        assert!(worth_asking_for(thumbnail, &[key(3)], &HashMap::new()));
    }

    #[test]
    fn a_page_that_has_been_given_up_on_is_never_asked_for_again() {
        // The bug this module exists for. The grid had no budget at all, so a page that
        // could not be rasterized was submitted again on every frame it was on screen.
        let mut failures = HashMap::new();
        failures.insert(
            key(3),
            Failure::from_error(&RenderError::Panicked { index: 3 }, None),
        );
        assert!(!worth_asking_for(key(3), &[], &failures));
    }

    #[test]
    fn a_timeout_is_worth_asking_for_again_until_the_budget_runs_out() {
        // The loop the viewer runs, without a worker pool: ask, spend, fail, carry the
        // reduced budget forward. `failure.rs` owns the budget itself; this checks that
        // asking and spending compose into the attempt count it promises.
        let mut failures: HashMap<CacheKey, Failure> = HashMap::new();
        let mut attempts = 0;

        while worth_asking_for(key(3), &[], &failures) {
            attempts += 1;
            assert!(attempts <= 10, "the budget never ran out");
            // What `want` does once a worker takes the job.
            if let Some(failure) = failures.get_mut(&key(3)) {
                failure.take_retry();
            }
            // And what the render coming back failed does.
            let previous = failures.remove(&key(3));
            failures.insert(key(3), Failure::from_error(&timed_out(), previous.as_ref()));
        }

        assert_eq!(
            attempts,
            usize::from(MAX_RENDER_RETRIES) + 1,
            "expected one attempt plus {MAX_RENDER_RETRIES} retries"
        );
    }

    #[test]
    fn a_refused_submission_does_not_cost_a_retry() {
        // Deciding and spending used to be one step, so a full queue burned the budget on
        // an attempt no worker ever saw. Asking twice without a submission in between must
        // leave the budget where it was.
        let mut failures = HashMap::new();
        failures.insert(key(3), Failure::from_error(&timed_out(), None));

        for ask in 0..5 {
            assert!(
                worth_asking_for(key(3), &[], &failures),
                "ask {ask} was refused after {ask} submissions that never happened"
            );
        }
    }
}
