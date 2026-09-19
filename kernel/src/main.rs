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
pub mod dtb;
pub mod exceptions;
pub mod gic;
pub mod irq;
pub mod mm;
pub mod sched;
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
    println!("  Jungey OS  v0.2.0  ·  stage 1  ·  aarch64");
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

    stage1_demo();

    println!("{}", RULE);
    println!("  stage 1 complete. handing the core to idle.");
    halt()
}

// ---------------------------------------------------------------------------
// Stage 1 exit test: three threads sharing one core, preempted by the timer.
// ---------------------------------------------------------------------------

const WORKERS: [&str; 3] = ["alpha", "beta", "gamma"];
/// How long the demo runs, in scheduler ticks.
const RUN_TICKS: u64 = 150;

fn worker(id: usize) {
    let name = WORKERS[id];
    let mut iterations: u64 = 0;
    let mut next_report = time::ticks() + 40;

    while time::ticks() < RUN_TICKS {
        iterations = iterations.wrapping_add(1);
        if time::ticks() >= next_report {
            next_report = time::ticks() + 40;
            println!("  [{:>5}] tick {:>3}  {:>10} iterations", name, time::ticks(), iterations);
        }
    }
    println!("  [{:>5}] done  {:>10} iterations", name, iterations);
}

fn stage1_demo() {
    println!("  spawning {} worker threads; timer preempts every {} ms", WORKERS.len(), 1000 / time::HZ);
    println!();

    for (i, name) in WORKERS.iter().enumerate() {
        sched::spawn(name, worker, i).expect("spawn worker");
    }

    // The boot thread is live too, so wait until it is the only one left.
    while sched::live_count() > 1 {
        sched::yield_now();
    }

    // ---- second half: everyone asleep, so the core should actually stop ----
    println!();
    println!("  all workers done. sleeping the boot thread for 50 ticks —");
    println!("  with an empty run queue the core must sit in WFI.");
    let idle_before = sched::idle_wakeups();
    let ticks_before = time::ticks();
    sched::sleep_ticks(50);
    println!(
        "  woke after {} ticks; idle ran {} times while nothing was runnable",
        time::ticks() - ticks_before,
        sched::idle_wakeups() - idle_before
    );

    println!();
    println!("  thread          state  slices");
    sched::for_each(|t| {
        println!("  {:<12} {:>8}  {:>6}", t.name, t.state.label(), t.slices);
    });

    let (spurious, unclaimed) = irq::stats();
    let (heap_total, heap_used) = mm::heap::stats();
    println!();
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
