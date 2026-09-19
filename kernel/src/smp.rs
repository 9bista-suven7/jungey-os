//! Multiprocessor bring-up and per-CPU state.
//!
//! Secondary cores are started through PSCI, the firmware interface every
//! AArch64 platform implements — QEMU emulates it, U-Boot and ATF provide it on
//! real silicon. The kernel never pokes a vendor-specific power controller.
//!
//! Per-CPU state hangs off `TPIDR_EL1`, so `this_cpu()` is one register read
//! rather than a lookup keyed on MPIDR. That matters because the scheduler
//! calls it on every switch.

use crate::mm::{frames, phys_to_virt, virt_to_phys, PAGE_SIZE};
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const MAX_CPUS: usize = 8;

/// Kernel stack for a secondary before it has a thread: 16 KiB, and it becomes
/// that core's idle-thread stack once the scheduler adopts it.
const BOOT_STACK_PAGES: usize = 4;

// ---- PSCI ----------------------------------------------------------------

/// PSCI 0.2 function ids, SMC64 calling convention.
const PSCI_CPU_ON: u64 = 0xC400_0003;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// PSCI return codes we care about.
const PSCI_SUCCESS: i64 = 0;
const PSCI_ALREADY_ON: i64 = -4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PsciMethod {
    Hvc,
    Smc,
    None,
}

static METHOD: SpinLock<PsciMethod> = SpinLock::new(PsciMethod::None);

fn psci_call(func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let method = *METHOD.lock();
    let ret: i64;
    unsafe {
        match method {
            PsciMethod::Hvc => core::arch::asm!(
                "hvc #0",
                inlateout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                options(nostack),
            ),
            PsciMethod::Smc => core::arch::asm!(
                "smc #0",
                inlateout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                options(nostack),
            ),
            PsciMethod::None => return -1,
        }
    }
    ret
}

/// Power the machine off. Used by the crash-consistency test, which needs a
/// clean "hard stop" it can restart from.
pub fn system_off() -> ! {
    psci_call(PSCI_SYSTEM_OFF, 0, 0, 0);
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

// ---- per-CPU state -------------------------------------------------------

#[repr(C, align(64))] // one cache line each: no false sharing between cores
pub struct Cpu {
    pub id: usize,
    pub mpidr: u64,
    /// Index into the scheduler's thread table of what this core is running.
    pub current: usize,
    /// This core's idle thread.
    pub idle: usize,
    /// Where this core resumes its round-robin scan.
    pub cursor: usize,
    pub switches: u64,
    pub ticks: u64,
    pub online: bool,
    /// The thread this core switched away from but has not released yet.
    ///
    /// A thread must stay claimed until its context has actually been saved.
    /// Clearing `on_cpu` before `cpu_switch_to` lets another core pick the
    /// thread up and restore a context that is still being written — two cores
    /// then run the same thread on the same stack. So the core that switched
    /// away hands the thread to whoever it switched *to*, which releases it
    /// once the save is complete. Linux calls this `finish_task_switch`.
    pub release_prev: usize,
}

impl Cpu {
    const fn new() -> Self {
        Cpu {
            id: 0,
            mpidr: 0,
            current: 0,
            idle: 0,
            cursor: 0,
            switches: 0,
            ticks: 0,
            online: false,
            release_prev: 0,
        }
    }
}

static mut CPUS: [Cpu; MAX_CPUS] = [const { Cpu::new() }; MAX_CPUS];

static ONLINE: AtomicUsize = AtomicUsize::new(0);
static DISCOVERED: AtomicUsize = AtomicUsize::new(0);

/// What PSCI hands a secondary in x0. Read physically, before the MMU is on.
#[repr(C)]
struct BootDescriptor {
    stack_top_phys: u64,
    cpu_id: u64,
}

static mut BOOT_DESCRIPTORS: [BootDescriptor; MAX_CPUS] =
    [const { BootDescriptor { stack_top_phys: 0, cpu_id: 0 } }; MAX_CPUS];

extern "C" {
    fn _secondary_start();
}

/// Point `TPIDR_EL1` at this core's block and mark it usable.
fn install_cpu(id: usize, mpidr: u64) -> &'static mut Cpu {
    let cpu = unsafe { &mut (*(&raw mut CPUS))[id] };
    cpu.id = id;
    cpu.mpidr = mpidr;
    unsafe { core::arch::asm!("msr tpidr_el1, {}", in(reg) cpu as *mut Cpu as u64) };
    cpu
}

/// The block for the core running this code.
pub fn this_cpu() -> &'static mut Cpu {
    let p: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) p);
        &mut *(p as *mut Cpu)
    }
}

pub fn cpu_id() -> usize {
    this_cpu().id
}

pub fn mpidr() -> u64 {
    let m: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) m) };
    m & 0xff_00ff_ffff // Aff3..Aff0, dropping the flags in between
}

pub fn online_count() -> usize {
    ONLINE.load(Ordering::Acquire)
}

pub fn discovered_count() -> usize {
    DISCOVERED.load(Ordering::Relaxed)
}

/// Run `f` for each online CPU block.
pub fn for_each(mut f: impl FnMut(&Cpu)) {
    let n = DISCOVERED.load(Ordering::Relaxed);
    for i in 0..n {
        let cpu = unsafe { &(*(&raw const CPUS))[i] };
        if cpu.online {
            f(cpu);
        }
    }
}

/// Adopt the boot core as CPU 0. Called before any secondary exists.
pub fn init_boot_cpu(method: &[u8], cpu_count: usize) {
    *METHOD.lock() = match method {
        b"hvc\0" | b"hvc" => PsciMethod::Hvc,
        b"smc\0" | b"smc" => PsciMethod::Smc,
        _ => PsciMethod::None,
    };
    DISCOVERED.store(cpu_count.min(MAX_CPUS), Ordering::Relaxed);
    let cpu = install_cpu(0, mpidr());
    cpu.online = true;
    ONLINE.store(1, Ordering::Release);
}

pub fn psci_method_name() -> &'static str {
    match *METHOD.lock() {
        PsciMethod::Hvc => "hvc",
        PsciMethod::Smc => "smc",
        PsciMethod::None => "none",
    }
}

/// Start the core whose MPIDR is `target`, as CPU `id`.
///
/// Returns once the core has reported itself online, or with an error if PSCI
/// refused or the core never came up.
pub fn start_cpu(id: usize, target_mpidr: u64) -> Result<(), &'static str> {
    if id == 0 || id >= MAX_CPUS {
        return Err("cpu id out of range");
    }

    let stack = frames::alloc_contiguous(BOOT_STACK_PAGES).ok_or("no frames for a boot stack")?;
    unsafe {
        let d = &mut (*(&raw mut BOOT_DESCRIPTORS))[id];
        d.stack_top_phys = (stack + BOOT_STACK_PAGES * PAGE_SIZE) as u64;
        d.cpu_id = id as u64;
    }
    let descriptor_phys = virt_to_phys(unsafe { &(*(&raw const BOOT_DESCRIPTORS))[id] }
        as *const BootDescriptor as usize) as u64;

    // The entry point PSCI takes is physical: the core arrives with the MMU off.
    let entry = virt_to_phys(_secondary_start as *const () as usize) as u64;

    let before = ONLINE.load(Ordering::Acquire);
    let r = psci_call(PSCI_CPU_ON, target_mpidr, entry, descriptor_phys);
    if r != PSCI_SUCCESS && r != PSCI_ALREADY_ON {
        return Err("PSCI CPU_ON refused");
    }

    // Wait for the core to announce itself. Bounded, so a core that never
    // arrives costs a message rather than a hang.
    for _ in 0..10_000_000 {
        if ONLINE.load(Ordering::Acquire) > before {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("core did not come online")
}

/// Rust entry point for a secondary core, called from `boot.s` with the MMU on.
///
/// # Safety
/// Called once per core, from assembly, with a valid boot descriptor.
#[no_mangle]
pub extern "C" fn secondary_main(descriptor: *const u8) -> ! {
    let id = unsafe { (*(descriptor as *const BootDescriptor)).cpu_id } as usize;

    let cpu = install_cpu(id, mpidr());

    // Kernel threads run with an empty user space, same as on the boot core.
    crate::mm::paging::activate(crate::mm::paging::empty_ttbr0());
    unsafe {
        let mut tcr: u64;
        core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr);
        tcr &= !(1 << 7); // EPD0: TTBR0 walks are live now that paging is up
        core::arch::asm!("msr tcr_el1, {}", "isb", in(reg) tcr);
    }

    crate::exceptions::init();
    crate::gic::init_secondary();

    cpu.online = true;
    ONLINE.fetch_add(1, Ordering::AcqRel);

    // Become this core's idle thread, then start taking work.
    crate::sched::adopt_as_idle(id);
    crate::time::start();
    unsafe { core::arch::asm!("msr daifclr, #3") };

    crate::sched::idle_loop()
}

/// Physical address of a frame run, for callers that need one. Kept here so
/// `start_cpu` and the DMA paths agree on the conversion.
pub fn phys_of(va: usize) -> usize {
    virt_to_phys(va)
}

/// Kernel-virtual address of a physical one.
pub fn virt_of(pa: usize) -> usize {
    phys_to_virt(pa)
}
