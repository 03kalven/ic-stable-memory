use crate::utils::encode::AsBytes;
use crate::{allocate, deallocate, OutOfMemory, SSlice};
use candid::types::{Serializer, Type};
use candid::{CandidType, Deserialize};
use serde::Deserializer;
use std::fmt;
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::mem::size_of;

const STABLE_VEC_DEFAULT_CAPACITY: u64 = 4;
const MAX_SECTOR_SIZE: usize = 2usize.pow(29); // 512MB

struct SVecSector;

#[derive(CandidType, Deserialize)]
struct SVecInfo {
    _len: u64,
    _capacity: u64,
    _sectors: Vec<SSlice<SVecSector>>,
    _sector_sizes: Vec<u32>,
}

pub struct SVec<T> {
    _info: SVecInfo,
    _data: PhantomData<T>,
}

impl<T> CandidType for SVec<T> {
    fn _ty() -> Type {
        SVecInfo::_ty()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: Serializer,
    {
        self._info.idl_serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for SVec<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let _info = SVecInfo::deserialize(deserializer)?;
        Ok(Self {
            _info,
            _data: PhantomData::default(),
        })
    }
}

impl<T: Sized + AsBytes> Default for SVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Sized + AsBytes> SVec<T> {
    pub fn new() -> Self {
        Self::new_with_capacity(STABLE_VEC_DEFAULT_CAPACITY)
    }

    pub fn new_with_capacity(capacity: u64) -> Self {
        let _info = SVecInfo {
            _len: 0,
            _capacity: capacity,
            _sectors: Vec::new(),
            _sector_sizes: Vec::new(),
        };

        Self {
            _info,
            _data: PhantomData::default(),
        }
    }

    pub fn push(&mut self, element: &T) -> Result<(), OutOfMemory> {
        if self._info._sectors.is_empty() {
            self.init_sectors()?;
        }

        self.grow_if_needed()?;
        self.set_len(self.len() + 1);

        let (sector, offset) = self.calculate_inner_index(self.len() - 1);

        let bytes_element = unsafe { element.as_bytes() };
        sector._write_bytes(offset, &bytes_element);

        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }

        let idx = len - 1;
        let (sector, offset) = self.calculate_inner_index(idx);

        let mut element_bytes = vec![0u8; size_of::<T>()];
        sector._read_bytes(offset, &mut element_bytes);
        self.set_len(idx);

        unsafe { Some(T::from_bytes(&element_bytes)) }
    }

    pub fn get(&self, idx: u64) -> Option<T> {
        if idx >= self.len() {
            return None;
        }

        let (sector, offset) = self.calculate_inner_index(idx);

        let mut element_bytes = vec![0u8; size_of::<T>()];
        sector._read_bytes(offset, &mut element_bytes);

        unsafe { Some(T::from_bytes(&element_bytes)) }
    }

    pub fn replace(&mut self, idx: u64, element: &T) -> T {
        assert!(idx < self.len(), "Out of bounds");

        let (sector, offset) = self.calculate_inner_index(idx);

        let mut element_bytes = vec![0u8; size_of::<T>()];
        sector._read_bytes(offset, &mut element_bytes);
        let prev_element = unsafe { T::from_bytes(&element_bytes) };

        let bytes_element = unsafe { element.as_bytes() };
        sector._write_bytes(offset, &bytes_element);

        prev_element
    }

    pub fn swap(&mut self, idx1: u64, idx2: u64) {
        assert!(idx1 < self.len(), "Out of bounds");
        assert!(idx2 < self.len(), "Out of bounds");

        if self.is_empty() || self.len() == 1 {
            return;
        }

        let (sector1, offset1) = self.calculate_inner_index(idx1);
        let (sector2, offset2) = self.calculate_inner_index(idx2);

        let mut elem1 = vec![0u8; size_of::<T>()];
        sector1._read_bytes(offset1, &mut elem1);

        let mut elem2 = vec![0u8; size_of::<T>()];
        sector2._read_bytes(offset2, &mut elem2);

        sector1._write_bytes(offset1, &elem2);
        sector2._write_bytes(offset2, &elem1);
    }

    pub fn drop(self) {
        for sector in self._info._sectors {
            deallocate(sector);
        }
    }

    pub fn capacity(&self) -> u64 {
        self._info._capacity
    }

    fn set_capacity(&mut self, new_capacity: u64) {
        self._info._capacity = new_capacity;
    }

    pub fn len(&self) -> u64 {
        self._info._len
    }

    pub fn is_empty(&self) -> bool {
        self._info._len == 0
    }

    fn set_len(&mut self, new_len: u64) {
        self._info._len = new_len;
    }

    fn get_sector_size(&self, idx: usize) -> usize {
        self._info._sector_sizes[idx] as usize
    }

    fn get_sector(&self, idx: usize) -> &SSlice<SVecSector> {
        &self._info._sectors[idx]
    }

    fn get_sector_mut(&mut self, idx: usize) -> &mut SSlice<SVecSector> {
        &mut self._info._sectors[idx]
    }

    fn get_sectors_count(&self) -> usize {
        self._info._sectors.len()
    }

    pub fn is_about_to_grow(&self) -> bool {
        self.len() == self.capacity()
    }

    fn grow_if_needed(&mut self) -> Result<(), OutOfMemory> {
        if self.is_about_to_grow() {
            let last_sector_size = self.get_sector_size(self.get_sectors_count() - 1);
            let new_sector_size = if last_sector_size * 2 < MAX_SECTOR_SIZE {
                last_sector_size * 2
            } else {
                MAX_SECTOR_SIZE
            };

            let sector = allocate(new_sector_size)?;
            self.set_capacity(self.capacity() + (new_sector_size / size_of::<T>()) as u64);
            self._info._sectors.push(sector);
            self._info._sector_sizes.push(new_sector_size as u32);
        }

        Ok(())
    }

    fn calculate_inner_index(&self, idx: u64) -> (&SSlice<SVecSector>, usize) {
        assert!(idx < self.len());

        let mut idx_counter: u64 = 0;

        for (sector_idx, sector_size) in self._info._sector_sizes.iter().enumerate() {
            let elems_in_sector = (*sector_size as usize / size_of::<T>()) as u64;
            idx_counter += elems_in_sector;

            if idx_counter > idx as u64 {
                let sector = self.get_sector(sector_idx);

                if idx == 0 {
                    return (sector, 0);
                }

                // usize cast guaranteed by the fact that a single sector can only hold usize of
                // bytes and we iterate over them one by one
                let offset =
                    (elems_in_sector - (idx_counter - idx as u64)) as usize * size_of::<T>();

                return (sector, offset);
            }
        }

        // guaranteed by the len check at the beginning of the function
        unreachable!("Unable to calculate inner index");
    }

    fn init_sectors(&mut self) -> Result<(), OutOfMemory> {
        let mut sectors = vec![];
        let mut sector_sizes = vec![];
        let mut capacity_size = self._info._capacity * size_of::<T>() as u64;

        while capacity_size > MAX_SECTOR_SIZE as u64 {
            let sector_res = allocate::<SVecSector>(MAX_SECTOR_SIZE);

            match sector_res {
                Ok(sector) => {
                    sectors.push(sector);
                    sector_sizes.push(MAX_SECTOR_SIZE as u32);
                    capacity_size -= MAX_SECTOR_SIZE as u64;
                }
                // revert
                Err(e) => {
                    for sector in sectors {
                        deallocate(sector);
                    }

                    return Err(e);
                }
            }
        }

        let sector_res = allocate::<SVecSector>(capacity_size as usize);

        match sector_res {
            Ok(sector) => {
                sectors.push(sector);
                sector_sizes.push(capacity_size as u32);
            }
            // revert
            Err(e) => {
                for sector in sectors {
                    deallocate(sector);
                }

                return Err(e);
            }
        }

        self._info._sectors = sectors.iter().map(|it| unsafe { it.clone() }).collect();
        self._info._sector_sizes = sector_sizes;

        Ok(())
    }
}

impl<T: Debug + Sized + AsBytes> Debug for SVec<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut sector_strs = Vec::new();
        for sector in &self._info._sectors {
            let mut elems = Vec::new();

            let size = sector.get_size_bytes();
            let size_elems = (size / size_of::<T>() as usize) as usize;

            for i in 0..size_elems {
                let mut elem_bytes = vec![0u8; size_of::<T>()];
                sector._read_bytes(i * size_of::<T>(), &mut elem_bytes);
                elems.push(format!("{:?}", unsafe { T::from_bytes(&elem_bytes) }));
            }

            sector_strs.push(elems)
        }

        f.debug_struct("SVec")
            .field("len", &self._info._len)
            .field("capacity", &self._info._capacity)
            .field("sectors", &sector_strs)
            .finish()
    }
}

impl<T: Sized + AsBytes + Display> Display for SVec<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut elements = vec![];

        for i in 0..self.len() {
            let element = self.get(i).unwrap();
            elements.push(format!("{}", element));
        }

        write!(f, "[{}]", elements.join(","))
    }
}

#[cfg(test)]
mod tests {
    use crate::collections::vec::{SVec, STABLE_VEC_DEFAULT_CAPACITY};
    use crate::init_allocator;
    use crate::primitive::s_cellbox::SCellBox;
    use crate::utils::mem_context::stable;
    use candid::{CandidType, Deserialize, Nat};

    #[test]
    fn create_destroy_work_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut stable_vec = SVec::<Test>::new();
        assert_eq!(stable_vec.capacity(), STABLE_VEC_DEFAULT_CAPACITY);
        assert_eq!(stable_vec.len(), 0);

        stable_vec.drop();

        stable_vec = SVec::<Test>::new_with_capacity(10_000);
        assert_eq!(stable_vec.capacity(), 10_000);
        assert_eq!(stable_vec.len(), 0);

        stable_vec.drop();
    }

    #[derive(Copy, Clone, Debug)]
    struct Test {
        a: u64,
        b: u64,
    }

    #[test]
    fn push_pop_work_fine() {
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut stable_vec = SVec::new();
        let count = 1000u64;

        for i in 0..count {
            let it = Test { a: i, b: count - i };

            stable_vec.push(&it).unwrap();
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after push");

        for i in 0..count {
            let it = Test { a: count - i, b: i };

            stable_vec.replace(i, &it);
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after set");

        for i in 0..count {
            let it = stable_vec.get(i).unwrap();

            assert_eq!(it.a, count - i);
            assert_eq!(it.b, i);
        }

        for i in 0..count {
            let it = stable_vec.pop().unwrap();

            assert_eq!(it.a, (i + 1)); // i+1 because the last one will be {a: 1; b: 999}
            assert_eq!(it.b, count - (i + 1));
        }

        assert_eq!(stable_vec.len(), 0, "Invalid len after pop");
    }

    #[derive(CandidType, Deserialize, Debug)]
    struct TestIndirect {
        a: Nat,
        b: String,
    }

    #[test]
    fn push_pop_indirect_work_fine() {
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut stable_vec = SVec::new();
        let count = 10u64;

        for i in 0..count {
            let it = SCellBox::new(&TestIndirect {
                a: Nat::from(i),
                b: format!("Str {}", i),
            })
            .unwrap();

            stable_vec
                .push(&it)
                .unwrap_or_else(|e| panic!("Unable to push at step {}: {:?}", i, e));
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after push");

        for i in 0..count {
            let it = SCellBox::new(&TestIndirect {
                a: Nat::from(i),
                b: format!("String of the element {}", i),
            })
            .unwrap();

            stable_vec.replace(i, &it).drop();
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after push");

        for i in 0..count {
            let it = stable_vec.pop().unwrap();
            let val = it.get_cloned();
            it.drop();

            assert_eq!(val.a, Nat::from(count - 1 - i));
            assert_eq!(val.b, format!("String of the element {}", count - 1 - i));
        }

        assert_eq!(stable_vec.len(), 0, "Invalid len after pop");
    }
}
