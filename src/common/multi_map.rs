use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// A heap-allocated item shared between two hash maps via raw pointers.
///
/// # Safety
/// Each `RawItem` owns a `Box<(K1, K2, V)>` via a raw pointer.
/// The value is allocated once in `insert` and freed exactly once in `remove1`,
/// `remove2`, or `Drop::drop`. The two maps hold separate `RawItem` handles
/// pointing to the same heap allocation, so removal from one map must not
/// double-free — this is guaranteed by having `remove1` / `remove2` also remove
/// the entry from the other map before freeing.
struct RawItem<K1, K2, V>(*mut (K1, K2, V));

// SAFETY: `RawItem` only gives out shared/mutable references to the inner
// tuple, and access is serialized through `&self` / `&mut self` on `MultiMap`.
unsafe impl<K1, K2, V> Send for RawItem<K1, K2, V> {}
unsafe impl<K1, K2, V> Sync for RawItem<K1, K2, V> {}

/// MultiMap is a hash map that can index an item by two keys
/// For example, after an item with key (a, b) is insert, `map.get1(a)` and
/// `map.get2(b)` both returns the item. Likewise the `remove1` and `remove2`.
pub struct MultiMap<K1, K2, V> {
    map1: HashMap<Key<K1>, RawItem<K1, K2, V>>,
    map2: HashMap<Key<K2>, RawItem<K1, K2, V>>,
}

/// Wrapper around a raw pointer used as a hash map key.
///
/// # Safety
/// The pointee must outlive any `Key` that references it, which is guaranteed
/// because `Key` values are only stored inside the `MultiMap` and the pointee
/// is the heap-allocated tuple owned by `RawItem` — the `MultiMap` itself
/// ensures the tuple outlives the keys stored in its hash maps.
struct Key<T>(*const T);

// SAFETY: `Key` is only used as a hash map key inside `MultiMap`, where the
// pointee is the heap-allocated item owned by the map itself. Access to the
// map is serialized through `&self` / `&mut self`.
unsafe impl<T> Send for Key<T> {}
unsafe impl<T> Sync for Key<T> {}

impl<T> Borrow<T> for Key<T> {
    fn borrow(&self) -> &T {
        // SAFETY: The pointee is guaranteed to outlive the `Key` because both
        // are owned by the `MultiMap` — the `Key` is stored in the map's
        // `HashMap` and the pointee is the heap-allocated tuple, which is
        // only freed when the item is removed or the map is dropped.
        unsafe { &*self.0 }
    }
}

impl<T: Hash> Hash for Key<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.borrow() as &T).hash(state)
    }
}

impl<T: PartialEq> PartialEq for Key<T> {
    fn eq(&self, other: &Self) -> bool {
        (self.borrow() as &T).eq(other.borrow())
    }
}

impl<T: Eq> Eq for Key<T> {}

impl<K1, K2, V> MultiMap<K1, K2, V> {
    pub fn new() -> Self {
        MultiMap {
            map1: HashMap::new(),
            map2: HashMap::new(),
        }
    }
}

#[allow(dead_code)]
impl<K1, K2, V> MultiMap<K1, K2, V>
where
    K1: Hash + Eq + Send,
    K2: Hash + Eq + Send,
    V: Send,
{
    pub fn insert(&mut self, k1: K1, k2: K2, v: V) -> Result<(), (K1, K2, V)> {
        if self.map1.contains_key(&k1) || self.map2.contains_key(&k2) {
            return Err((k1, k2, v));
        }
        let item = Box::new((k1, k2, v));
        let k1 = Key(&item.0);
        let k2 = Key(&item.1);
        let item = Box::into_raw(item);
        self.map1.insert(k1, RawItem(item));
        self.map2.insert(k2, RawItem(item));
        Ok(())
    }

    pub fn get1(&self, k1: &K1) -> Option<&V> {
        let item = self.map1.get(k1)?;
        // SAFETY: The `RawItem` pointer is valid because it was created from
        // a `Box::into_raw` in `insert` and hasn't been freed yet — the item
        // is still stored in at least one of the maps.
        let item = unsafe { &*item.0 };
        Some(&item.2)
    }

    pub fn get1_mut(&mut self, k1: &K1) -> Option<&mut V> {
        let item = self.map1.get(k1)?;
        // SAFETY: `&mut self` ensures exclusive access, and the pointer is
        // valid as above. The aliasing `map2` entry points to the same
        // allocation but is not accessed during the mutable borrow.
        let item = unsafe { &mut *item.0 };
        Some(&mut item.2)
    }

    pub fn get2(&self, k2: &K2) -> Option<&V> {
        let item = self.map2.get(k2)?;
        // SAFETY: Same as `get1` — the pointer is valid and the item is
        // still live.
        let item = unsafe { &*item.0 };
        Some(&item.2)
    }

    pub fn get_mut2(&mut self, k2: &K2) -> Option<&mut V> {
        let item = self.map2.get(k2)?;
        // SAFETY: Same as `get1_mut` — `&mut self` gives exclusive access.
        let item = unsafe { &mut *item.0 };
        Some(&mut item.2)
    }

    pub fn remove1(&mut self, k1: &K1) -> Option<V> {
        let item = self.map1.remove(k1)?;
        // SAFETY: We just removed the entry from `map1`, so the raw pointer
        // is the last reference (aside from the still-live entry in `map2`).
        // Reconstructing the `Box` lets it drop, which also frees the
        // allocation. The matching `map2` entry is cleaned up below.
        let item = unsafe { Box::from_raw(item.0) };
        self.map2.remove(&item.1);
        Some(item.2)
    }

    pub fn remove2(&mut self, k2: &K2) -> Option<V> {
        let item = self.map2.remove(k2)?;
        // SAFETY: Same as `remove1` — the last reference was removed from
        // `map2`, and we reconstruct the `Box` to free it.
        let item = unsafe { Box::from_raw(item.0) };
        self.map1.remove(&item.0);
        Some(item.2)
    }
}

impl<K1, K2, V> Drop for MultiMap<K1, K2, V> {
    fn drop(&mut self) {
        self.map1.clear();
        // SAFETY: After `map1.clear()`, only `map2` holds live `RawItem`
        // handles. Each handle points to a heap-allocated tuple, which we
        // reconstruct as a `Box` so it is properly freed. We drain `map2` to
        // avoid iterating freed pointers.
        self.map2
            .drain()
            .for_each(|(_, item)| drop(unsafe { Box::from_raw(item.0) }));
    }
}
