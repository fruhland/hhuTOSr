use alloc::sync::Arc;
use core::cmp::min;
use core::ptr;
use log::debug;
use spin::RwLock;
use x86_64::structures::paging::{Page, PageTable, PageTableFlags, PageTableIndex, PhysFrame};
use x86_64::{PhysAddr, VirtAddr};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::frame::PhysFrameRange;
use x86_64::structures::paging::page::PageRange;
use crate::memory::{MemorySpace, PAGE_SIZE, physical};
use crate::memory::physical::phys_limit;
use crate::process::process::kernel_process;

pub struct AddressSpace {
    root_table: RwLock<*mut PageTable>,
    depth: usize
}

#[derive(Copy, Clone)]
pub struct VirtualMemoryArea {
    range: PageRange,
    typ: VmaType
}

#[derive(Copy, Clone, PartialEq)]
pub enum VmaType {
    Code, Heap, Stack
}

unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

pub fn create_address_space() -> Arc<AddressSpace> {
    debug!("Page frame allocator before address space creation:\n{}", physical::dump());
    match kernel_process() {
        Some(kernel_process) => { // Create user address space
            let kernel_space = AddressSpace::from_other(&kernel_process.address_space());
            Arc::new(kernel_space)
        }
        None => { // Create kernel address space
            let address_space = AddressSpace::new(4);
            let max_phys_addr = phys_limit().start_address();
            let range = PageRange { start: Page::containing_address(VirtAddr::zero()), end: Page::containing_address(VirtAddr::new(max_phys_addr.as_u64())) };

            address_space.map(range, MemorySpace::Kernel, PageTableFlags::PRESENT | PageTableFlags::WRITABLE);
            Arc::new(address_space)
        }
    }
}

fn page_table_index(virt_addr: VirtAddr, level: usize) -> PageTableIndex {
    return PageTableIndex::new_truncate((virt_addr.as_u64() >> 12 >> ((level as u8 - 1) * 9)) as u16);
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let depth = self.depth;
        let root_table_guard = self.root_table.write();
        let root_table = unsafe { root_table_guard.as_mut().unwrap() };

        AddressSpace::drop_table(root_table, depth);
        debug!("Page frame allocator after address space drop:\n{}", physical::dump());
    }
}

impl VirtualMemoryArea {
    pub const fn new(range: PageRange, typ: VmaType) -> Self {
        Self { range, typ }
    }

    pub fn from_address(start: VirtAddr, size: usize, typ: VmaType) -> Self {
        let start_page = Page::from_start_address(start).expect("VirtualMemoryArea: Address is not page aligned!");
        let range = PageRange { start: start_page, end: start_page + (size / PAGE_SIZE) as u64 };

        Self { range, typ }
    }

    pub fn start(&self) -> VirtAddr {
        self.range.start.start_address()
    }

    pub fn end(&self) -> VirtAddr {
        self.range.end.start_address()
    }

    pub fn range(&self) -> PageRange {
        self.range
    }

    pub fn typ(&self) -> VmaType {
        self.typ
    }

    pub fn overlaps_with(&self, other: &VirtualMemoryArea) -> bool {
        if self.range.end <= other.range.start || self.range.start >= other.range.end {
            false
        } else {
            true
        }
    }
}

impl AddressSpace {
    pub fn new(depth: usize) -> Self {
        let table_addr = physical::alloc(1).start;
        let root_table = table_addr.start_address().as_u64() as *mut PageTable;
        unsafe { root_table.as_mut().unwrap().zero(); }

        Self { root_table: RwLock::new(root_table), depth }
    }

    pub fn from_other(other: &AddressSpace) -> Self {
        let address_space = AddressSpace::new(other.depth);

        {
            let root_table_guard = address_space.root_table.write();
            let root_table = unsafe { root_table_guard.as_mut().unwrap() };
            let other_root_table_guard = other.root_table.read();
            let other_root_table = unsafe { other_root_table_guard.as_ref().unwrap() };

            AddressSpace::copy_table(other_root_table, root_table, other.depth);
        }

        return address_space;
    }

    pub fn load(&self) {
        unsafe { Cr3::write(PhysFrame::from_start_address(self.page_table_address()).unwrap(), Cr3Flags::empty()) };
    }

    pub fn page_table_address(&self) -> PhysAddr {
        // Get root table pointer without locking.
        // We cannot use the lock here, because this function is called by the scheduler.
        // This is still safe, since we only return an address and not a reference.
        let root_table = unsafe { self.root_table.as_mut_ptr().read() };
        PhysAddr::new(root_table as u64)
    }

    pub fn map(&self, pages: PageRange, space: MemorySpace, flags: PageTableFlags) {
        let depth = self.depth;
        let root_table_guard = self.root_table.write();
        let root_table = unsafe { root_table_guard.as_mut().unwrap() };
        let frames = PhysFrameRange { start: PhysFrame::from_start_address(PhysAddr::zero()).unwrap(), end: PhysFrame::from_start_address(PhysAddr::zero()).unwrap() };

        AddressSpace::map_in_table(root_table, frames, pages, space, flags, depth);
    }

    pub fn map_physical(&self, frames: PhysFrameRange, pages: PageRange, space: MemorySpace, flags: PageTableFlags) {
        let depth = self.depth;
        let root_table_guard = self.root_table.write();
        let root_table = unsafe { root_table_guard.as_mut().unwrap() };

        assert_eq!(frames.end - frames.start, pages.end - pages.start);
        AddressSpace::map_in_table(root_table, frames, pages, space, flags, depth);
    }

    pub fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        let depth = self.depth;
        let root_table_guard = self.root_table.read();
        let root_table = unsafe { root_table_guard.as_mut().unwrap() };

        AddressSpace::translate_in_table(root_table, addr, depth)
    }

    pub fn unmap(&self, pages: PageRange) {
        let depth = self.depth;
        let root_table_guard = self.root_table.read();
        let root_table = unsafe { root_table_guard.as_mut().unwrap() };

        AddressSpace::unmap_in_table(root_table, pages, depth);
    }

    fn copy_table(source: &PageTable, target: &mut PageTable, level: usize) {
        if level > 1 { // On all levels larger than 1, we allocate new page frames
            for (index, target_entry) in target.iter_mut().enumerate() {
                let source_entry = &source[index];
                if source_entry.is_unused() { // Skip empty entries
                    continue;
                }

                let phys_frame = physical::alloc(1).start;
                let flags = source[index].flags();
                target_entry.set_frame(phys_frame, flags);

                let next_level_source = unsafe { (source_entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                let next_level_target = unsafe { (target_entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                AddressSpace::copy_table(next_level_source, next_level_target, level - 1);
            }
        } else { // Only on the last level, we create a 1:1 copy of the page table
            for (index, target_entry) in target.iter_mut().enumerate() {
                let source_entry = &source[index];
                target_entry.set_addr(source_entry.addr(), source_entry.flags());
            }
        }
    }

    fn map_in_table(table: &mut PageTable, mut frames: PhysFrameRange, mut pages: PageRange, space: MemorySpace, flags: PageTableFlags, level: usize) -> usize {
        let mut total_allocated_pages: usize = 0;
        let start_index = usize::from(page_table_index(pages.start.start_address(), level));

        if level > 1 { // Calculate next level page table until level == 1
            for entry in table.iter_mut().skip(start_index) {
                let next_level_table;
                if entry.is_unused() { // Entry is empty -> Allocate new page frame
                    let phys_frame = physical::alloc(1).start;
                    entry.set_frame(phys_frame, flags);

                    next_level_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                    next_level_table.zero();
                } else {
                    next_level_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                }

                let allocated_pages = AddressSpace::map_in_table(next_level_table, frames, pages, space, flags, level - 1);
                pages = PageRange { start: pages.start + allocated_pages as u64, end: pages.end };
                total_allocated_pages += allocated_pages;

                if frames.end > frames.start {
                    frames = PhysFrameRange { start: frames.start + allocated_pages as u64, end: frames.end };
                }

                if pages.start >= pages.end {
                    break;
                }
            }
        } else { // Reached level 1 page table
            total_allocated_pages += match space {
                MemorySpace::Kernel => AddressSpace::identity_map_kernel(table, pages, flags),
                MemorySpace::User => {
                    if frames.start == frames.end {
                        AddressSpace::map_user(table, pages, flags)
                    } else {
                        AddressSpace::map_user_physical(table, frames, pages, flags)
                    }
                }
            }
        }

        return total_allocated_pages;
    }

    fn unmap_in_table(table: &mut PageTable, mut pages: PageRange, level: usize) -> usize {
        let mut total_freed_pages: usize = 0;
        let start_index = usize::from(page_table_index(pages.start.start_address(), level));

        if level > 1 { // Calculate next level page table until level == 1
            for entry in table.iter_mut().skip(start_index) {
                if entry.is_unused() {
                    continue;
                }

                let next_level_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                let freed_pages = AddressSpace::unmap_in_table(next_level_table, pages, level - 1);
                pages = PageRange { start: pages.start + freed_pages as u64, end: pages.end };
                total_freed_pages += freed_pages;

                if AddressSpace::is_table_empty(next_level_table) {
                    let table_frame = PhysFrame::from_start_address(entry.addr()).unwrap();
                    unsafe { physical::free(PhysFrameRange { start: table_frame, end: table_frame + 1 }); }
                    entry.set_unused();
                }

                if pages.start >= pages.end {
                    break;
                }
            }
        } else { // Reached level 1 page table
            let start_index = usize::from(page_table_index(pages.start.start_address(), 1));
            let free_count = min((pages.end - pages.start) as usize, 512 - start_index);

            for (count, entry) in table.iter_mut().skip(start_index).enumerate() {
                if count >= free_count {
                    break;
                }

                if !entry.is_unused() {
                    let frame = PhysFrame::from_start_address(entry.addr()).unwrap();
                    unsafe { physical::free(PhysFrameRange { start: frame, end: frame + 1 }); }
                    entry.set_unused();
                }
            }

            return free_count;
        }

        return total_freed_pages;
    }

    fn drop_table(table: &mut PageTable, level: usize) {
        if level > 1 { // Calculate next level page table until level == 1
            for entry in table.iter_mut() {
                if entry.addr() == PhysAddr::zero() {
                    continue;
                }

                let next_level_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
                AddressSpace::drop_table(next_level_table, level - 1);
            }

            let table_frame = PhysFrame::from_start_address(PhysAddr::new(ptr::from_ref(table) as u64)).unwrap();
            unsafe { physical::free(PhysFrameRange { start: table_frame, end: table_frame + 1 }); }
        }
    }

    fn translate_in_table(table: &mut PageTable, addr: VirtAddr, level: usize) -> Option<PhysAddr> {
        let aligned_addr = addr.align_down(PAGE_SIZE as u64);
        let index = usize::from(page_table_index(aligned_addr, level));
        let entry = &table[index];
        if entry.is_unused() {
            return None;
        }

        if level > 1 { // Calculate next level page table until level == 1
            let next_level_table = unsafe { (entry.addr().as_u64() as *mut PageTable).as_mut().unwrap() };
            return AddressSpace::translate_in_table(next_level_table, addr, level - 1);
        } else { // Reached level 1 page table
            return Some(entry.addr() + (addr - aligned_addr));
        }
    }

    fn identity_map_kernel(table: &mut PageTable, pages: PageRange, flags: PageTableFlags) -> usize {
        let start_index = usize::from(page_table_index(pages.start.start_address(), 1));
        let alloc_count = min((pages.end - pages.start) as usize, 512 - start_index);
        let mut frame_addr = PhysAddr::new(pages.start.start_address().as_u64());

        for (count, entry) in table.iter_mut().skip(start_index).enumerate() {
            if count >= alloc_count {
                break;
            }

            entry.set_addr(frame_addr, flags);
            frame_addr = frame_addr + PAGE_SIZE;
        }

        return alloc_count;
    }

    fn map_user(table: &mut PageTable, pages: PageRange, flags: PageTableFlags) -> usize {
        let start_index = usize::from(page_table_index(pages.start.start_address(), 1));
        let alloc_count = min((pages.end - pages.start) as usize, 512 - start_index);

        for (count, entry) in table.iter_mut().skip(start_index).enumerate() {
            if count >= alloc_count {
                break;
            }

            let phys_frame = physical::alloc(1).start;
            entry.set_frame(phys_frame, flags);
        }

        return alloc_count;
    }

    fn map_user_physical(table: &mut PageTable, frames: PhysFrameRange, pages: PageRange, flags: PageTableFlags) -> usize {
        let start_index = usize::from(page_table_index(pages.start.start_address(), 1));
        let alloc_count = min((pages.end - pages.start) as usize, 512 - start_index);
        let mut frame_iter = frames.into_iter().skip(start_index);

        for (count, entry) in table.iter_mut().skip(start_index).enumerate() {
            if count >= alloc_count {
                break;
            }

            entry.set_frame(frame_iter.next().unwrap(), flags);
        }

        return alloc_count;
    }

    fn is_table_empty(table: &PageTable) -> bool {
        for entry in table.iter() {
            if !entry.is_unused() {
                return false;
            }
        }

        return true;
    }
}