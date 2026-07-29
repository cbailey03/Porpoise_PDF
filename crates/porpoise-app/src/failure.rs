//! When a page fails to rasterize, and whether to try again.
//!
//! Its own module because it is pure policy: no window, no cache, no egui. The whole
//! question it answers is whether a given [`RenderError`] is worth another worker's
//! time, and that is decidable from the error alone.

use porpoise_render::RenderError;

/// Extra attempts a page gets after a timeout, beyond the first.
///
/// Three attempts total, costing at most three job timeouts of one worker. Bounded
/// because the failure might be the machine and might be the page, and we cannot tell
/// which from here.
pub(crate) const MAX_RENDER_RETRIES: u8 = 2;

/// A rasterization that failed, and whether it is worth another attempt.
pub(crate) struct Failure {
    /// The renderer's own message, shown on the error tile.
    pub(crate) message: String,
    /// Attempts remaining. Zero means we have given up on this rasterization.
    pub(crate) retries_left: u8,
}

impl Failure {
    /// The failure to record for `error`, carrying over whatever retries an earlier
    /// attempt at the same rasterization had left.
    ///
    /// A timeout usually means the machine was momentarily busy rather than that this
    /// page is unrenderable, so it earns another attempt. Every other failure is
    /// deterministic — the index is out of range, the size is refused, or the
    /// interpreter panicked — and retrying one only burns a worker to arrive at the
    /// same answer.
    pub(crate) fn from_error(error: &RenderError, previous: Option<&Self>) -> Self {
        let retries_left = if matches!(error, RenderError::TimedOut { .. }) {
            previous.map_or(MAX_RENDER_RETRIES, |failure| failure.retries_left)
        } else {
            0
        };
        Self {
            message: error.to_string(),
            retries_left,
        }
    }

    /// Spends one retry, reporting whether there was one to spend.
    pub(crate) fn take_retry(&mut self) -> bool {
        if self.retries_left == 0 {
            return false;
        }
        self.retries_left -= 1;
        true
    }

    /// Whether this rasterization has been abandoned.
    pub(crate) fn gave_up(&self) -> bool {
        self.retries_left == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed_out() -> RenderError {
        RenderError::TimedOut {
            index: 3,
            timeout_ms: 5_000,
        }
    }

    fn panicked() -> RenderError {
        RenderError::Panicked { index: 3 }
    }

    #[test]
    fn a_timeout_starts_with_a_retry_budget() {
        let failure = Failure::from_error(&timed_out(), None);
        assert_eq!(failure.retries_left, MAX_RENDER_RETRIES);
        assert!(
            !failure.gave_up(),
            "a first timeout must not abandon the page"
        );
    }

    #[test]
    fn a_deterministic_failure_is_not_retried() {
        // Retrying a panic, a refused size, or a bad index only burns a worker to
        // reach the same answer.
        for error in [
            panicked(),
            RenderError::NoSuchPage { index: 3, count: 1 },
            RenderError::AreaTooLarge {
                index: 3,
                width: 60_000,
                height: 30_000,
                total_pixels: 1_800_000_000,
                max_total_pixels: 1 << 20,
            },
        ] {
            let failure = Failure::from_error(&error, None);
            assert!(failure.gave_up(), "{error:?} should not be retried");
        }
    }

    #[test]
    fn repeated_timeouts_exhaust_the_budget_and_then_give_up() {
        // The exact loop the viewer runs: request spends a retry, the render fails,
        // the new failure carries the reduced budget forward.
        let mut failure = Failure::from_error(&timed_out(), None);
        let mut attempts = 1;

        while failure.take_retry() {
            attempts += 1;
            failure = Failure::from_error(&timed_out(), Some(&failure));
        }

        assert_eq!(
            attempts,
            usize::from(MAX_RENDER_RETRIES) + 1,
            "expected one initial attempt plus {MAX_RENDER_RETRIES} retries"
        );
        assert!(failure.gave_up());
        assert!(
            !failure.take_retry(),
            "an exhausted failure must stay exhausted"
        );
    }

    #[test]
    fn a_timeout_that_later_panics_stops_being_retried() {
        // The budget must not survive a change of failure kind: if the page turns out
        // to panic, retrying it is pointless however many timeouts preceded it.
        let first = Failure::from_error(&timed_out(), None);
        let second = Failure::from_error(&panicked(), Some(&first));
        assert!(second.gave_up());
    }

    #[test]
    fn the_failure_message_is_the_renderers_own() {
        // It is shown on the error tile, so it has to say which failure this was.
        let failure = Failure::from_error(&timed_out(), None);
        assert!(
            failure.message.contains("5000 ms"),
            "unhelpful message: {}",
            failure.message
        );
    }
}
