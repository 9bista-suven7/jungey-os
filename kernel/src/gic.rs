//! GICv3 interrupt controller.
//!
//! GICv3 is what every AArch64 SoC worth targeting ships, and unlike v2 its CPU
//! interface is a set of system registers rather than MMIO — so delivering an
//! interrupt to a userspace driver in stage 3 costs a register write, not a
//! trip through a mapped page.
//!
//! Addresses come from the device tree (`arm,gic-v3`), never from a constant.

use crate::mm::phys_to_virt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

// ---- Distributor (GICD) ----
const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_ICENABLER: usize = 0x0180;
const GICD_IPRIORITYR: usize = 0x0400;
const GICD_IROUTER: usize = 0x6000;

const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_EN_GRP1NS: u32 = 1 << 1;
const GICD_CTLR_RWP: u32 = 1 << 31;

// ---- Redistributor (GICR): an RD frame followed by an SGI frame ----
const GICR_CTLR: usize = 0x0000;
const GICR_WAKER: usize = 0x0014;
const GICR_SGI_OFFSET: usize = 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI_OFFSET + 0x0080;
const GICR_ISENABLER0: usize = GICR_SGI_OFFSET + 0x0100;
const GICR_ICENABLER0: usize = GICR_SGI_OFFSET + 0x0180;
const GICR_IPRIORITYR: usize = GICR_SGI_OFFSET + 0x0400;

const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_CTLR_RWP: u32 = 1 << 3;

/// Interrupt priority we give everything for now. Numerically lower is more
/// urgent; the priority mask is set above this so all of it gets through.
const DEFAULT_PRIORITY: u8 = 0xA0;

/// The `intid` returned by an acknowledge when there was nothing to take.
pub const SPURIOUS: u32 = 1023;

const GICR_TYPER: usize = 0x0008;
const GICR_TYPER_LAST: u64 = 1 << 4;
/// Redistributor frame stride: an RD frame plus an SGI frame, 64 KiB each.
const GICR_STRIDE: usize = 0x2_0000;

static GICD: AtomicUsize = AtomicUsize::new(0);
/// Base of the redistributor region, and this core's frame within it.
static GICR_BASE: AtomicUsize = AtomicUsize::new(0);
static GICR_PER_CPU: [AtomicUsize; crate::smp::MAX_CPUS] =
    [const { AtomicUsize::new(0) }; crate::smp::MAX_CPUS];

#[inline]
unsafe fn rd(base: usize, off: usize) -> u32 {
    read_volatile((base + off) as *const u32)
}

#[inline]
unsafe fn wr(base: usize, off: usize, v: u32) {
    write_volatile((base + off) as *mut u32, v)
}

unsafe fn gicd_wait_rwp(gicd: usize) {
    while rd(gicd, GICD_CTLR) & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

unsafe fn gicr_wait_rwp(gicr: usize) {
    while rd(gicr, GICR_CTLR) & GICR_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

/// Find the redistributor frame belonging to `mpidr`.
///
/// Frames are laid out consecutively but not necessarily in MPIDR order, and on
/// a big.LITTLE part the affinities are not dense — so this matches on the
/// affinity GICR_TYPER reports rather than indexing by CPU number.
fn find_redistributor(base: usize, mpidr: u64) -> Option<usize> {
    let want = (((mpidr >> 32) & 0xff) << 24) | (mpidr & 0xff_ffff);
    let mut frame = base;
    loop {
        let typer = unsafe { read_volatile((frame + GICR_TYPER) as *const u64) };
        if typer >> 32 == want {
            return Some(frame);
        }
        if typer & GICR_TYPER_LAST != 0 {
            return None;
        }
        frame += GICR_STRIDE;
    }
}

/// This core's redistributor frame.
fn my_gicr() -> usize {
    GICR_PER_CPU[crate::smp::cpu_id()].load(Ordering::Relaxed)
}

/// Wake this core's redistributor and open its CPU interface. Every core runs
/// this; only CPU 0 also programs the distributor.
fn init_this_cpu() {
    let base = GICR_BASE.load(Ordering::Relaxed);
    let gicr = find_redistributor(base, crate::smp::mpidr())
        .expect("no redistributor frame for this core");
    GICR_PER_CPU[crate::smp::cpu_id()].store(gicr, Ordering::Relaxed);

    unsafe {
        let waker = rd(gicr, GICR_WAKER) & !GICR_WAKER_PROCESSOR_SLEEP;
        wr(gicr, GICR_WAKER, waker);
        while rd(gicr, GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
            core::hint::spin_loop();
        }

        // SGIs and PPIs live in the redistributor, not the distributor, and
        // each core has its own copy of them.
        wr(gicr, GICR_ICENABLER0, 0xFFFF_FFFF);
        wr(gicr, GICR_IGROUPR0, 0xFFFF_FFFF);
        for i in 0..32usize {
            write_volatile((gicr + GICR_IPRIORITYR + i) as *mut u8, DEFAULT_PRIORITY);
        }
        gicr_wait_rwp(gicr);

        // CPU interface: system-register access, then let group 1 through.
        let mut sre: u64;
        core::arch::asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre);          // ICC_SRE_EL1
        core::arch::asm!("msr S3_0_C12_C12_5, {}", "isb", in(reg) sre | 1);
        core::arch::asm!("msr S3_0_C4_C6_0, {}", in(reg) 0xF0u64);         // ICC_PMR_EL1
        core::arch::asm!("msr S3_0_C12_C12_3, {}", in(reg) 0u64);          // ICC_BPR1_EL1
        core::arch::asm!("msr S3_0_C12_C12_7, {}", "isb", in(reg) 1u64);   // ICC_IGRPEN1_EL1
    }
}

/// Bring up this core's redistributor and CPU interface. Secondaries only.
pub fn init_secondary() {
    init_this_cpu();
}

/// Bring up the distributor, CPU 0's redistributor, and its CPU interface.
///
/// `gicd_phys` / `gicr_phys` are the first two `reg` entries of the device
/// tree's `arm,gic-v3` node.
pub fn init(gicd_phys: usize, gicr_phys: usize) -> u32 {
    let gicd = phys_to_virt(gicd_phys);
    GICD.store(gicd, Ordering::Relaxed);
    GICR_BASE.store(phys_to_virt(gicr_phys), Ordering::Relaxed);

    let lines = unsafe {
        // How many SPIs this distributor implements: TYPER.ITLinesNumber.
        let lines = (((rd(gicd, GICD_TYPER) & 0x1f) + 1) * 32) as usize;

        // Affinity routing must be on before IROUTER means anything.
        wr(gicd, GICD_CTLR, GICD_CTLR_ARE_NS);
        gicd_wait_rwp(gicd);

        // SPIs: group 1 non-secure, default priority, disabled, routed to us.
        let mut i = 32usize;
        while i < lines {
            wr(gicd, GICD_ICENABLER + (i / 32) * 4, 0xFFFF_FFFF);
            wr(gicd, GICD_IGROUPR + (i / 32) * 4, 0xFFFF_FFFF);
            i += 32;
        }
        for i in 32..lines {
            write_volatile((gicd + GICD_IPRIORITYR + i) as *mut u8, DEFAULT_PRIORITY);
            write_volatile((gicd + GICD_IROUTER + i * 8) as *mut u64, 0);
        }
        gicd_wait_rwp(gicd);

        wr(gicd, GICD_CTLR, GICD_CTLR_ARE_NS | GICD_CTLR_EN_GRP1NS);
        gicd_wait_rwp(gicd);

        lines as u32
    };

    init_this_cpu();
    lines
}

/// Enable a per-core interrupt (SGI 0-15, PPI 16-31) on this core.
pub fn enable_ppi(intid: u32) {
    let gicr = my_gicr();
    unsafe {
        wr(gicr, GICR_ISENABLER0, 1 << (intid & 31));
        gicr_wait_rwp(gicr);
    }
}

/// Enable a shared peripheral interrupt (SPI, 32 and up).
pub fn enable_spi(intid: u32) {
    let gicd = GICD.load(Ordering::Relaxed);
    unsafe {
        wr(gicd, GICD_ISENABLER + (intid as usize / 32) * 4, 1 << (intid & 31));
        gicd_wait_rwp(gicd);
    }
}

/// Stop delivering a shared peripheral interrupt.
///
/// Used when an interrupt belongs to a userspace driver: the kernel cannot
/// quiet the device, only the driver can, so the line is masked at the GIC on
/// arrival and unmasked when the driver comes back for the next one. Without
/// that, a level-triggered line would re-assert immediately and the core would
/// do nothing but take the same interrupt forever.
pub fn disable_spi(intid: u32) {
    let gicd = GICD.load(Ordering::Relaxed);
    unsafe {
        wr(gicd, GICD_ICENABLER + (intid as usize / 32) * 4, 1 << (intid & 31));
        gicd_wait_rwp(gicd);
    }
}

/// Take the highest-priority pending interrupt. `SPURIOUS` if there is none.
#[inline]
pub fn acknowledge() -> u32 {
    let iar: u64;
    unsafe { core::arch::asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar) }; // ICC_IAR1_EL1
    (iar & 0xFF_FFFF) as u32
}

/// Signal end-of-interrupt, dropping the running priority.
#[inline]
pub fn end_of_interrupt(intid: u32) {
    unsafe { core::arch::asm!("msr S3_0_C12_C12_1, {}", in(reg) intid as u64) }; // ICC_EOIR1_EL1
}
