//! The one in-memory byte-bounded LRU of the app: a `HashMap` for lookup and a
//! `BTreeMap` from use tick to key, so touching and evicting are O(log n).
//! Callers pass each entry's weight; the cache never looks inside values.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

pub struct ByteLru<K, V> {
    entries: HashMap<K, (V, usize, u64)>,
    order: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
    max_bytes: usize,
    max_items: usize,
}

/// A cache that holds nothing: a placeholder for views that draw without one.
impl<K: Clone + Eq + Hash, V> Default for ByteLru<K, V> {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl<K: Clone + Eq + Hash, V> ByteLru<K, V> {
    pub fn new(max_bytes: usize, max_items: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            max_bytes,
            max_items,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Reads without changing the recency (for drawing from `&self`).
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(value, ..)| value)
    }

    /// Reads and marks the entry most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.touch(key);
        self.peek(key)
    }

    /// Marks the entry most recently used; false when it is absent.
    pub fn touch(&mut self, key: &K) -> bool {
        let Some((_, _, tick)) = self.entries.get_mut(key) else {
            return false;
        };
        self.order.remove(tick);
        self.tick += 1;
        *tick = self.tick;
        self.order.insert(self.tick, key.clone());
        true
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (value, size, tick) = self.entries.remove(key)?;
        self.order.remove(&tick);
        self.bytes -= size;
        Some(value)
    }

    /// Inserts or replaces `key` as the most recently used, evicting the least
    /// recently used until it fits. `None` (nothing changes) when `size`
    /// alone exceeds the byte limit; otherwise how many *other* entries were
    /// evicted.
    pub fn insert(&mut self, key: K, value: V, size: usize) -> Option<usize> {
        if size > self.max_bytes || self.max_items == 0 {
            return None;
        }
        self.remove(&key);
        let mut evicted = 0;
        while self.bytes + size > self.max_bytes || self.entries.len() >= self.max_items {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((_, old_size, _)) = self.entries.remove(&oldest) {
                self.bytes -= old_size;
                evicted += 1;
            }
        }
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.entries.insert(key, (value, size, self.tick));
        self.bytes += size;
        Some(evicted)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_and_get_refreshes() {
        let mut lru = ByteLru::new(30, 100);
        for n in 0..3u8 {
            lru.insert(n, (), 10);
        }
        assert!(lru.get(&0).is_some());
        assert_eq!(lru.insert(3, (), 10), Some(1));
        assert!(lru.contains_key(&0) && !lru.contains_key(&1));
        assert!(lru.contains_key(&2) && lru.contains_key(&3));
        assert_eq!(lru.bytes(), 30);
        // peek does not refresh: 2 is still the oldest.
        assert!(lru.peek(&2).is_some());
        lru.insert(4, (), 10);
        assert!(!lru.contains_key(&2) && lru.contains_key(&3) && lru.contains_key(&0));
    }

    #[test]
    fn item_limit_oversize_and_resize() {
        let mut lru = ByteLru::new(100, 2);
        lru.insert('a', (), 1);
        lru.insert('b', (), 1);
        assert_eq!(lru.insert('c', (), 1), Some(1));
        assert!(!lru.contains_key(&'a') && lru.len() == 2);
        // An entry larger than the whole budget is refused and changes nothing.
        assert_eq!(lru.insert('b', (), 101), None);
        assert_eq!((lru.len(), lru.bytes()), (2, 2));
        // Re-inserting a key replaces its weight instead of adding to it.
        assert_eq!(lru.insert('b', (), 60), Some(0));
        assert_eq!(lru.bytes(), 61);
        assert_eq!(lru.insert('b', (), 5), Some(0));
        assert_eq!(lru.bytes(), 6);
        assert_eq!(lru.remove(&'c'), Some(()));
        assert_eq!(lru.bytes(), 5);
        lru.clear();
        assert!(lru.is_empty() && lru.bytes() == 0);
    }
}
