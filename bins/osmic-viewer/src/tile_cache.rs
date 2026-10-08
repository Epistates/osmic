//! A bounded least-recently-used cache of loaded tiles.

use std::collections::HashSet;

use lru::LruCache;
use osmic_core::TileCoord;

/// Approximate memory held by a cached value, for the byte budget.
pub trait Weigh {
    fn weight(&self) -> usize;
}

/// LRU cache keyed by tile coordinate, bounded by entry count **and** by
/// total weight (bytes).
///
/// Reading with [`TileCache::get`] marks an entry most recently used, so a
/// frame that touches every visible tile keeps them all resident while
/// tiles that scrolled out of view age out first.
///
/// Tiles on screen can be pinned ([`TileCache::set_pinned`]): eviction
/// skips them, so a view needing more than the budget holds goes over
/// budget by what it shows instead of evicting and reloading its own tiles
/// over and over.
pub struct TileCache<V: Weigh> {
    entries: LruCache<TileCoord, V>,
    max_entries: usize,
    max_weight: usize,
    weight: usize,
    pinned: HashSet<TileCoord>,
}

impl<V: Weigh> TileCache<V> {
    /// A cache holding at most `max_entries` tiles and `max_weight` bytes
    /// (plus whatever is pinned).
    pub fn new(max_entries: usize, max_weight: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            max_entries: max_entries.max(1),
            max_weight,
            weight: 0,
            pinned: HashSet::new(),
        }
    }

    /// Replace the set of tiles eviction must keep.
    pub fn set_pinned(&mut self, pinned: impl IntoIterator<Item = TileCoord>) {
        self.pinned.clear();
        self.pinned.extend(pinned);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Total weight of the cached values.
    #[cfg(test)]
    pub fn weight(&self) -> usize {
        self.weight
    }

    /// Whether `coord` is cached (does not affect recency).
    pub fn contains(&self, coord: &TileCoord) -> bool {
        self.entries.contains(coord)
    }

    /// The cached value, marking it most recently used.
    pub fn get(&mut self, coord: &TileCoord) -> Option<&V> {
        self.entries.get(coord)
    }

    /// The cached value without affecting recency.
    pub fn peek(&self, coord: &TileCoord) -> Option<&V> {
        self.entries.peek(coord)
    }

    /// Insert (replacing any previous value) and evict least recently used
    /// entries until both budgets hold again. The newly inserted entry and
    /// pinned entries are never evicted. Returns the evicted entries,
    /// oldest first.
    pub fn insert(&mut self, coord: TileCoord, value: V) -> Vec<(TileCoord, V)> {
        let mut evicted = Vec::new();
        self.weight += value.weight();
        // The cache is unbounded underneath, so `push` only returns a
        // replaced value.
        if let Some((_, old)) = self.entries.push(coord, value) {
            self.weight -= old.weight();
        }
        while self.entries.len() > self.max_entries || self.weight > self.max_weight {
            // Oldest first; pinned tiles are on screen, so normally among
            // the most recent and rarely walked past.
            let victim = self
                .entries
                .iter()
                .rev()
                .map(|(k, _)| *k)
                .find(|k| *k != coord && !self.pinned.contains(k));
            let Some(old) = victim.and_then(|k| self.entries.pop(&k).map(|v| (k, v))) else {
                break; // only pinned entries (and the new one) are left
            };
            self.weight -= old.1.weight();
            evicted.push(old);
        }
        evicted
    }

    /// Cached coordinates from most to least recently used.
    #[cfg(test)]
    pub fn keys_by_recency(&self) -> Vec<TileCoord> {
        self.entries.iter().map(|(k, _)| *k).collect()
    }
}

#[cfg(test)]
mod tests {
    use osmic_core::Zoom;

    use super::*;

    struct Item(usize);

    impl Weigh for Item {
        fn weight(&self) -> usize {
            self.0
        }
    }

    fn t(x: u32) -> TileCoord {
        TileCoord::new(x, 0, Zoom::clamped(5))
    }

    fn keys(evicted: &[(TileCoord, Item)]) -> Vec<u32> {
        evicted.iter().map(|(k, _)| k.x).collect()
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut cache = TileCache::new(3, usize::MAX);
        for x in 1..=3 {
            assert!(cache.insert(t(x), Item(1)).is_empty());
        }
        // Touch 1: now the order is 1 (newest), 3, 2 (oldest).
        assert!(cache.get(&t(1)).is_some());
        assert_eq!(keys(&cache.insert(t(4), Item(1))), vec![2]);
        assert_eq!(keys(&cache.insert(t(5), Item(1))), vec![3]);
        assert_eq!(keys(&cache.insert(t(6), Item(1))), vec![1]);
        assert_eq!(
            cache
                .keys_by_recency()
                .iter()
                .map(|k| k.x)
                .collect::<Vec<_>>(),
            vec![6, 5, 4]
        );
    }

    #[test]
    fn peek_and_contains_do_not_refresh() {
        let mut cache = TileCache::new(2, usize::MAX);
        cache.insert(t(1), Item(1));
        cache.insert(t(2), Item(1));
        assert!(cache.contains(&t(1)));
        assert!(cache.peek(&t(1)).is_some());
        assert_eq!(
            keys(&cache.insert(t(3), Item(1))),
            vec![1],
            "1 was never refreshed"
        );
    }

    #[test]
    fn byte_budget_evicts_in_lru_order() {
        let mut cache = TileCache::new(100, 10);
        cache.insert(t(1), Item(4));
        cache.insert(t(2), Item(4));
        assert_eq!(cache.weight(), 8);
        cache.get(&t(1));
        let evicted = cache.insert(t(3), Item(4));
        assert_eq!(keys(&evicted), vec![2]);
        assert_eq!(cache.weight(), 8);
        // One big entry pushes out as many as needed, oldest first.
        let evicted = cache.insert(t(4), Item(9));
        assert_eq!(keys(&evicted), vec![1, 3]);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), 9);
    }

    #[test]
    fn an_oversized_entry_is_kept_alone() {
        let mut cache = TileCache::new(10, 5);
        cache.insert(t(1), Item(1));
        let evicted = cache.insert(t(2), Item(50));
        assert_eq!(keys(&evicted), vec![1]);
        assert!(cache.contains(&t(2)), "the entry just inserted survives");
    }

    #[test]
    fn pinned_tiles_survive_both_budgets() {
        let mut cache = TileCache::new(3, 10);
        cache.insert(t(1), Item(4));
        cache.insert(t(2), Item(4));
        // 1 and 2 are on screen; 1 is the least recently used.
        cache.set_pinned([t(1), t(2)]);
        assert_eq!(keys(&cache.insert(t(3), Item(1))), Vec::<u32>::new());
        // Over the byte budget: only unpinned 3 may go.
        assert_eq!(keys(&cache.insert(t(4), Item(4))), vec![3]);
        assert_eq!(cache.weight(), 12, "over budget by what is pinned");
        // Over the entry budget too, with everything else pinned: nothing
        // can be evicted.
        cache.set_pinned([t(1), t(2), t(4)]);
        assert!(cache.insert(t(5), Item(1)).is_empty());
        assert_eq!(cache.len(), 4);
        // Once unpinned, the next insert trims back within both budgets,
        // oldest first.
        cache.set_pinned([]);
        assert_eq!(keys(&cache.insert(t(6), Item(1))), vec![1, 2]);
        assert_eq!((cache.len(), cache.weight()), (3, 6));
    }

    #[test]
    fn replacing_an_entry_updates_the_weight() {
        let mut cache = TileCache::new(10, usize::MAX);
        cache.insert(t(1), Item(5));
        let evicted = cache.insert(t(1), Item(2));
        assert!(evicted.is_empty());
        assert_eq!(cache.weight(), 2);
        assert_eq!(cache.len(), 1);
    }
}
