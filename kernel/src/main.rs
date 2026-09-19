//! Jungey OS kernel — AArch64.
//!
//! Stage 0: boot, console, exception vectors, physical memory discovery.
//! See `os/docs/ARCHITECTURE.md` for where this is going.

#![no_std]
#![no_main]

use core::arch::global_asm;
use core::panic::PanicInfo;

#[macro_use]
pub mod uart;
pub mod dtb;
pub mod exceptions;
pub mod mm;

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("vectors.s"));

extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

const BANNER: &str = r"
    _                              ___  ____
   | |_   _ _ __   __ _  ___ _   _/ _ \/ ___|
 _ | | | | | '_ \ / _` |/ _ \ | | | | | \___ \
| |_| | |_| | | | | (_| |  __/ |_| | |_| |___) |
 \___/ \__,_|_| |_|\__, |\___|\__, |\___/|____/
                   |___/      |___/
";

#[no_mangle]
pub extern "C" fn kernel_main(dtb: usize) -> ! {
    println!("{}", BANNER);
    println!("  Jungey OS  v0.1.0  ·  stage 0  ·  aarch64");
    println!("  ------------------------------------------------");

    let (kstart, kend) = unsafe {
        (
            &__kernel_start as *const u8 as usize,
            &__kernel_end as *const u8 as usize,
        )
    };
    println!(
        "  image      : {:#012x}..{:#012x}  ({} KiB)",
        kstart,
        kend,
        (kend - kstart) / 1024
    );
    println!("  exec level : EL{}", current_el());
    println!("  dtb        : {:#012x}", dtb);

    exceptions::init();
    println!("  vectors    : installed at VBAR_EL1");

    let fdt = dtb::Fdt::new(dtb);
    match fdt {
        Some(fdt) => {
            println!("  fdt        : valid, {} bytes", fdt.total_size());
            if let Some(model) = fdt.model() {
                println!("  machine    : {}", model);
            }
            let mut total = 0u64;
            for (i, (base, size)) in fdt.memory_regions().enumerate() {
                println!(
                    "  ram[{}]     : {:#012x}..{:#012x}  ({} MiB)",
                    i,
                    base,
                    base + size,
                    size / (1024 * 1024)
                );
                total += size;
            }
            println!("  ram total  : {} MiB", total / (1024 * 1024));

            // The DTB lives in RAM above the kernel on QEMU virt — reserve it,
            // or the allocator will hand out the device tree we are reading.
            mm::frames::init(&fdt, &[(kstart, kend), (dtb, dtb + fdt.total_size())]);
            println!(
                "  frames     : {} free of {} ({} MiB usable)",
                mm::frames::free_count(),
                mm::frames::total_count(),
                mm::frames::free_count() * 4096 / (1024 * 1024)
            );

            // Prove the allocator round-trips.
            let a = mm::frames::alloc().expect("frame alloc failed");
            let b = mm::frames::alloc().expect("frame alloc failed");
            println!("  alloc test : got {:#012x} and {:#012x}", a, b);
            mm::frames::free(a);
            mm::frames::free(b);
            println!("  alloc test : freed, {} frames free", mm::frames::free_count());
        }
        None => println!("  fdt        : INVALID or missing — running blind"),
    }

    println!("  ------------------------------------------------");
    println!("  stage 0 complete. idling.");

    halt()
}

fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el) };
    el >> 2
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\n*** KERNEL PANIC ***");
    println!("{}", info);
    halt()
}
