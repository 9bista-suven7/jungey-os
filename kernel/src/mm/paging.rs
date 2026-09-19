//! Page tables and address spaces.
//!
//! Stage 1 mapped all of RAM with four 1 GiB blocks and never touched a table
//! again. Userspace needs the real thing: per-process translation at 4 KiB
//! granularity, with permissions the hardware enforces.
//!
//! An `AddressSpace` owns a level-0 table and everything reachable from it.
//! Kernel mappings are not duplicated into it — they live in TTBR1, which never
//! changes — so switching address spaces is one register write plus a TLB
//! invalidate by ASID, not a page-table rebuild.

use super::{frames, phys_to_virt, PAGE_SIZE};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};


// Descriptor bits, ARM ARM D8.3.
const PTE_VALID: u64 = 1 << 0;
/// At levels 0-2 this means "table"; at level 3 it means "page". Same bit.
const PTE_TABLE_OR_PAGE: u64 = 1 << 1;
const PTE_ATTR_NORMAL: u64 = 1 << 2; // AttrIndx = 1, matching MAIR attr1
const PTE_AP_EL0: u64 = 1 << 6; // AP[1]: accessible at EL0
const PTE_AP_RO: u64 = 1 << 7; // AP[2]: read-only
const PTE_SH_INNER: u64 = 3 << 8;
const PTE_AF: u64 = 1 << 10; // access flag; without it every touch faults
const PTE_NG: u64 = 1 << 11; // non-global: tagged with the ASID
const PTE_PXN: u64 = 1 << 53; // never executable at EL1
const PTE_UXN: u64 = 1 << 54; // never executable at EL0

const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// What a user mapping is allowed to do. Kernel text is never mapped here, so
/// every one of these sets PXN: a bug that jumps to user memory at EL1 faults
/// instead of executing whatever the process put there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    /// Executable at EL0, read-only. Program text.
    UserText,
    /// Readable at EL0, never executable. Constants.
    UserReadOnly,
    /// Read/write at EL0, never executable. Data, BSS, stack.
    UserData,
    /// Device registers: read/write at EL0, never executable, and Device-nGnRnE
    /// so the compiler and the hardware cannot reorder, merge or cache an
    /// access to them.
    UserDevice,
}

impl Perm {
    fn bits(self) -> u64 {
        let common = PTE_VALID | PTE_TABLE_OR_PAGE | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF
            | PTE_NG | PTE_AP_EL0 | PTE_PXN;
        match self {
            Perm::UserText => common | PTE_AP_RO,
            Perm::UserReadOnly => common | PTE_AP_RO | PTE_UXN,
            Perm::UserData => common | PTE_UXN,
            // Device memory takes MAIR attr0, so the normal-memory AttrIndx and
            // the shareability bits both come off.
            Perm::UserDevice => {
                (common & !PTE_ATTR_NORMAL & !PTE_SH_INNER) | PTE_UXN
            }
        }
    }
}

fn index(va: usize, level: usize) -> usize {
    (va >> (39 - level * 9)) & 0x1ff
}

/// Allocate a zeroed frame for use as a page table.
fn alloc_table() -> Option<usize> {
    let pa = frames::alloc()?;
    unsafe { core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, PAGE_SIZE) };
    Some(pa)
}

/// Next ASID to hand out. 0 is reserved for the kernel's empty space.
static NEXT_ASID: AtomicU64 = AtomicU64::new(1);

pub struct AddressSpace {
    root: usize, // physical address of the L0 table
    asid: u64,
    /// Every frame this space owns: tables and mapped pages alike.
    owned: Vec<usize>,
}

impl AddressSpace {
    pub fn new() -> Option<Self> {
        let root = alloc_table()?;
        Some(AddressSpace {
            root,
            asid: NEXT_ASID.fetch_add(1, Ordering::Relaxed) & 0xffff,
            owned: vec![root],
        })
    }

    pub fn root(&self) -> usize {
        self.root
    }

    /// TTBR0_EL1 value that selects this space.
    pub fn ttbr0(&self) -> u64 {
        (self.asid << 48) | self.root as u64
    }

    pub fn asid(&self) -> u64 {
        self.asid
    }

    /// Walk to the level-3 entry for `va`, creating tables on the way.
    fn entry_for(&mut self, va: usize) -> Option<*mut u64> {
        let mut table = phys_to_virt(self.root) as *mut u64;
        for level in 0..3 {
            let e = unsafe { &mut *table.add(index(va, level)) };
            if *e & PTE_VALID == 0 {
                let next = alloc_table()?;
                self.owned.push(next);
                *e = next as u64 | PTE_VALID | PTE_TABLE_OR_PAGE;
            }
            table = phys_to_virt((*e & ADDR_MASK) as usize) as *mut u64;
        }
        Some(unsafe { table.add(index(va, 3)) })
    }

    /// Map `pages` 4 KiB pages of `pa` at `va`.
    pub fn map(&mut self, va: usize, pa: usize, pages: usize, perm: Perm) -> Result<(), &'static str> {
        if va % PAGE_SIZE != 0 || pa % PAGE_SIZE != 0 {
            return Err("unaligned mapping");
        }
        for i in 0..pages {
            let e = self
                .entry_for(va + i * PAGE_SIZE)
                .ok_or("out of frames for page tables")?;
            unsafe { *e = (pa + i * PAGE_SIZE) as u64 | perm.bits() };
        }
        self.flush();
        Ok(())
    }

    /// Allocate `pages` fresh zeroed frames and map them at `va`.
    pub fn map_anonymous(&mut self, va: usize, pages: usize, perm: Perm) -> Result<(), &'static str> {
        for i in 0..pages {
            let frame = frames::alloc().ok_or("out of frames")?;
            unsafe { core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE_SIZE) };
            self.owned.push(frame);
            self.map(va + i * PAGE_SIZE, frame, 1, perm)?;
        }
        Ok(())
    }

    /// Physical address backing a user virtual address, if it is mapped.
    pub fn translate(&self, va: usize) -> Option<usize> {
        let mut table = phys_to_virt(self.root) as *const u64;
        for level in 0..3 {
            let e = unsafe { *table.add(index(va, level)) };
            if e & PTE_VALID == 0 {
                return None;
            }
            table = phys_to_virt((e & ADDR_MASK) as usize) as *const u64;
        }
        let e = unsafe { *table.add(index(va, 3)) };
        if e & PTE_VALID == 0 {
            return None;
        }
        Some((e & ADDR_MASK) as usize | (va & (PAGE_SIZE - 1)))
    }

    /// Drop this space's TLB entries. Scoped by ASID, so other spaces survive.
    fn flush(&self) {
        unsafe {
            core::arch::asm!(
                "dsb ishst",
                "tlbi aside1is, {}",
                "dsb ish",
                "isb",
                in(reg) self.asid << 48,
            );
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        for &f in self.owned.iter() {
            frames::free(f);
        }
    }
}

/// TTBR0 value for "no user address space": an empty table, so a stray low
/// access is a translation fault rather than whatever the last process mapped.
static EMPTY_TTBR0: AtomicU64 = AtomicU64::new(0);

/// Build the empty user space and start honouring TTBR0.
///
/// Stage 1 left `TCR_EL1.EPD0` set so the low half faulted outright. Userspace
/// needs TTBR0 walks, so the guarantee moves from "no walk" to "walk an empty
/// table" — same fault, and now per-process.
pub fn init() -> Result<(), &'static str> {
    let empty = alloc_table().ok_or("no frame for the empty address space")?;
    EMPTY_TTBR0.store(empty as u64, Ordering::Relaxed);
    activate(empty as u64);
    unsafe {
        let mut tcr: u64;
        core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr);
        tcr &= !(1 << 7); // clear EPD0
        core::arch::asm!("msr tcr_el1, {}", "isb", in(reg) tcr);
    }
    Ok(())
}

/// TTBR0 value that maps nothing.
pub fn empty_ttbr0() -> u64 {
    EMPTY_TTBR0.load(Ordering::Relaxed)
}

/// Install a TTBR0 value. Cheap: no table walk, no full TLB flush.
pub fn activate(ttbr0: u64) {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {}",
            "isb",
            in(reg) ttbr0,
        );
    }
}
