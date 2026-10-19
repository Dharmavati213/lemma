//! A small least-recently-used cache bounded by both an entry count and a byte size (the preview
//! cache: `[preview] cache` entries, each a rendered image).

use std::collections::HashMap;
use std::hash::Hash;

/// Bounded by [`Lru::max_entries`] and [`Lru::max_bytes`]; the least recently used entries go
/// first. Lookups and inserts are O(1); an eviction scans the entries (there are few).
#[derive(Debug)]
pub struct Lru<K, V> {
    map: HashMap<K, Slot<V>>,
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    clock: u64,
}

#[derive(Debug)]
struct Slot<V> {
    value: V,
    bytes: usize,
    used: u64,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    /// An empty cache holding at most `max_entries` entries of `max_bytes` bytes in total
    /// (0 entries: nothing is kept).
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            max_entries,
            max_bytes,
            bytes: 0,
            clock: 0,
        }
    }

    /// The value for `key`, marked as just used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.clock += 1;
        let clock = self.clock;
        self.map.get_mut(key).map(|slot| {
            slot.used = clock;
            &slot.value
        })
    }

    /// True when `key` is cached (without marking it used).
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Adds (or replaces) `key`, then evicts the least recently used entries until the bounds
    /// hold; returns how many were evicted. A value larger than the whole byte bound is not kept.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) -> usize {
        self.remove(&key);
        if self.max_entries == 0 || bytes > self.max_bytes {
            return 0;
        }
        self.clock += 1;
        self.bytes += bytes;
        self.map.insert(
            key,
            Slot {
                value,
                bytes,
                used: self.clock,
            },
        );
        self.shrink()
    }

    /// Removes `key`.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.map.remove(key)?;
        self.bytes -= slot.bytes;
        Some(slot.value)
    }

    /// Keeps only the entries `keep` says yes to.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let mut freed = 0;
        self.map.retain(|k, slot| {
            let kept = keep(k, &slot.value);
            if !kept {
                freed += slot.bytes;
            }
            kept
        });
        self.bytes -= freed;
    }

    /// Changes the bounds, evicting what no longer fits; returns how many were evicted.
    pub fn set_bounds(&mut self, max_entries: usize, max_bytes: usize) -> usize {
        self.max_entries = max_entries;
        self.max_bytes = max_bytes;
        self.shrink()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    fn shrink(&mut self) -> usize {
        let mut evicted = 0;
        while self.map.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, slot)| slot.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&oldest);
            evicted += 1;
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut lru = Lru::new(3, 1000);
        lru.insert(1, "a", 10);
        lru.insert(2, "b", 10);
        lru.insert(3, "c", 10);
        assert_eq!(lru.get(&1), Some(&"a"));
        assert_eq!(lru.insert(4, "d", 10), 1);
        assert!(!lru.contains(&2), "2 was the oldest");
        assert!(lru.contains(&1) && lru.contains(&3) && lru.contains(&4));
        assert_eq!(lru.len(), 3);
        assert_eq!(lru.bytes(), 30);
    }

    #[test]
    fn bytes_are_bounded_too() {
        let mut lru = Lru::new(10, 100);
        lru.insert("a", 1, 60);
        lru.insert("b", 2, 30);
        assert_eq!(lru.insert("c", 3, 30), 1);
        assert!(!lru.contains(&"a"));
        assert_eq!(lru.bytes(), 60);
        assert_eq!(lru.insert("huge", 4, 101), 0);
        assert!(!lru.contains(&"huge"));
        assert_eq!(lru.insert("b", 5, 10), 0, "replacing frees the old size");
        assert_eq!(lru.bytes(), 40);
        assert_eq!(lru.get(&"b"), Some(&5));
    }

    #[test]
    fn bounds_change_and_zero_keeps_nothing() {
        let mut lru = Lru::new(4, 1000);
        for i in 0..4 {
            lru.insert(i, i, 1);
        }
        assert_eq!(lru.set_bounds(2, 1000), 2);
        assert_eq!(lru.len(), 2);
        assert!(lru.contains(&2) && lru.contains(&3));
        lru.set_bounds(0, 1000);
        assert!(lru.is_empty());
        lru.insert(9, 9, 1);
        assert!(lru.is_empty());
        lru.set_bounds(5, 1000);
        lru.insert(1, 1, 5);
        lru.insert(2, 2, 7);
        lru.retain(|k, _| *k == 2);
        assert_eq!((lru.len(), lru.bytes()), (1, 7));
        lru.clear();
        assert_eq!((lru.len(), lru.bytes()), (0, 0));
        assert_eq!((lru.max_entries(), lru.max_bytes()), (5, 1000));
    }

    proptest::proptest! {
        #[test]
        fn the_bounds_always_hold(ops in proptest::collection::vec((0u8..20, 0usize..50), 1..200),
                                  entries in 0usize..8, max_bytes in 0usize..120) {
            let mut lru = Lru::new(entries, max_bytes);
            for (key, size) in ops {
                lru.insert(key, (), size);
                proptest::prop_assert!(lru.len() <= entries);
                proptest::prop_assert!(lru.bytes() <= max_bytes);
            }
        }
    }
}
