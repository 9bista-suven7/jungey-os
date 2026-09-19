//! Jungey OS kernel — AArch64.
//!
//! Stage 1: MMU on with a higher-half kernel, physical frame allocator, kernel
//! heap, GICv3, generic timer, and preemptive round-robin kernel threads.
//!
//! See `os/docs/ARCHITECTURE.md` for where this is going.

#![no_std]
#![no_main]

extern crate alloc;

use core::arch::global_asm;
use core::panic::PanicInfo;

#[macro_use]
pub mod uart;
pub mod cap;
pub mod dtb;
pub mod elf;
pub mod exceptions;
pub mod gic;
pub mod ipc;
pub mod irq;
pub mod mm;
pub mod proc;
pub mod sched;
pub mod syscall;
pub mod sync;
pub mod time;

#[global_allocator]
static ALLOCATOR: mm::heap::KernelAllocator = mm::heap::KernelAllocator;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));
global_asm!(include_str!("sched/switch.s"));

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Pages handed to the kernel heap at boot. Grows on demand later.
const HEAP_PAGES: usize = 256;

/// The userspace image, built from `os/user` by this crate's build script and
/// embedded in the kernel. Stage 3 reads it off a filesystem instead.
static INIT_ELF: &[u8] = include_bytes!(env!("JUNGEY_INIT_ELF"));

extern "C" {
    fn enter_user(entry: usize, user_sp: usize, arg: usize) -> !;
}

const BANNER: &str = r"
    _                              ___  ____
   | |_   _ _ __   __ _  ___ _   _/ _ \/ ___|
 _ | | | | | '_ \ / _` |/ _ \ | | | | | \___ \
| |_| | |_| | | | | (_| |  __/ |_| | |_| |___) |
 \___/ \__,_|_| |_|\__, |\___|\__, |\___/|____/
                   |___/      |___/
";

const RULE: &str = "  ----------------------------------------------------------";

#[no_mangle]
pub extern "C" fn kernel_main(dtb_phys: usize) -> ! {
    println!("{}", BANNER);
    println!("  Jungey OS  v0.3.0  ·  stage 2  ·  aarch64");
    println!("{}", RULE);

    let (kstart, kend) = unsafe {
        (
            &__kernel_start as *const u8 as usize,
            &__kernel_end as *const u8 as usize,
        )
    };

    println!("  image      : {:#018x}..{:#018x}  ({} KiB)", kstart, kend, (kend - kstart) / 1024);
    println!("  phys       : {:#012x}..{:#012x}", mm::virt_to_phys(kstart), mm::virt_to_phys(kend));
    println!("  exec level : EL{}", current_el());
    println!("  mmu        : on, linear map at {:#018x}", mm::PHYS_OFFSET);

    exceptions::init();
    println!("  vectors    : installed at VBAR_EL1");

    let Some(fdt) = dtb::Fdt::new(mm::phys_to_virt(dtb_phys)) else {
        println!("  fdt        : INVALID at {:#012x} — cannot continue", dtb_phys);
        halt()
    };

    println!("  dtb        : {:#012x} phys, {} bytes", dtb_phys, fdt.total_size());
    if let Some(model) = fdt.model() {
        println!("  machine    : {}", model);
    }

    // Re-point the console at whatever the device tree says the PL011 is.
    let mut regs = [(0u64, 0u64); 2];
    if fdt.node_regs("arm,pl011", &mut regs) > 0 {
        uart::set_base(regs[0].0 as usize);
        println!("  console    : pl011 at {:#012x} (from dtb)", regs[0].0);
    }

    // ---- physical memory ----
    let mut total = 0u64;
    for (i, (base, size)) in fdt.memory_regions().enumerate() {
        println!("  ram[{}]     : {:#012x}..{:#012x}  ({} MiB)", i, base, base + size, size / (1024 * 1024));
        total += size;
    }
    println!("  ram total  : {} MiB", total / (1024 * 1024));

    mm::frames::init(
        &fdt,
        &[
            (mm::virt_to_phys(kstart), mm::virt_to_phys(kend)),
            (dtb_phys, dtb_phys + fdt.total_size()),
        ],
    );
    println!(
        "  frames     : {} free of {} ({} MiB usable)",
        mm::frames::free_count(),
        mm::frames::total_count(),
        mm::frames::free_count() * 4096 / (1024 * 1024)
    );

    // ---- kernel heap ----
    mm::heap::init(HEAP_PAGES).expect("heap init");
    let (heap_total, _) = mm::heap::stats();
    println!("  heap       : {} KiB", heap_total / 1024);

    // ---- user address spaces ----
    mm::paging::init().expect("paging init");
    println!("  paging     : ttbr0 live, 4 KiB pages, per-process asids");

    // ---- interrupt controller ----
    let mut gic_regs = [(0u64, 0u64); 2];
    if fdt.node_regs("arm,gic-v3", &mut gic_regs) < 2 {
        println!("  gic        : no arm,gic-v3 node — cannot continue");
        halt()
    }
    let lines = gic::init(gic_regs[0].0 as usize, gic_regs[1].0 as usize);
    println!(
        "  gic        : v3 at {:#012x}/{:#012x}, {} interrupt lines",
        gic_regs[0].0, gic_regs[1].0, lines
    );

    // ---- scheduler and timer ----
    sched::init();
    println!("  sched      : round-robin, thread 0 is '{}'", sched::current_name());

    time::start();
    println!("  timer      : cntv at {} Hz, tick {} Hz, intid {}", time::frequency(), time::HZ, time::TIMER_INTID);

    unsafe { core::arch::asm!("msr daifclr, #3") }; // interrupts live from here
    println!("  irq        : unmasked");
    println!("{}", RULE);

    stage2_demo();

    println!("{}", RULE);
    println!("  stage 2 complete. handing the core to idle.");
    halt()
}

// ---------------------------------------------------------------------------
// Stage 2 exit test: two processes exchange a message they could only exchange
// because each was handed a capability, a third is denied for holding none,
// and revoking the parent capability kills the whole subtree at once.
// ---------------------------------------------------------------------------

/// When the kernel cuts the root capability, in scheduler ticks.
const REVOKE_AT_TICK: u64 = 60;

/// Kernel-side entry for a user thread: install the address space, then leave
/// EL1 for good.
fn user_thread(pid: usize) {
    let p = proc::get(pid).expect("process vanished");
    let (entry, sp, arg, ttbr0) = unsafe {
        ((*p).entry, (*p).stack_top, (*p).arg, (*p).space.ttbr0())
    };
    mm::paging::activate(ttbr0);
    unsafe { enter_user(entry, sp, arg) }
}

fn start_process(name: &'static str, role: usize, cap: Option<cap::Cap>) -> usize {
    let pid = proc::create(name, INIT_ELF, role).expect("create process");
    if let Some(c) = cap {
        proc::install_cap(pid, 0, c).expect("install capability");
    }
    let tid = sched::spawn(name, user_thread, pid).expect("spawn user thread");
    let ttbr0 = unsafe { (*proc::get(pid).unwrap()).space.ttbr0() };
    sched::attach_process(tid, pid, ttbr0);
    pid
}

fn stage2_demo() {
    // One channel, and one capability to it: the root of all authority over it.
    let channel = ipc::create();
    let root = cap::Cap::root(cap::Obj::Channel(channel), cap::RIGHTS_ALL);
    println!("  channel {} created; root capability #{} ({})", channel, root.id, root.rights_str());

    // Derivation only ever narrows. Neither process can reconstruct the other's
    // authority, and neither can widen its own.
    let send_cap = root.derive(cap::RIGHT_SEND);
    let recv_cap = root.derive(cap::RIGHT_RECV);
    println!("  derived #{} ({}) and #{} ({}) from #{}",
        send_cap.id, send_cap.rights_str(), recv_cap.id, recv_cap.rights_str(), root.id);
    println!();

    start_process("receiver", 1, Some(recv_cap));
    start_process("sender", 0, Some(send_cap));
    start_process("intruder", 2, None);
    start_process("trespasser", 3, None);

    // Let the first exchange happen, then cut the root. Sleeping rather than
    // spinning means the core idles while the processes do their work.
    sched::sleep_ticks(REVOKE_AT_TICK.saturating_sub(time::ticks()));
    println!();
    println!("  [kernel  ] revoking root capability #{}", root.id);
    let killed = proc::revoke(root.id);
    println!("  [kernel  ] {} derived capabilities died with it", killed);
    println!();

    while sched::live_count() > 1 {
        sched::yield_now();
    }

    report();
}

fn report() {
    println!();
    println!("  process        pid  exit  capability");
    proc::for_each(|p| {
        let cap_desc = match p.caps[0] {
            Some(c) if c.revoked => alloc::format!("#{} {} REVOKED", c.id, c.rights_str()),
            Some(c) => alloc::format!("#{} {}", c.id, c.rights_str()),
            None => alloc::string::String::from("none"),
        };
        println!("  {:<12} {:>4} {:>5}  {}", p.name, p.pid,
            p.exit_code.unwrap_or(-1), cap_desc);
    });

    println!();
    println!("  thread          state  slices");
    sched::for_each(|t| {
        println!("  {:<12} {:>8}  {:>6}", t.name, t.state.label(), t.slices);
    });

    let (sent, received, queued) = ipc::stats(0);
    let (spurious, unclaimed) = irq::stats();
    let (heap_total, heap_used) = mm::heap::stats();
    println!();
    println!("  channel 0  : {} sent, {} received, {} queued", sent, received, queued);
    println!("  caps       : {} minted", cap::minted());
    println!("  uptime     : {} ms ({} ticks)", time::uptime_ms(), time::ticks());
    println!("  irqs       : {} spurious, {} unclaimed", spurious, unclaimed);
    println!("  heap       : {} of {} bytes in use", heap_used, heap_total);
    println!("  frames     : {} free of {}", mm::frames::free_count(), mm::frames::total_count());

    // Opt-in: prove the MMU actually unmapped the low half by touching it.
    #[cfg(feature = "fault-demo")]
    {
        println!();
        println!("  fault-demo : dereferencing a null pointer on purpose");
        let p = 0usize as *const u64;
        let v = unsafe { core::ptr::read_volatile(p) };
        println!("  fault-demo : UNREACHABLE, read {:#x}", v);
    }
}

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
    el >> 2
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println_forced!("\n*** KERNEL PANIC ***");
    println_forced!("{}", info);
    halt()
}
