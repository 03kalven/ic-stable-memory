use crate::primitive::s_slice::PTR_SIZE;
use crate::utils::phantom_data::SPhantomData;
use crate::{allocate, deallocate, SSlice, SUnsafeCell};
use speedy::{LittleEndian, Readable, Writable};

const STABLE_VEC_DEFAULT_CAPACITY: u64 = 4;
const MAX_SECTOR_SIZE: usize = 2usize.pow(29); // 512MB

struct SVecSector;

#[derive(Readable, Writable)]
struct SVecInfo {
    _len: u64,
    _capacity: u64,
    _sectors: Vec<SSlice<SVecSector>>,
    _sector_sizes: Vec<u32>,
}

#[derive(Readable, Writable)]
pub struct SVec<T> {
    _info: SVecInfo,
    _data: SPhantomData<T>,
}

impl<'a, T: Readable<'a, LittleEndian> + Writable<LittleEndian>> SVec<T> {
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
            _data: SPhantomData::default(),
        }
    }

    pub fn push(&mut self, element: &T) {
        let elem_cell = SUnsafeCell::new(element);
        let elem_ptr = unsafe { elem_cell.as_ptr() };

        if self._info._sectors.is_empty() {
            self.init_sectors();
        }

        self.grow_if_needed();
        self.set_len(self.len() + 1);

        let (sector, offset) = self.calculate_inner_index(self.len() - 1);

        sector._write_word(offset, elem_ptr);
    }

    pub fn pop(&mut self) -> Option<T> {
        let len = self.len();
        if len == 0 {
            return None;
        }

        let idx = len - 1;
        let (sector, offset) = self.calculate_inner_index(idx);
        let elem_ptr = sector._read_word(offset);
        self.set_len(idx);

        let elem_cell = unsafe { SUnsafeCell::<T>::from_ptr(elem_ptr) };
        let elem = elem_cell.get_cloned();
        elem_cell.drop();

        Some(elem)
    }

    pub fn get_cloned(&self, idx: u64) -> Option<T> {
        if idx >= self.len() {
            return None;
        }

        let (sector, offset) = self.calculate_inner_index(idx);
        let elem_ptr = sector._read_word(offset);
        let elem_cell = unsafe { SUnsafeCell::<T>::from_ptr(elem_ptr) };
        let elem = elem_cell.get_cloned();

        Some(elem)
    }

    pub fn replace(&mut self, idx: u64, element: &T) -> T {
        assert!(idx < self.len(), "Out of bounds");
        let new_elem_cell = SUnsafeCell::new(element);
        let new_elem_ptr = unsafe { new_elem_cell.as_ptr() };

        let (sector, offset) = self.calculate_inner_index(idx);

        let prev_elem_ptr = sector._read_word(offset);
        let prev_elem_cell = unsafe { SUnsafeCell::<T>::from_ptr(prev_elem_ptr) };
        let prev_elem = prev_elem_cell.get_cloned();

        sector._write_word(offset, new_elem_ptr);

        prev_elem
    }

    pub fn swap(&mut self, idx1: u64, idx2: u64) {
        assert!(idx1 < self.len(), "Out of bounds");
        assert!(idx2 < self.len(), "Out of bounds");

        if self.is_empty() || self.len() == 1 {
            return;
        }

        let (sector1, offset1) = self.calculate_inner_index(idx1);
        let (sector2, offset2) = self.calculate_inner_index(idx2);

        let elem_ptr_1 = sector1._read_word(offset1);
        let elem_ptr_2 = sector2._read_word(offset2);

        sector1._write_word(offset1, elem_ptr_2);
        sector2._write_word(offset2, elem_ptr_1);
    }

    pub fn drop(mut self) {
        loop {
            if self.pop().is_none() {
                break;
            }
        }

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

    fn grow_if_needed(&mut self) {
        if self.is_about_to_grow() {
            let last_sector_size = self.get_sector_size(self.get_sectors_count() - 1);
            let new_sector_size = if last_sector_size * 2 < MAX_SECTOR_SIZE {
                last_sector_size * 2
            } else {
                MAX_SECTOR_SIZE
            };

            let sector = allocate(new_sector_size);
            self.set_capacity(self.capacity() + (new_sector_size / PTR_SIZE) as u64);
            self._info._sectors.push(sector);
            self._info._sector_sizes.push(new_sector_size as u32);
        }
    }

    fn calculate_inner_index(&self, idx: u64) -> (&SSlice<SVecSector>, usize) {
        assert!(idx < self.len());

        let mut idx_counter: u64 = 0;

        for (sector_idx, sector_size) in self._info._sector_sizes.iter().enumerate() {
            let elems_in_sector = (*sector_size as usize / PTR_SIZE) as u64;
            idx_counter += elems_in_sector;

            if idx_counter > idx as u64 {
                let sector = self.get_sector(sector_idx);

                if idx == 0 {
                    return (sector, 0);
                }

                // usize cast guaranteed by the fact that a single sector can only hold usize of
                // bytes and we iterate over them one by one
                let offset = (elems_in_sector - (idx_counter - idx as u64)) as usize * PTR_SIZE;

                return (sector, offset);
            }
        }

        // guaranteed by the len check at the beginning of the function
        unreachable!("Unable to calculate inner index");
    }

    fn init_sectors(&mut self) {
        let mut sectors = vec![];
        let mut sector_sizes = vec![];
        let mut capacity_size = self._info._capacity * PTR_SIZE as u64;

        while capacity_size > MAX_SECTOR_SIZE as u64 {
            let sector = allocate::<SVecSector>(MAX_SECTOR_SIZE);

            sectors.push(sector);
            sector_sizes.push(MAX_SECTOR_SIZE as u32);
            capacity_size -= MAX_SECTOR_SIZE as u64;
        }

        let sector = allocate::<SVecSector>(capacity_size as usize);

        sectors.push(sector);
        sector_sizes.push(capacity_size as u32);

        self._info._sectors = sectors.iter().map(|it| unsafe { it.clone() }).collect();
        self._info._sector_sizes = sector_sizes;
    }
}

impl<'a, T: Readable<'a, LittleEndian> + Writable<LittleEndian>> Default for SVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::collections::vec::{SVec, STABLE_VEC_DEFAULT_CAPACITY};
    use crate::init_allocator;
    use crate::utils::mem_context::stable;
    use speedy::{Readable, Writable};

    #[derive(Readable, Writable, Debug)]
    struct Test {
        a: u64,
        b: String,
    }

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

    #[test]
    fn push_pop_work_fine() {
        stable::clear();
        stable::grow(1).unwrap();
        init_allocator(0);

        let mut stable_vec = SVec::new();
        let count = 10u64;

        for i in 0..count {
            let it = Test {
                a: i,
                b: format!("Str {}", i),
            };

            stable_vec.push(&it);
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after push");

        for i in 0..count {
            let it = Test {
                a: i,
                b: format!("String of the element {}", i),
            };

            stable_vec.replace(i, &it);
        }

        assert_eq!(stable_vec.len(), count, "Invalid len after push");

        for i in 0..count {
            let it = stable_vec.pop().unwrap();

            assert_eq!(it.a, count - 1 - i);
            assert_eq!(it.b, format!("String of the element {}", count - 1 - i));
        }

        assert_eq!(stable_vec.len(), 0, "Invalid len after pop");

        for i in 0..count {
            let it = Test {
                a: i,
                b: format!("Str {}", i),
            };

            stable_vec.push(&it);
        }

        stable_vec.drop();
    }
}
