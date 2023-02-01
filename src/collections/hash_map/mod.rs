use crate::collections::hash_map::iter::{SHashMapIter, SHashMapIterCopy, SHashMapIterMut};
use crate::encoding::{AsDynSizeBytes, AsFixedSizeBytes, Buffer};
use crate::mem::allocator::EMPTY_PTR;
use crate::mem::s_slice::Side;
use crate::primitive::s_ref::SRef;
use crate::primitive::s_ref_mut::SRefMut;
use crate::primitive::{StableAllocated, StableDrop};
use crate::{allocate, deallocate, SSlice};
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use zwohash::ZwoHasher;

pub mod iter;

// BY DEFAULT:
// KEYS: [K; CAPACITY] = [zeroed(K); CAPACITY]
// VALUES: [V; CAPACITY] = [zeroed(V); CAPACITY]

const KEYS_OFFSET: usize = 0;

#[inline]
const fn values_offset<K: AsFixedSizeBytes>(capacity: usize) -> usize {
    KEYS_OFFSET + (1 + K::SIZE) * capacity
}

const DEFAULT_CAPACITY: usize = 7;

const EMPTY: u8 = 0;
const OCCUPIED: u8 = 255;

type KeyHash = usize;

// all for maximum cache-efficiency
// fixed-size, open addressing, linear probing, 3/4 load factor, non-lazy removal (https://stackoverflow.com/a/60709252/7171515)
pub struct SHashMap<K, V> {
    table_ptr: u64,
    len: usize,
    cap: usize,
    _marker_k: PhantomData<K>,
    _marker_v: PhantomData<V>,
}

impl<K, V> SHashMap<K, V> {
    fn hash<T: Hash>(val: &T) -> KeyHash {
        let mut hasher = ZwoHasher::default();
        val.hash(&mut hasher);

        hasher.finish() as KeyHash
    }
}

impl<K: StableAllocated + Hash + Eq, V: StableAllocated> SHashMap<K, V> {
    #[inline]
    pub fn new() -> Self {
        Self::new_with_capacity(DEFAULT_CAPACITY)
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        Self {
            table_ptr: EMPTY_PTR,
            len: 0,
            cap: capacity,
            _marker_k: PhantomData::default(),
            _marker_v: PhantomData::default(),
        }
    }

    pub fn insert(&mut self, mut key: K, mut value: V) -> Option<V> {
        if self.table_ptr == EMPTY_PTR {
            let size = (1 + K::SIZE + V::SIZE) * self.capacity();
            let table = allocate(size as usize);

            let zeroed = vec![0u8; size as usize];
            table.write_bytes(0, &zeroed);

            self.table_ptr = table.as_ptr();
        }

        let key_hash = Self::hash(&key);
        let mut i = key_hash % self.capacity();

        loop {
            match self.read_key_at(i, true) {
                HashMapKey::Occupied(prev_key) => {
                    if prev_key.eq(&key) {
                        let mut prev_value = self.read_val_at(i);
                        prev_value.remove_from_stable();

                        value.move_to_stable();
                        self.write_val_at(i, value);

                        return Some(prev_value);
                    } else {
                        i = (i + 1) % self.capacity();

                        continue;
                    }
                }
                HashMapKey::Empty => {
                    if self.is_full() {
                        let mut new = Self::new_with_capacity(self.capacity() * 2 - 1);

                        for (k, v) in unsafe { self.iter_copy() } {
                            new.insert(k, v);
                        }

                        let res = new.insert(key, value);

                        let slice = SSlice::from_ptr(self.table_ptr, Side::Start).unwrap();
                        deallocate(slice);

                        *self = new;

                        return res;
                    }

                    key.move_to_stable();
                    value.move_to_stable();

                    self.write_key_at(i, HashMapKey::Occupied(key));
                    self.write_val_at(i, value);

                    self.len += 1;

                    return None;
                }
                _ => unreachable!(),
            }
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (i, mut k) = self.find_inner_idx(key)?;
        let mut v = self.remove_by_idx(i);

        k.remove_from_stable();
        v.remove_from_stable();

        Some(v)
    }

    fn remove_by_idx(&mut self, mut i: usize) -> V {
        let prev_value = self.read_val_at(i);
        let mut j = i;

        loop {
            j = (j + 1) % self.capacity();
            if j == i {
                break;
            }
            match self.read_key_at(j, true) {
                HashMapKey::Empty => break,
                HashMapKey::Occupied(next_key) => {
                    let k = Self::hash(&next_key) % self.capacity();
                    if (j < i) ^ (k <= i) ^ (k > j) {
                        self.write_key_at(i, HashMapKey::Occupied(next_key));
                        self.write_val_at(i, self.read_val_at(j));

                        i = j;
                    }
                }
                _ => unreachable!(),
            }
        }

        self.write_key_at(i, HashMapKey::Empty);
        self.len -= 1;

        prev_value
    }

    #[inline]
    pub unsafe fn get_copy(&self, key: &K) -> Option<V> {
        let (i, _) = self.find_inner_idx(key)?;

        Some(self.read_val_at(i))
    }

    #[inline]
    pub fn get(&self, key: &K) -> Option<SRef<'_, V>> {
        let (i, _) = self.find_inner_idx(key)?;
        let ptr = self.get_value_ptr(i);

        Some(SRef::new(ptr))
    }

    #[inline]
    pub fn get_mut(&mut self, key: &K) -> Option<SRefMut<'_, V>> {
        let (i, _) = self.find_inner_idx(key)?;
        let ptr = self.get_value_ptr(i);

        Some(SRefMut::new(ptr))
    }

    #[inline]
    pub fn contains_key(&self, key: &K) -> bool {
        self.find_inner_idx(key).is_some()
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub const fn is_full(&self) -> bool {
        self.len() == (self.capacity() >> 2) * 3
    }

    #[inline]
    pub fn iter(&self) -> SHashMapIter<K, V> {
        SHashMapIter::new(self)
    }

    #[inline]
    pub fn iter_mut(&mut self) -> SHashMapIterMut<K, V> {
        SHashMapIterMut::new(self)
    }

    #[inline]
    pub unsafe fn iter_copy(&self) -> SHashMapIterCopy<K, V> {
        SHashMapIterCopy::new(self)
    }

    pub fn clear(&mut self) {
        if self.is_empty() {
            return;
        }

        for i in 0..self.cap {
            match self.read_key_at(i, true) {
                HashMapKey::Empty => continue,
                HashMapKey::Occupied(mut k) => {
                    let mut v = self.read_val_at(i);

                    k.remove_from_stable();
                    v.remove_from_stable();

                    self.write_key_at(i, HashMapKey::Empty);
                }
                _ => unreachable!(),
            }
        }

        self.len = 0;
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&K, &V) -> bool,
    {
        if self.is_empty() {
            return;
        }

        for i in 0..self.cap {
            match self.read_key_at(i, true) {
                HashMapKey::Empty => continue,
                HashMapKey::Occupied(mut k) => {
                    let mut v = self.read_val_at(i);
                    if f(&k, &v) {
                        continue;
                    }

                    k.remove_from_stable();
                    v.remove_from_stable();

                    self.write_key_at(i, HashMapKey::Empty);
                    self.len -= 1;
                }
                _ => unreachable!(),
            }
        }
    }

    fn find_inner_idx(&self, key: &K) -> Option<(usize, K)> {
        if self.is_empty() {
            return None;
        }

        let key_hash = Self::hash(key);
        let mut i = key_hash % self.capacity();

        loop {
            match self.read_key_at(i, true) {
                HashMapKey::Occupied(prev_key) => {
                    if prev_key.eq(key) {
                        return Some((i, prev_key));
                    } else {
                        i = (i + 1) % self.capacity();
                        continue;
                    }
                }
                HashMapKey::Empty => {
                    return None;
                }
                _ => unreachable!(),
            };
        }
    }

    fn get_key_ptr(&self, idx: usize) -> u64 {
        self.table_ptr + (KEYS_OFFSET + (1 + K::SIZE) * idx + 1) as u64
    }

    fn get_value_ptr(&self, idx: usize) -> u64 {
        self.table_ptr + (values_offset::<K>(self.capacity()) + V::SIZE * idx) as u64
    }

    pub(crate) fn read_key_at(&self, idx: usize, read_value: bool) -> HashMapKey<K> {
        let mut key_flag = [0u8];
        let offset = KEYS_OFFSET + (1 + K::SIZE) * idx;

        println!("{} {}", self.table_ptr, offset);

        SSlice::_read_bytes(self.table_ptr, offset, &mut key_flag);

        match key_flag[0] {
            EMPTY => HashMapKey::Empty,
            OCCUPIED => {
                if read_value {
                    let k = SSlice::_as_fixed_size_bytes_read::<K>(self.table_ptr, offset + 1);

                    HashMapKey::Occupied(k)
                } else {
                    HashMapKey::OccupiedNull
                }
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    pub(crate) fn read_val_at(&self, idx: usize) -> V {
        let offset = values_offset::<K>(self.capacity()) + V::SIZE * idx;

        SSlice::_as_fixed_size_bytes_read::<V>(self.table_ptr, offset)
    }

    fn write_key_at(&mut self, idx: usize, key: HashMapKey<K>) {
        let offset = KEYS_OFFSET + (1 + K::SIZE) * idx;

        let key_flag = match key {
            HashMapKey::Empty => [EMPTY],
            HashMapKey::Occupied(k) => {
                SSlice::_as_fixed_size_bytes_write::<K>(self.table_ptr, offset + 1, k);

                [OCCUPIED]
            }
            _ => unreachable!(),
        };

        SSlice::_write_bytes(self.table_ptr, offset, &key_flag);
    }

    #[inline]
    fn write_val_at(&mut self, idx: usize, val: V) {
        let offset = values_offset::<K>(self.capacity()) + V::SIZE * idx;

        SSlice::_as_fixed_size_bytes_write::<V>(self.table_ptr, offset, val);
    }

    pub fn debug_print(&self) {
        print!("Node({}, {})[", self.len(), self.capacity());
        for i in 0..self.capacity() {
            let mut k_flag = [0u8];
            let mut k = K::Buf::new(K::SIZE);
            let mut v = K::Buf::new(K::SIZE);

            SSlice::_read_bytes(self.table_ptr, KEYS_OFFSET + (1 + K::SIZE) * i, &mut k_flag);
            SSlice::_read_bytes(
                self.table_ptr,
                KEYS_OFFSET + (1 + K::SIZE) * i + 1,
                k._deref_mut(),
            );
            SSlice::_read_bytes(
                self.table_ptr,
                values_offset::<K>(self.capacity()) + V::SIZE * i,
                v._deref_mut(),
            );

            print!("(");

            match k_flag[0] {
                EMPTY => print!("<empty> = "),
                OCCUPIED => print!("<occupied> = "),
                _ => unreachable!(),
            };

            print!("{:?}, {:?})", k._deref(), v._deref());

            if i < self.capacity() - 1 {
                print!(", ");
            }
        }
        println!("]");
    }
}

impl<K: StableAllocated + Hash + Eq, V: StableAllocated> Default for SHashMap<K, V> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) enum HashMapKey<K> {
    Empty,
    Occupied(K),
    OccupiedNull,
}

impl<K, V> AsFixedSizeBytes for SHashMap<K, V> {
    const SIZE: usize = u64::SIZE + usize::SIZE * 2;
    type Buf = [u8; u64::SIZE + usize::SIZE * 2];

    fn as_fixed_size_bytes(&self, buf: &mut [u8]) {
        self.table_ptr.as_fixed_size_bytes(&mut buf[0..u64::SIZE]);
        self.len
            .as_fixed_size_bytes(&mut buf[u64::SIZE..(usize::SIZE + u64::SIZE)]);
        self.cap.as_fixed_size_bytes(
            &mut buf[(usize::SIZE + u64::SIZE)..(usize::SIZE * 2 + u64::SIZE)],
        );
    }

    fn from_fixed_size_bytes(buf: &[u8]) -> Self {
        let table_ptr = u64::from_fixed_size_bytes(&buf[0..u64::SIZE]);
        let len = usize::from_fixed_size_bytes(&buf[u64::SIZE..(usize::SIZE + u64::SIZE)]);
        let cap = usize::from_fixed_size_bytes(
            &buf[(usize::SIZE + u64::SIZE)..(usize::SIZE * 2 + u64::SIZE)],
        );

        Self {
            table_ptr,
            len,
            cap,
            _marker_k: PhantomData::default(),
            _marker_v: PhantomData::default(),
        }
    }
}

impl<K: StableAllocated + Eq + Hash, V: StableAllocated> StableAllocated for SHashMap<K, V> {
    #[inline]
    fn move_to_stable(&mut self) {}

    #[inline]
    fn remove_from_stable(&mut self) {}
}

impl<K: StableAllocated + Eq + Hash + StableDrop, V: StableAllocated + StableDrop> StableDrop
    for SHashMap<K, V>
{
    type Output = ();

    unsafe fn stable_drop(self) {
        if self.table_ptr != EMPTY_PTR {
            for (k, v) in self.iter_copy() {
                k.stable_drop();
                v.stable_drop();
            }

            let slice = SSlice::from_ptr(self.table_ptr, Side::Start).unwrap();
            deallocate(slice);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::collections::hash_map::SHashMap;
    use crate::encoding::{AsDynSizeBytes, AsFixedSizeBytes, Buffer};
    use crate::init_allocator;
    use crate::primitive::s_box::SBox;
    use crate::primitive::StableDrop;
    use crate::utils::mem_context::stable;
    use rand::seq::SliceRandom;
    use rand::thread_rng;

    #[test]
    fn simple_flow_works_well() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new_with_capacity(3);

        let k1 = 1;
        let k2 = 2;
        let k3 = 3;
        let k4 = 4;
        let k5 = 5;
        let k6 = 6;
        let k7 = 7;
        let k8 = 8;

        map.insert(k1, 1);
        map.insert(k2, 2);
        map.insert(k3, 3);
        map.insert(k4, 4);
        map.insert(k5, 5);
        map.insert(k6, 6);
        map.insert(k7, 7);
        map.insert(k8, 8);

        assert_eq!(*map.get(&k1).unwrap().read(), 1);
        assert_eq!(*map.get(&k2).unwrap().read(), 2);
        assert_eq!(*map.get(&k3).unwrap().read(), 3);
        assert_eq!(*map.get(&k4).unwrap().read(), 4);
        assert_eq!(*map.get(&k5).unwrap().read(), 5);
        assert_eq!(*map.get(&k6).unwrap().read(), 6);
        assert_eq!(*map.get(&k7).unwrap().read(), 7);
        assert_eq!(*map.get(&k8).unwrap().read(), 8);

        assert!(map.get(&9).is_none());
        assert!(map.get(&0).is_none());

        assert_eq!(map.remove(&k3).unwrap(), 3);
        assert!(map.get(&k3).is_none());

        assert_eq!(map.remove(&k1).unwrap(), 1);
        assert!(map.get(&k1).is_none());

        assert_eq!(map.remove(&k5).unwrap(), 5);
        assert!(map.get(&k5).is_none());

        assert_eq!(map.remove(&k7).unwrap(), 7);
        assert!(map.get(&k7).is_none());

        assert_eq!(*map.get(&k2).unwrap().read(), 2);
        assert_eq!(*map.get(&k4).unwrap().read(), 4);
        assert_eq!(*map.get(&k6).unwrap().read(), 6);
        assert_eq!(*map.get(&k8).unwrap().read(), 8);

        unsafe { map.stable_drop() };
    }

    #[test]
    fn basic_flow_works_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new_with_capacity(3);

        assert!(map.remove(&10).is_none());
        assert!(map.get(&10).is_none());

        let it = map.insert(1, 1);
        assert!(it.is_none());
        assert!(map.insert(2, 2).is_none());
        assert!(map.insert(3, 3).is_none());
        assert_eq!(map.insert(1, 10).unwrap(), 1);

        assert!(map.remove(&5).is_none());
        assert_eq!(map.remove(&1).unwrap(), 10);

        assert!(map.contains_key(&2));
        assert!(!map.contains_key(&5));

        map.debug_print();

        unsafe { map.stable_drop() };

        let mut map = SHashMap::default();
        for i in 0..100 {
            assert!(map.insert(i, i).is_none());
        }

        for i in 0..100 {
            assert_eq!(*map.get(&i).unwrap().read(), i);
        }

        for i in 0..100 {
            assert_eq!(map.remove(&(99 - i)).unwrap(), 99 - i);
        }

        unsafe { map.stable_drop() };
    }

    #[test]
    fn removes_work() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new();

        for i in 0..500 {
            map.insert(499 - i, i);
        }

        let mut vec = (200..300).collect::<Vec<_>>();
        vec.shuffle(&mut thread_rng());

        for i in vec {
            map.remove(&i);
        }

        for i in 500..5000 {
            map.insert(i, i);
        }

        for i in 200..300 {
            map.insert(i, i);
        }

        let mut vec = (0..5000).collect::<Vec<_>>();
        vec.shuffle(&mut thread_rng());

        for i in vec {
            map.remove(&i);
        }

        unsafe { map.stable_drop() };
    }

    #[test]
    fn tombstones_work_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new();

        for i in 0..100 {
            map.insert(i, i);
        }

        assert_eq!(map.len(), 100);

        for i in 0..50 {
            map.remove(&i);
        }

        assert_eq!(map.len(), 50);

        for i in 0..50 {
            map.insert(i, i);
        }

        assert_eq!(map.len(), 100);

        unsafe { map.stable_drop() };
    }

    #[test]
    fn serialization_work_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new();
        map.insert(0, 0);

        let len = map.len();
        let cap = map.capacity();
        let ptr = map.table_ptr;

        let buf = map.as_new_fixed_size_bytes();
        let map1 = SHashMap::<i32, i32>::from_fixed_size_bytes(&buf);

        assert_eq!(len, map1.len());
        assert_eq!(cap, map1.capacity());
        assert_eq!(ptr, map1.table_ptr);
    }

    #[test]
    fn iter_works_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new();
        for i in 0..100 {
            map.insert(i, i);
        }

        let mut c = 0;
        for (mut k, v) in map.iter() {
            c += 1;

            assert!(*k.read() < 100);
        }

        assert_eq!(c, 100);
    }

    #[test]
    fn sboxes_work_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut map = SHashMap::new();

        for i in 0..100 {
            map.insert(SBox::new(i), i);
        }

        unsafe { map.stable_drop() };

        println!("sbox mut");
        let mut map = SHashMap::new();

        for i in 0..100 {
            map.insert(SBox::new(i), i);
        }

        unsafe { map.stable_drop() };
    }
}
