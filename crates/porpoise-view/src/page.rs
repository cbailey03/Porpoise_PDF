//! Page numbers as people write them.

use std::fmt;
use std::num::NonZeroUsize;

/// A page number as people write them: the first page is 1.
///
/// Deliberately a distinct type from the zero-based `usize` indices used by
/// [`crate::ScrollLayout`], the page cache and the renderer. The two differ by
/// exactly one and are otherwise identical, which is the kind of confusion that
/// never crashes — it quietly shows the wrong page. Both conventions had already
/// grown up side by side here: the CLI's `--start-page` and `render --page` counted
/// from 1, while the control protocol's `go_to_page` counted from 0.
///
/// The rule this type enforces: anything a person or an agent reads or writes is a
/// `PageNumber`; anything that indexes a collection is a plain `usize`. Converting
/// between them has to be spelled out, so it cannot happen by accident.
///
/// Zero is unrepresentable, which is not just tidiness. The wire form is the bare
/// number, so `{"page":0}` is refused by deserialization itself rather than by a
/// hand-written check somebody has to remember to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct PageNumber(NonZeroUsize);

impl PageNumber {
    /// The first page of any document.
    pub const FIRST: Self = Self(NonZeroUsize::MIN);

    /// Wraps a one-based number, or `None` if it is zero.
    #[must_use]
    pub const fn new(number: usize) -> Option<Self> {
        match NonZeroUsize::new(number) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }

    /// Numbers a zero-based index.
    ///
    /// Saturating, so a `usize::MAX` index cannot wrap to page zero. A document
    /// with that many pages is not reachable, but the invariant holds regardless of
    /// what reaches it.
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        match NonZeroUsize::new(index.saturating_add(1)) {
            Some(number) => Self(number),
            // Unreachable: `saturating_add(1)` is never zero.
            None => Self::FIRST,
        }
    }

    /// The one-based number, for showing someone.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }

    /// The zero-based index, for indexing a collection.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0.get() - 1
    }
}

impl fmt::Display for PageNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.get().fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_one_is_index_zero() {
        let first = PageNumber::new(1).unwrap();
        assert_eq!(first.get(), 1);
        assert_eq!(first.index(), 0);
        assert_eq!(first, PageNumber::FIRST);
    }

    #[test]
    fn there_is_no_page_zero() {
        assert_eq!(PageNumber::new(0), None);
    }

    #[test]
    fn numbering_an_index_and_indexing_a_number_round_trip() {
        for index in [0, 1, 2, 41, 399, 100_000] {
            assert_eq!(PageNumber::from_index(index).index(), index);
            assert_eq!(PageNumber::from_index(index).get(), index + 1);
        }
    }

    #[test]
    fn numbering_the_largest_index_cannot_wrap_to_zero() {
        // A page number of zero is unrepresentable, so the only way to produce one
        // would be an overflow. Saturating means the type's invariant does not
        // depend on the caller being sensible.
        assert_eq!(PageNumber::from_index(usize::MAX).get(), usize::MAX);
    }

    #[test]
    fn it_displays_as_the_number_a_person_expects() {
        assert_eq!(PageNumber::from_index(0).to_string(), "1");
        assert_eq!(PageNumber::from_index(49).to_string(), "50");
    }

    #[test]
    fn ordering_follows_the_page_order() {
        assert!(PageNumber::from_index(0) < PageNumber::from_index(1));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn the_wire_form_is_the_bare_number() {
        let json = serde_json::to_string(&PageNumber::from_index(49)).expect("serialize");
        assert_eq!(json, "50");

        let back: PageNumber = serde_json::from_str("50").expect("deserialize");
        assert_eq!(back, PageNumber::from_index(49));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn page_zero_is_refused_by_deserialization_itself() {
        // The point of the newtype: no hand-written guard to forget.
        serde_json::from_str::<PageNumber>("0").expect_err("page 0 should not deserialize");
    }
}
