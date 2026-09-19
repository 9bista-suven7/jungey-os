//! Physical frame allocator.
//!
//! Stage 0 strategy: bump-allocate upward through the largest usable RAM region,
//! stepping over reserved ranges (the kernel image, the device tree blob), with
//! freed frames pushed onto an intrusive free list stored in the frames
//! themselves. The MMU is still off, so physical addresses are directly writable.
//!
//! This gets replaced by a buddy allocator once page tables exist — the API here
//! is deliberately the one a buddy allocator can keep.

use super::{page_align_down, page_align_up, PAGE_SIZE};
use crate::dtb::Fdt;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Ranges the allocator must never hand out: kernel image, DTB, and later the
/// firmware-reserved nodes from `/reserved-memory`.
const MAX_RESERVED: usize = 8;

static BUMP: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static FREE_LIST: AtomicUsize = AtomicUsize::new(0);
static FREE_LIST_LEN: AtomicUsize = AtomicUsize::new(0);
static RESERVED_LEN: AtomicUsize = AtomicUsize::new(0);

// Written once during single-core init, read-only afterwards.
static mut RESERVED: [(usize, usize); MAX_RESERVED] = [(0, 0); MAX_RESERVED];

fn reserved() -> &'static [(usize, usize)] {
    let n = RESERVED_LEN.load(Ordering::Relaxed);
    unsafe { core::slice::from_raw_parts((&raw const RESERVED).cast::<(usize, usize)>(), n) }
}

/// If `frame` overlaps a reserved range, return the end of that range.
fn blocked_until(frame: usize) -> Option<usize> {
    let end = frame + PAGE_SIZE;
    reserved()
        .iter()
        .find(|&&(s, e)| frame < e && s < end)
        .map(|&(_, e)| e)
}

/// Claim the biggest DRAM region the device tree reports, minus `reserved_ranges`.
///
/// Each reserved range is a half-open `[start, end)` byte interval; it is rounded
/// outward to whole pages before being applied.
pub fn init(fdt: &Fdt, reserved_ranges: &[(usize, usize)]) {
    let mut n = 0;
    for &(s, e) in reserved_ranges.iter().take(MAX_RESERVED) {
        if e <= s {
            continue;
        }
        unsafe {
            (&raw mut RESERVED).cast::<(usize, usize)>().add(n).write((page_align_down(s), page_align_up(e)));
        }
        n += 1;
    }
    RESERVED_LEN.store(n, Ordering::Relaxed);

    let mut best: Option<(u64, u64)> = None;
    for (base, size) in fdt.memory_regions() {
        if best.map_or(true, |(_, bs)| size > bs) {
            best = Some((base, size));
        }
    }

    let Some((base, size)) = best else { return };
    let start = page_align_up(base as usize);
    let end = page_align_down((base + size) as usize);
    if start >= end {
        return;
    }

    BUMP.store(start, Ordering::Relaxed);
    END.store(end, Ordering::Relaxed);

    // Count what is actually usable, so `total_count` means something.
    let mut usable = 0;
    let mut p = start;
    while p + PAGE_SIZE <= end {
        match blocked_until(p) {
            Some(skip_to) => p = page_align_up(skip_to),
            None => {
                usable += 1;
                p += PAGE_SIZE;
            }
        }
    }
    TOTAL.store(usable, Ordering::Relaxed);
}

/// Hand out one 4 KiB frame. Returns its physical address.
pub fn alloc() -> Option<usize> {
    // Recycled frames first — they are already warm in cache.
    let head = FREE_LIST.load(Ordering::Relaxed);
    if head != 0 {
        let next = unsafe { core::ptr::read_volatile(head as *const usize) };
        FREE_LIST.store(next, Ordering::Relaxed);
        FREE_LIST_LEN.fetch_sub(1, Ordering::Relaxed);
        return Some(head);
    }

    let end = END.load(Ordering::Relaxed);
    loop {
        let cur = BUMP.load(Ordering::Relaxed);
        if cur == 0 || cur + PAGE_SIZE > end {
            return None;
        }
        match blocked_until(cur) {
            Some(skip_to) => BUMP.store(page_align_up(skip_to), Ordering::Relaxed),
            None => {
                BUMP.store(cur + PAGE_SIZE, Ordering::Relaxed);
                return Some(cur);
            }
        }
    }
}

/// Return a frame to the allocator. The frame's first word becomes the link.
pub fn free(frame: usize) {
    debug_assert!(frame % PAGE_SIZE == 0, "frame not page aligned");
    debug_assert!(blocked_until(frame).is_none(), "freeing a reserved frame");
    let head = FREE_LIST.load(Ordering::Relaxed);
    unsafe { core::ptr::write_volatile(frame as *mut usize, head) };
    FREE_LIST.store(frame, Ordering::Relaxed);
    FREE_LIST_LEN.fetch_add(1, Ordering::Relaxed);
}

/// Frames this allocator started with.
pub fn total_count() -> usize {
    TOTAL.load(Ordering::Relaxed)
}

/// Frames available right now.
pub fn free_count() -> usize {
    let bump = BUMP.load(Ordering::Relaxed);
    let end = END.load(Ordering::Relaxed);
    let mut unbumped = 0;
    let mut p = bump;
    while p != 0 && p + PAGE_SIZE <= end {
        match blocked_until(p) {
            Some(skip_to) => p = page_align_up(skip_to),
            None => {
                unbumped += 1;
                p += PAGE_SIZE;
            }
        }
    }
    unbumped + FREE_LIST_LEN.load(Ordering::Relaxed)
}
