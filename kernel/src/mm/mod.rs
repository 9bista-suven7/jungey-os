//! Memory management.
//!
//! Stage 0 is the physical frame allocator only. Stage 1 adds the page tables,
//! and with them the two allocations that matter for this OS: *model weight
//! pages* (shared, read-only, reclaimable) and *KV-cache pages* (per-session,
//! evictable under pressure). See `os/docs/ARCHITECTURE.md`.

pub mod frames;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

#[inline]
pub const fn page_align_up(addr: usize) -> usize {
    (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[inline]
pub const fn page_align_down(addr: usize) -> usize {
    addr & !(PAGE_SIZE - 1)
}
