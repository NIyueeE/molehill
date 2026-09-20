//! A hash map that indexes each item by two keys.
//!
//! After `insert(k1, k2, v)` the item is reachable through `get2(k2)`, and
//! `remove1(k1)` drops it from both indexes. The second key is stored in
//! both maps — as `map2`'s key and inside `map1`'s value — which is what
//! makes the removal total: no shared ownership between the maps, and no
//! `unsafe`.

use std::collections::HashMap;
use std::hash::Hash;

/// A hash map that can index an item by two keys.
///
/// For example, after an item with keys (a, b) is inserted, `map.get2(b)`
/// returns the item and `remove1(a)` removes it from both indexes.
pub struct MultiMap<K1, K2, V> {
    map1: HashMap<K1, (K2, V)>,
    map2: HashMap<K2, K1>,
}

impl<K1, K2, V> MultiMap<K1, K2, V> {
    pub fn new() -> MultiMap<K1, K2, V> {
        MultiMap {
            map1: HashMap::new(),
            map2: HashMap::new(),
        }
    }
}

impl<K1, K2, V> MultiMap<K1, K2, V>
where
    K1: Eq + Hash + Clone,
    K2: Eq + Hash + Clone,
{
    pub fn insert(&mut self, k1: K1, k2: K2, v: V) -> Result<(), (K1, K2, V)> {
        if self.map1.contains_key(&k1) || self.map2.contains_key(&k2) {
            return Err((k1, k2, v));
        }
        self.map2.insert(k2.clone(), k1.clone());
        self.map1.insert(k1, (k2, v));
        Ok(())
    }

    pub fn get2(&self, k2: &K2) -> Option<&V> {
        let k1 = self.map2.get(k2)?;
        self.map1.get(k1).map(|(_, v)| v)
    }

    pub fn remove1(&mut self, k1: &K1) -> Option<V> {
        let (k2, v) = self.map1.remove(k1)?;
        self.map2.remove(&k2);
        Some(v)
    }
}
