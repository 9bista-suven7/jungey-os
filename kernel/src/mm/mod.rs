//! Memory management.
//!
//! Stage 0 is the physical frame allocator only. Stage 1 adds the page tables,
//! and with them the two allocations that matter for this OS: *model weight
//! pages* (shared, read-only, reclaimable) and *KV-cache pages* (per-session,
//! evictable under pressure). See `os/docs/ARCHITECTURE.md`.

pub mod frames;
pub mod heap;
pub mod paging;
pub mod uaccess;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

/// Base of the higher-half linear map. Every byte of physical RAM is reachable
/// at `pa + PHYS_OFFSET` once boot.s has enabled the MMU, so the kernel never
/// needs a temporary mapping to touch a page it just allocated.
///
/// Must match `PHYS_OFFSET` in `linker.ld`.
pub const PHYS_OFFSET: usize = 0xFFFF_0000_0000_0000;

/// Physical address -> kernel virtual address in the linear map.
#[inline]
pub const fn phys_to_virt(pa: usize) -> usize {
    pa + PHYS_OFFSET
}

/// Kernel virtual address in the linear map -> physical address.
#[inline]
pub const fn virt_to_phys(va: usize) -> usize {
    va - PHYS_OFFSET
}

#[inline]
pub const fn page_align_up(addr: usize) -> usize {
    (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[inline]
pub const fn page_align_down(addr: usize) -> usize {
    addr & !(PAGE_SIZE - 1)
}
