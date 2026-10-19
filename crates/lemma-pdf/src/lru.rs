//! A least-recently-used cache bounded by the total byte size of its values.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

/// LRU cache bounded by bytes (48 MiB by default for page textures).
///
/// Every value is inserted with its size in bytes, and the sum of the sizes never exceeds the cap after
/// [`insert`](Self::insert) or [`shrink_to`](Self::shrink_to) returns. `get` and `insert` mark an entry as
/// most recently used; eviction removes the least recently used entries first. All operations are
/// O(log n).
#[derive(Debug)]
pub struct ByteLru<K, V> {
    cap: usize,
    used: usize,
    /// Monotonic use counter; the entry with the smallest tick is the least recently used.
    tick: u64,
    entries: HashMap<K, Entry<V>>,
    /// tick → key, ordered from least to most recently used.
    order: BTreeMap<u64, K>,
}

#[derive(Debug)]
struct Entry<V> {
    value: V,
    bytes: usize,
    tick: u64,
}

impl<K: Eq + Hash + Clone, V> ByteLru<K, V> {
    /// Creates an empty cache holding at most `cap_bytes` bytes.
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            cap: cap_bytes,
            used: 0,
            tick: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    /// Inserts `v` under `k` as the most recently used entry, replacing any previous value for `k`, then
    /// evicts least recently used entries until the cache fits its cap.
    ///
    /// Returns the evicted keys, least recently used first. An entry larger than the whole cap is evicted
    /// immediately, so `k` itself is then the last key returned.
    pub fn insert(&mut self, k: K, v: V, bytes: usize) -> Vec<K> {
        if let Some(old) = self.entries.remove(&k) {
            self.order.remove(&old.tick);
            self.used -= old.bytes;
        }
        let tick = self.next_tick();
        self.order.insert(tick, k.clone());
        self.entries.insert(
            k,
            Entry {
                value: v,
                bytes,
                tick,
            },
        );
        self.used += bytes;
        self.evict_to(self.cap)
    }

    /// Returns the value for `k` and marks it as most recently used.
    pub fn get(&mut self, k: &K) -> Option<&V> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(k)?;
        let key = self
            .order
            .remove(&entry.tick)
            .expect("every entry has an order slot");
        self.order.insert(tick, key);
        entry.tick = tick;
        Some(&entry.value)
    }

    /// Keeps only the entries whose key satisfies `f` (e.g. drops textures of stale generations).
    pub fn retain(&mut self, mut f: impl FnMut(&K) -> bool) {
        let used = &mut self.used;
        let order = &mut self.order;
        self.entries.retain(|k, e| {
            let keep = f(k);
            if !keep {
                *used -= e.bytes;
                order.remove(&e.tick);
            }
            keep
        });
    }

    /// Evicts least recently used entries until at most `cap_bytes` are in use. The configured cap is
    /// unchanged, so the cache may grow back to it later (used when the PDF pane is hidden).
    pub fn shrink_to(&mut self, cap_bytes: usize) {
        self.evict_to(cap_bytes);
    }

    /// Changes the cap to `cap_bytes`, evicting least recently used entries until the cache fits it.
    /// Returns the evicted keys, least recently used first.
    pub fn set_cap(&mut self, cap_bytes: usize) -> Vec<K> {
        self.cap = cap_bytes;
        self.evict_to(cap_bytes)
    }

    /// Total bytes of all entries.
    pub fn used(&self) -> usize {
        self.used
    }

    /// The configured cap in bytes.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn evict_to(&mut self, cap: usize) -> Vec<K> {
        let mut evicted = Vec::new();
        while self.used > cap {
            let Some((_, key)) = self.order.pop_first() else {
                break;
            };
            if let Some(e) = self.entries.remove(&key) {
                self.used -= e.bytes;
            }
            evicted.push(key);
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn evicts_in_lru_order() {
        let mut lru = ByteLru::new(30);
        assert!(lru.insert("a", 1, 10).is_empty());
        assert!(lru.insert("b", 2, 10).is_empty());
        assert!(lru.insert("c", 3, 10).is_empty());
        assert_eq!(lru.used(), 30);
        assert_eq!(lru.get(&"a"), Some(&1));
        assert_eq!(lru.insert("d", 4, 10), vec!["b"]);
        assert_eq!(lru.insert("e", 5, 15), vec!["c", "a"]);
        assert_eq!(lru.used(), 25);
        assert_eq!(lru.get(&"b"), None);
        assert_eq!(lru.get(&"d"), Some(&4));
        assert_eq!(lru.len(), 2);
    }

    #[test]
    fn replacing_a_key_updates_bytes_and_recency() {
        let mut lru = ByteLru::new(30);
        lru.insert(1, "x", 10);
        lru.insert(2, "y", 10);
        assert!(lru.insert(1, "x2", 15).is_empty());
        assert_eq!(lru.used(), 25);
        assert_eq!(lru.insert(3, "z", 10), vec![2]);
        assert_eq!(lru.get(&1), Some(&"x2"));
    }

    #[test]
    fn set_cap_evicts_down_to_the_new_cap() {
        let mut lru = ByteLru::new(30);
        lru.insert(1, (), 10);
        lru.insert(2, (), 10);
        lru.insert(3, (), 10);
        lru.get(&1);
        assert_eq!(lru.set_cap(15), vec![2, 3]);
        assert_eq!((lru.cap(), lru.used()), (15, 10));
        assert!(lru.set_cap(40).is_empty());
        assert!(
            lru.insert(4, (), 25).is_empty(),
            "the raised cap holds more"
        );
        assert_eq!(lru.used(), 35);
    }

    #[test]
    fn oversized_entry_is_evicted_immediately() {
        let mut lru = ByteLru::new(10);
        lru.insert(1, (), 5);
        assert_eq!(lru.insert(2, (), 11), vec![1, 2]);
        assert!(lru.is_empty());
        assert_eq!(lru.used(), 0);
    }

    #[test]
    fn retain_and_shrink() {
        let mut lru = ByteLru::new(100);
        for k in 0..10 {
            lru.insert(k, k, 10);
        }
        lru.retain(|k| k % 2 == 0);
        assert_eq!(lru.len(), 5);
        assert_eq!(lru.used(), 50);
        lru.get(&0);
        lru.shrink_to(20);
        assert_eq!(lru.used(), 20);
        assert_eq!(lru.cap(), 100);
        assert!(lru.get(&0).is_some());
        assert!(lru.get(&8).is_some());
        assert!(lru.get(&2).is_none());
        // The cap is unchanged, so the cache can grow back.
        assert!(lru.insert(20, 20, 80).is_empty());
        assert_eq!(lru.used(), 100);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(u8, usize),
        Get(u8),
        Retain(u8),
        Shrink(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0u8..16, 0usize..40).prop_map(|(k, b)| Op::Insert(k, b)),
            3 => (0u8..16).prop_map(Op::Get),
            1 => (1u8..5).prop_map(Op::Retain),
            1 => (0usize..100).prop_map(Op::Shrink),
        ]
    }

    proptest! {
        /// Compares against a naive model: a Vec ordered from least to most recently used.
        #[test]
        fn matches_model_and_never_exceeds_cap(cap in 0usize..100, ops in prop::collection::vec(op(), 1..200)) {
            let mut lru = ByteLru::new(cap);
            let mut model: Vec<(u8, usize)> = Vec::new();
            for op in ops {
                match op {
                    Op::Insert(k, b) => {
                        model.retain(|e| e.0 != k);
                        model.push((k, b));
                        let mut expected = Vec::new();
                        while model.iter().map(|e| e.1).sum::<usize>() > cap {
                            expected.push(model.remove(0).0);
                        }
                        prop_assert_eq!(lru.insert(k, b, b), expected);
                    }
                    Op::Get(k) => {
                        let expected = model.iter().position(|e| e.0 == k).map(|p| {
                            let e = model.remove(p);
                            model.push(e);
                            e.1
                        });
                        prop_assert_eq!(lru.get(&k).copied(), expected);
                    }
                    Op::Retain(m) => {
                        model.retain(|e| e.0 % m != 0);
                        lru.retain(|k| k % m != 0);
                    }
                    Op::Shrink(c) => {
                        while model.iter().map(|e| e.1).sum::<usize>() > c {
                            model.remove(0);
                        }
                        lru.shrink_to(c);
                    }
                }
                prop_assert!(lru.used() <= cap);
                prop_assert_eq!(lru.used(), model.iter().map(|e| e.1).sum::<usize>());
                prop_assert_eq!(lru.len(), model.len());
            }
        }
    }
}
