//! Copying across the user/kernel boundary.
//!
//! The kernel never dereferences a user pointer directly. A process can pass
//! any value it likes, so every access is translated through *that process's*
//! page tables first: an unmapped page returns an error instead of faulting the
//! kernel, and a pointer into kernel space simply does not translate.

use super::{paging::AddressSpace, phys_to_virt, PAGE_SIZE};
use alloc::vec::Vec;

/// Largest single transfer, so a bad length cannot make the kernel allocate
/// unboundedly on a process's say-so.
pub const MAX_TRANSFER: usize = 4096;

pub fn copy_from_user(space: &AddressSpace, uva: usize, len: usize) -> Result<Vec<u8>, &'static str> {
    if len > MAX_TRANSFER {
        return Err("transfer too large");
    }
    let mut out = Vec::with_capacity(len);
    let mut done = 0;
    while done < len {
        let va = uva + done;
        let pa = space.translate(va).ok_or("unmapped user address")?;
        let in_page = PAGE_SIZE - (va & (PAGE_SIZE - 1));
        let n = in_page.min(len - done);
        let src = phys_to_virt(pa) as *const u8;
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(src, n) });
        done += n;
    }
    Ok(out)
}

pub fn copy_to_user(space: &AddressSpace, uva: usize, data: &[u8]) -> Result<(), &'static str> {
    let mut done = 0;
    while done < data.len() {
        let va = uva + done;
        let pa = space.translate(va).ok_or("unmapped user address")?;
        let in_page = PAGE_SIZE - (va & (PAGE_SIZE - 1));
        let n = in_page.min(data.len() - done);
        let dst = phys_to_virt(pa) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), dst, n) };
        done += n;
    }
    Ok(())
}
