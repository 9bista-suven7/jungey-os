//! Kernel heap — a first-fit free-list allocator with coalescing.
//!
//! Backed by frames from `mm::frames`, addressed through the higher-half linear
//! map. It exists so the kernel can hold `Box`, `Vec` and `String`: thread
//! control blocks, driver state, the capability table later on.
//!
//! Deliberately not a slab or buddy allocator yet. Kernel allocation here is
//! low-rate and long-lived; fragmentation pressure is what would justify those,
//! and it does not exist at this stage. `grow` adds a region at a time so the
//! heap does not have to be contiguous.

use super::{phys_to_virt, PAGE_SIZE};
use crate::sync::SpinLock;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

/// Free block header, stored in the free space itself.
#[repr(C)]
struct Block {
    size: usize,
    next: *mut Block,
}

const BLOCK_SIZE: usize = core::mem::size_of::<Block>();
const MIN_ALIGN: usize = core::mem::align_of::<Block>();

/// Donated regions, kept so the free list can be checked against them.
const MAX_REGIONS: usize = 4;

pub struct Heap {
    /// Address-sorted singly linked list of free blocks.
    head: *mut Block,
    total: usize,
    allocated: usize,
    regions: [(usize, usize); MAX_REGIONS],
    region_count: usize,
}

// Safety: every access goes through the SpinLock below.
unsafe impl Send for Heap {}

impl Heap {
    const fn new() -> Self {
        Heap { head: ptr::null_mut(), total: 0, allocated: 0, regions: [(0, 0); MAX_REGIONS], region_count: 0 }
    }

    /// Donate `[start, start + size)` of kernel-virtual memory to the heap.
    unsafe fn add_region(&mut self, start: usize, size: usize) {
        let start = align_up(start, MIN_ALIGN);
        if size < BLOCK_SIZE {
            return;
        }
        self.total += size;
        if self.region_count < MAX_REGIONS {
            self.regions[self.region_count] = (start, start + size);
            self.region_count += 1;
        }
        self.insert(start as *mut Block, size);
    }

    /// Insert a block, keeping the list address-sorted and merging neighbours.
    unsafe fn insert(&mut self, block: *mut Block, size: usize) {
        (*block).size = size;
        (*block).next = ptr::null_mut();

        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < block as usize {
            prev = cur;
            cur = (*cur).next;
        }

        (*block).next = cur;
        if prev.is_null() {
            self.head = block;
        } else {
            (*prev).next = block;
        }

        // Merge forward, then backward.
        if !cur.is_null() && block as usize + size == cur as usize {
            (*block).size += (*cur).size;
            (*block).next = (*cur).next;
        }
        if !prev.is_null() && prev as usize + (*prev).size == block as usize {
            (*prev).size += (*block).size;
            (*prev).next = (*block).next;
        }
    }

    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(MIN_ALIGN);
        let size = align_up(layout.size().max(BLOCK_SIZE), MIN_ALIGN);

        let mut prev: *mut Block = ptr::null_mut();
        let mut cur = self.head;

        while !cur.is_null() {
            let start = align_up(cur as usize, align);
            let front_pad = start - cur as usize;

            // The aligned allocation has to fit, and any gap it leaves in front
            // has to be big enough to survive as a block of its own.
            if front_pad == 0 || front_pad >= BLOCK_SIZE {
                if front_pad + size <= (*cur).size {
                    let block_end = cur as usize + (*cur).size;
                    let tail = block_end - (start + size);

                    // Unlink, then put back whatever is left on either side.
                    let next = (*cur).next;
                    if prev.is_null() {
                        self.head = next;
                    } else {
                        (*prev).next = next;
                    }

                    if front_pad >= BLOCK_SIZE {
                        self.insert(cur, front_pad);
                    }
                    if tail >= BLOCK_SIZE {
                        self.insert((start + size) as *mut Block, tail);
                    }

                    self.allocated += size;
                    return start as *mut u8;
                }
            }
            prev = cur;
            cur = (*cur).next;
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let size = align_up(layout.size().max(BLOCK_SIZE), MIN_ALIGN);
        self.allocated -= size;
        self.insert(ptr as *mut Block, size);
    }
}

#[inline]
const fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap::new());

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.lock().alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP.lock().dealloc(ptr, layout)
    }
}

/// Claim `pages` frames and hand them to the heap.
pub fn init(pages: usize) -> Result<(), &'static str> {
    let phys = super::frames::alloc_contiguous(pages).ok_or("no contiguous frames for heap")?;
    unsafe { HEAP.lock().add_region(phys_to_virt(phys), pages * PAGE_SIZE) };
    Ok(())
}

/// (bytes given to the heap, bytes currently handed out)
pub fn stats() -> (usize, usize) {
    let h = HEAP.lock();
    (h.total, h.allocated)
}

/// What a walk of the free list found.
pub struct HeapCheck {
    pub blocks: usize,
    pub free_bytes: usize,
    pub error: Option<&'static str>,
    pub at: usize,
}

/// Walk the free list and confirm it is still a sorted, non-overlapping list of
/// blocks inside the donated regions.
///
/// Worth having permanently: heap corruption shows up as a fault somewhere else
/// entirely, often thousands of instructions later, and the only cheap way to
/// attribute it is to check the structure at known-good points.
pub fn check() -> HeapCheck {
    let h = HEAP.lock();
    let mut out = HeapCheck { blocks: 0, free_bytes: 0, error: None, at: 0 };
    let mut cur = h.head;
    let mut last_end = 0usize;

    while !cur.is_null() {
        let addr = cur as usize;
        out.at = addr;

        if addr % MIN_ALIGN != 0 {
            out.error = Some("block is misaligned");
            return out;
        }
        if addr < last_end {
            out.error = Some("free list is not sorted, or blocks overlap");
            return out;
        }
        let size = unsafe { (*cur).size };
        if size < BLOCK_SIZE || size > h.total {
            out.error = Some("block size is impossible");
            return out;
        }
        let in_region = h.regions[..h.region_count]
            .iter()
            .any(|&(s, e)| addr >= s && addr + size <= e);
        if !in_region {
            out.error = Some("block lies outside every donated region");
            return out;
        }

        last_end = addr + size;
        out.free_bytes += size;
        out.blocks += 1;
        if out.blocks > 1_000_000 {
            out.error = Some("free list has a cycle");
            return out;
        }
        cur = unsafe { (*cur).next };
    }
    out
}
