//! A stable label for one staged document.

use std::fmt;
use std::num::NonZeroUsize;

/// Which staged document a command means, counting from 1.
///
/// Deliberately not a `usize` index into anything: it never resizes a `Vec` or
/// looks up a slot, it is only ever compared for equality and shown to
/// whoever is looking. Assigned once, when a document is staged, and never
/// reused afterward — even once that stage is cleared — the same discipline
/// [`crate::viewer::OpenDocument::add_file`] and `porpoise_doc::PageOrder`'s
/// own document indices already hold to: staging the same path twice, or
/// staging a fresh one after clearing an old id, must never be mistaken for
/// "the same staged document as before." See `docs/goal-5-plan.md` §10.12.
///
/// Zero is unrepresentable for the same reason it is for
/// [`porpoise_view::PageNumber`]: the wire form is the bare number, so
/// `{"stage":0}` is refused by deserialization itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub(crate) struct StageId(NonZeroUsize);

impl StageId {
    /// The first document ever staged in a session.
    pub(crate) const FIRST: Self = Self(NonZeroUsize::MIN);

    /// Wraps a one-based number, or `None` if it is zero.
    pub(crate) const fn new(number: usize) -> Option<Self> {
        match NonZeroUsize::new(number) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }

    /// The id the next staged document should get — this one plus one, so a
    /// counter can hand out `StageId::FIRST`, then keep calling `next()` on
    /// what it last handed out.
    pub(crate) const fn next(self) -> Self {
        match NonZeroUsize::new(self.0.get().saturating_add(1)) {
            Some(next) => Self(next),
            // Unreachable: `saturating_add(1)` on a `NonZeroUsize` is never zero.
            None => self,
        }
    }
}

impl fmt::Display for StageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.get().fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_no_stage_zero() {
        assert_eq!(StageId::new(0), None);
    }

    #[test]
    fn next_counts_up_by_one() {
        let first = StageId::FIRST;
        let second = first.next();
        let third = second.next();
        assert_eq!(second.to_string(), "2");
        assert_eq!(third.to_string(), "3");
    }

    #[test]
    fn next_never_produces_the_same_id_twice() {
        let mut seen = std::collections::HashSet::new();
        let mut id = StageId::FIRST;
        for _ in 0..100 {
            assert!(seen.insert(id), "{id} was handed out twice");
            id = id.next();
        }
    }

    #[test]
    fn next_saturates_rather_than_wrapping_to_zero() {
        let Some(largest) = StageId::new(usize::MAX) else {
            unreachable!("usize::MAX is not zero")
        };
        assert_eq!(largest.next(), largest, "wrapped instead of saturating");
    }

    #[test]
    fn it_displays_as_the_number_a_person_expects() {
        assert_eq!(StageId::FIRST.to_string(), "1");
        assert_eq!(StageId::FIRST.next().to_string(), "2");
    }

    #[test]
    fn the_wire_form_is_the_bare_number() {
        let json = serde_json::to_string(&StageId::FIRST.next()).expect("serialize");
        assert_eq!(json, "2");
    }
}
