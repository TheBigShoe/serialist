//! A small exact LRU map, used for shaped lines and wrap counts.
//!
//! Slots live in a `Vec` threaded into a doubly linked list by index, so lookups,
//! inserts and evictions are O(1) with no allocation once the cache is full.

use std::collections::HashMap;
use std::hash::Hash;

const NIL: usize = usize::MAX;

struct Slot<K, V> {
    key: K,
    value: V,
    prev: usize,
    next: usize,
}

pub struct Lru<K, V> {
    map: HashMap<K, usize>,
    slots: Vec<Slot<K, V>>,
    /// Most recently used.
    head: usize,
    /// Least recently used, the next to go.
    tail: usize,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            map: HashMap::with_capacity(capacity),
            slots: Vec::with_capacity(capacity),
            head: NIL,
            tail: NIL,
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.slots.clear();
        self.head = NIL;
        self.tail = NIL;
    }

    /// Look up `key` and mark it most recently used.
    pub fn get(&mut self, key: &K) -> Option<&mut V> {
        let ix = *self.map.get(key)?;
        self.touch(ix);
        Some(&mut self.slots[ix].value)
    }

    /// Look up without changing the order.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.map.get(key).map(|&ix| &self.slots[ix].value)
    }

    /// Insert or replace, evicting the least recently used entry when full.
    pub fn insert(&mut self, key: K, value: V) {
        if let Some(&ix) = self.map.get(&key) {
            self.slots[ix].value = value;
            self.touch(ix);
            return;
        }
        let ix = if self.slots.len() < self.capacity {
            self.slots.push(Slot {
                key: key.clone(),
                value,
                prev: NIL,
                next: NIL,
            });
            self.slots.len() - 1
        } else {
            // Reuse the least recently used slot.
            let ix = self.tail;
            self.unlink(ix);
            let slot = &mut self.slots[ix];
            self.map.remove(&slot.key);
            slot.key = key.clone();
            slot.value = value;
            ix
        };
        self.map.insert(key, ix);
        self.push_front(ix);
    }

    fn touch(&mut self, ix: usize) {
        if self.head != ix {
            self.unlink(ix);
            self.push_front(ix);
        }
    }

    fn unlink(&mut self, ix: usize) {
        let (prev, next) = (self.slots[ix].prev, self.slots[ix].next);
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next].prev = prev;
        }
        self.slots[ix].prev = NIL;
        self.slots[ix].next = NIL;
    }

    fn push_front(&mut self, ix: usize) {
        self.slots[ix].prev = NIL;
        self.slots[ix].next = self.head;
        if self.head != NIL {
            self.slots[self.head].prev = ix;
        }
        self.head = ix;
        if self.tail == NIL {
            self.tail = ix;
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn evicts_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert(1, "one");
        lru.insert(2, "two");
        assert_eq!(lru.get(&1), Some(&mut "one"));
        lru.insert(3, "three");
        assert_eq!(lru.peek(&2), None, "2 was least recently used");
        assert_eq!(lru.peek(&1), Some(&"one"));
        assert_eq!(lru.peek(&3), Some(&"three"));
        lru.insert(1, "uno");
        assert_eq!(lru.len(), 2);
        lru.insert(4, "four");
        assert_eq!(lru.peek(&3), None);
        assert_eq!(lru.peek(&1), Some(&"uno"));
        lru.clear();
        assert!(lru.is_empty());
        lru.insert(5, "five");
        assert_eq!(lru.peek(&5), Some(&"five"));
    }

    proptest! {
        /// Against a naive model: a Vec in recency order.
        #[test]
        fn matches_a_naive_model(ops in prop::collection::vec((0u8..3, 0u8..12), 0..400)) {
            let mut lru = Lru::new(5);
            let mut model: Vec<(u8, u32)> = Vec::new();
            for (step, (op, key)) in ops.into_iter().enumerate() {
                match op {
                    0 | 1 => {
                        let value = step as u32;
                        lru.insert(key, value);
                        model.retain(|(k, _)| *k != key);
                        model.insert(0, (key, value));
                        model.truncate(5);
                    }
                    _ => {
                        let expected = model.iter().position(|(k, _)| *k == key);
                        let got = lru.get(&key).copied();
                        match expected {
                            Some(ix) => {
                                let entry = model.remove(ix);
                                prop_assert_eq!(got, Some(entry.1));
                                model.insert(0, entry);
                            }
                            None => prop_assert_eq!(got, None),
                        }
                    }
                }
                prop_assert_eq!(lru.len(), model.len());
            }
        }
    }
}
