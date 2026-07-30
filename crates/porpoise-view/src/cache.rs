//! A byte-budgeted, least-recently-used cache of rendered pages.
//!
//! Generic over the stored value so this crate needs no GPU or windowing
//! dependency and the eviction logic can be tested with a dummy. The viewer
//! stores texture handles; a test stores a `u32`.
//!
//! Memory has to track the *viewport*, not the document, or a 400-page file
//! eventually holds 400 rasterized pages. The budget here is what enforces that,
//! and it is a byte budget rather than an entry count because pages differ wildly
//! in size — one tabloid page at high zoom can outweigh twenty small ones.

use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;

use crate::ZoomBucket;

/// Identifies one rasterization: a page at a particular zoom rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// Zero-based page index.
    pub page: usize,
    /// The zoom rung it was rasterized for.
    pub bucket: ZoomBucket,
}

impl CacheKey {
    /// Convenience constructor.
    #[must_use]
    pub fn new(page: usize, bucket: ZoomBucket) -> Self {
        Self { page, bucket }
    }
}

struct Entry<T> {
    value: T,
    bytes: usize,
    /// Logical time of last access, for LRU ordering.
    touched: u64,
}

/// A cache of rendered pages bounded by total bytes.
pub struct PageCache<T> {
    entries: HashMap<CacheKey, Entry<T>>,
    budget_bytes: usize,
    used_bytes: usize,
    /// Monotonic counter standing in for a clock. Cheaper and more deterministic
    /// than real time, and immune to the clock going backwards.
    clock: u64,
}

impl<T> PageCache<T> {
    /// A cache holding at most `budget_bytes` of rendered pages.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget_bytes,
            used_bytes: 0,
            clock: 0,
        }
    }

    /// Total byte budget.
    #[must_use]
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Bytes currently held.
    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Number of cached rasterizations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether this exact rasterization is cached, without touching LRU order.
    #[must_use]
    pub fn contains(&self, key: CacheKey) -> bool {
        self.entries.contains_key(&key)
    }

    /// Fetches a rasterization, marking it recently used.
    pub fn get(&mut self, key: CacheKey) -> Option<&T> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let entry = self.entries.get_mut(&key)?;
        entry.touched = clock;
        Some(&entry.value)
    }

    /// Any cached rasterization of `page`, preferring the closest zoom to
    /// `desired`.
    ///
    /// This is what stops a zoom change from flashing grey: the previous rung's
    /// texture is drawn, slightly the wrong resolution, until the right one
    /// arrives. Does not affect LRU order, because it is a fallback rather than a
    /// real use.
    #[must_use]
    pub fn best_for_page(&self, page: usize, desired: ZoomBucket) -> Option<(CacheKey, &T)> {
        self.entries
            .iter()
            .filter(|(key, _)| key.page == page)
            .min_by_key(|(key, _)| key.bucket.rung().abs_diff(desired.rung()))
            .map(|(key, entry)| (*key, &entry.value))
    }

    /// Inserts a rasterization, evicting least-recently-used entries until the
    /// budget is satisfied.
    ///
    /// An entry larger than the whole budget is still stored — refusing it would
    /// mean a page that can never be displayed — but everything else is evicted
    /// first. Callers wanting to avoid that should check the size against
    /// [`Self::budget_bytes`] beforehand.
    pub fn insert(&mut self, key: CacheKey, value: T, bytes: usize) {
        self.clock = self.clock.wrapping_add(1);

        // Replacing an existing key: drop the old weight first.
        if let Some(previous) = self.entries.remove(&key) {
            self.used_bytes = self.used_bytes.saturating_sub(previous.bytes);
        }

        self.entries.insert(
            key,
            Entry {
                value,
                bytes,
                touched: self.clock,
            },
        );
        self.used_bytes = self.used_bytes.saturating_add(bytes);

        self.evict_to_budget(key);
    }

    /// Drops everything whose page falls outside `keep`.
    ///
    /// The byte budget alone would eventually do this, but only after the cache
    /// filled up. Dropping by position keeps memory proportional to the viewport
    /// even when the budget is generous.
    pub fn retain_pages(&mut self, keep: impl Fn(usize) -> bool) {
        let mut freed = 0_usize;
        self.entries.retain(|key, entry| {
            let kept = keep(key.page);
            if !kept {
                freed = freed.saturating_add(entry.bytes);
            }
            kept
        });
        self.used_bytes = self.used_bytes.saturating_sub(freed);
    }

    // `retain_bucket` used to live here: it dropped every rung of a page except one, and its
    // doc said it was called once the wanted rung arrived. Nothing ever called it, and the
    // policy it encoded is now actively wrong — the thumbnail grid needs a page to hold two
    // rungs at once, its own and the main view's. Removed rather than left as a trap with a
    // comment claiming it was in use. The byte budget and `retain_pages` were doing this job
    // all along.

    /// Removes everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used_bytes = 0;
    }

    /// Evicts least-recently-used entries until within budget, never dropping
    /// `protect`.
    fn evict_to_budget(&mut self, protect: CacheKey) {
        while self.used_bytes > self.budget_bytes {
            let victim = self
                .entries
                .iter()
                .filter(|(key, _)| **key != protect)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key);

            let Some(victim) = victim else {
                // Only the protected entry is left, and it is over budget by
                // itself. Keep it: a page we cannot show is worse than a
                // temporarily oversized cache.
                break;
            };

            if let MapEntry::Occupied(occupied) = self.entries.entry(victim) {
                let bytes = occupied.get().bytes;
                occupied.remove();
                self.used_bytes = self.used_bytes.saturating_sub(bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: usize, zoom: f32) -> CacheKey {
        CacheKey::new(page, ZoomBucket::enclosing(zoom))
    }

    #[test]
    fn an_inserted_entry_can_be_read_back() {
        let mut cache: PageCache<u32> = PageCache::new(1000);
        cache.insert(key(0, 1.0), 42, 100);
        assert_eq!(cache.get(key(0, 1.0)), Some(&42));
        assert_eq!(cache.used_bytes(), 100);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn the_byte_budget_is_respected() {
        let mut cache: PageCache<u32> = PageCache::new(250);
        for page in 0..5 {
            cache.insert(key(page, 1.0), page as u32, 100);
        }
        assert!(
            cache.used_bytes() <= 250,
            "used {} over a 250 budget",
            cache.used_bytes()
        );
        assert_eq!(cache.len(), 2, "250 bytes holds two 100-byte pages");
    }

    #[test]
    fn eviction_drops_the_least_recently_used() {
        let mut cache: PageCache<u32> = PageCache::new(250);
        cache.insert(key(0, 1.0), 0, 100);
        cache.insert(key(1, 1.0), 1, 100);

        // Touch page 0 so page 1 becomes the oldest.
        assert_eq!(cache.get(key(0, 1.0)), Some(&0));

        cache.insert(key(2, 1.0), 2, 100);

        assert!(
            cache.contains(key(0, 1.0)),
            "recently used page was evicted"
        );
        assert!(!cache.contains(key(1, 1.0)), "stale page survived");
        assert!(cache.contains(key(2, 1.0)));
    }

    #[test]
    fn an_entry_larger_than_the_budget_is_still_stored() {
        // Refusing it would mean a page that can never be displayed at all.
        let mut cache: PageCache<u32> = PageCache::new(100);
        cache.insert(key(0, 1.0), 0, 100);
        cache.insert(key(1, 1.0), 1, 5_000);

        assert!(
            cache.contains(key(1, 1.0)),
            "the oversized page was dropped"
        );
        assert!(
            !cache.contains(key(0, 1.0)),
            "the small page was not evicted"
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn replacing_a_key_does_not_double_count_its_bytes() {
        let mut cache: PageCache<u32> = PageCache::new(1000);
        cache.insert(key(0, 1.0), 0, 100);
        cache.insert(key(0, 1.0), 1, 300);
        assert_eq!(cache.used_bytes(), 300);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(key(0, 1.0)), Some(&1));
    }

    #[test]
    fn the_same_page_at_two_zooms_is_two_entries() {
        let mut cache: PageCache<u32> = PageCache::new(1000);
        cache.insert(key(0, 1.0), 10, 100);
        cache.insert(key(0, 4.0), 40, 100);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.used_bytes(), 200);
    }

    #[test]
    fn best_for_page_prefers_the_closest_zoom() {
        let mut cache: PageCache<u32> = PageCache::new(10_000);
        cache.insert(key(0, 0.5), 5, 10);
        cache.insert(key(0, 4.0), 40, 10);

        // Wanting 3.5x should reuse the 4.0x render, not the 0.5x one.
        let (found, value) = cache
            .best_for_page(0, ZoomBucket::enclosing(3.5))
            .expect("some bucket for page 0");
        assert_eq!(*value, 40);
        assert_eq!(found.bucket, ZoomBucket::enclosing(4.0));
    }

    #[test]
    fn best_for_page_ignores_other_pages() {
        let mut cache: PageCache<u32> = PageCache::new(10_000);
        cache.insert(key(7, 1.0), 70, 10);
        assert!(cache.best_for_page(0, ZoomBucket::enclosing(1.0)).is_none());
        assert!(cache.best_for_page(7, ZoomBucket::enclosing(1.0)).is_some());
    }

    #[test]
    fn retain_pages_drops_outside_the_window_and_frees_bytes() {
        let mut cache: PageCache<u32> = PageCache::new(10_000);
        for page in 0..10 {
            cache.insert(key(page, 1.0), page as u32, 100);
        }
        assert_eq!(cache.used_bytes(), 1000);

        cache.retain_pages(|page| (4..7).contains(&page));

        assert_eq!(cache.len(), 3);
        assert_eq!(cache.used_bytes(), 300);
        assert!(cache.contains(key(5, 1.0)));
        assert!(!cache.contains(key(0, 1.0)));
    }

    #[test]
    fn one_page_can_hold_several_rungs_at_once() {
        // The thumbnail grid depends on this: it draws a page at a tiny rung while the
        // main view holds the same page at reading size, and neither may evict the
        // other. This replaces a `retain_bucket` that dropped every rung but one — it
        // was never called, and the policy it encoded is now the opposite of what is
        // wanted.
        let mut cache: PageCache<u32> = PageCache::new(10_000);
        cache.insert(key(0, 0.1), 5, 100);
        cache.insert(key(0, 2.0), 20, 100);

        assert!(
            cache.contains(key(0, 0.1)),
            "the thumbnail rung was dropped"
        );
        assert!(cache.contains(key(0, 2.0)), "the reading rung was dropped");
        assert_eq!(cache.used_bytes(), 200);

        // And the byte budget, not a per-page rule, is what bounds it.
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn clear_resets_byte_accounting() {
        let mut cache: PageCache<u32> = PageCache::new(1000);
        cache.insert(key(0, 1.0), 0, 400);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.used_bytes(), 0);
    }

    #[test]
    fn a_zero_budget_still_keeps_the_page_just_inserted() {
        // Otherwise nothing would ever display.
        let mut cache: PageCache<u32> = PageCache::new(0);
        cache.insert(key(0, 1.0), 7, 100);
        assert_eq!(cache.get(key(0, 1.0)), Some(&7));
        assert_eq!(cache.len(), 1);
    }
}
