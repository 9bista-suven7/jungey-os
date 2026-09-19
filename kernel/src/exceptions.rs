//! Exception handling for EL1.
//!
//! Stage 0 reports and halts. Stage 1 turns entries 8..11 into the syscall and
//! IRQ paths, and the synchronous handler into the page-fault path.

extern "C" {
    static __vectors: u8;
}

/// Everything the vector entry pushed, in the order `vectors.s` pushes it.
/// The scheduler can switch threads on top of one of these, so it has to hold
/// ELR and SPSR too, not just the general-purpose registers.
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 31],
    _pad: u64,
    pub elr: u64,
    pub spsr: u64,
    pub sp_el0: u64,
    _pad2: u64,
}

/// Point VBAR_EL1 at our table and unmask asynchronous aborts.
pub fn init() {
    unsafe {
        let base = &__vectors as *const u8 as u64;
        core::arch::asm!("msr vbar_el1, {}", "isb", in(reg) base);
    }
}

const NAMES: [&str; 16] = [
    "EL1t sync", "EL1t irq", "EL1t fiq", "EL1t serror",
    "EL1h sync", "EL1h irq", "EL1h fiq", "EL1h serror",
    "EL0-64 sync", "EL0-64 irq", "EL0-64 fiq", "EL0-64 serror",
    "EL0-32 sync", "EL0-32 irq", "EL0-32 fiq", "EL0-32 serror",
];

/// Decode the Exception Class field of ESR_ELx into something readable.
fn describe(esr: u64) -> &'static str {
    match esr >> 26 {
        0b000000 => "unknown reason",
        0b000001 => "trapped WFI/WFE",
        0b000111 => "SIMD/FP access trapped",
        0b001110 => "illegal execution state",
        0b010101 => "SVC (syscall) from AArch64",
        0b011000 => "trapped MSR/MRS",
        0b100000 => "instruction abort, lower EL",
        0b100001 => "instruction abort, same EL",
        0b100010 => "PC alignment fault",
        0b100100 => "data abort, lower EL",
        0b100101 => "data abort, same EL",
        0b100110 => "SP alignment fault",
        0b101100 => "floating-point exception",
        0b111100 => "BRK instruction",
        _ => "unclassified",
    }
}

/// Vector index 1, 5, 9 and 13 are the IRQ entries for each of the four
/// exception origins.
const fn is_irq(idx: u64) -> bool {
    idx & 3 == 1
}

/// ESR exception class for an SVC executed in AArch64 state.
const EC_SVC64: u64 = 0b010101;

#[no_mangle]
pub extern "C" fn rust_exception(idx: u64, esr: u64, elr: u64, far: u64, frame: *mut TrapFrame) {
    if is_irq(idx) {
        crate::irq::dispatch();
        return;
    }

    // A system call: synchronous, from a lower exception level.
    if idx == 8 && esr >> 26 == EC_SVC64 {
        // Taking an exception masks interrupts in hardware. Userspace had them
        // enabled, and a syscall that waits — for a message, for a device —
        // must too, or the core it is on stops taking timer interrupts: no
        // preemption, no tick, and every deadline in the system becomes
        // infinite. They are masked again before returning, because the vector
        // restores registers on the assumption that nothing interrupts it.
        unsafe { core::arch::asm!("msr daifclr, #3") };
        crate::syscall::dispatch(unsafe { &mut *frame });
        unsafe { core::arch::asm!("msr daifset, #3") };
        return;
    }

    // A translation fault from EL0 may be a demand-paged region asking for a
    // page rather than a process misbehaving. Interrupts are unmasked because
    // answering it can mean a disk read, which goes out to a driver process.
    if idx == 8 {
        let ec = esr >> 26;
        let translation = matches!(esr & 0x3f, 0b000100..=0b000111);
        if matches!(ec, 0b100000 | 0b100100) && translation {
            unsafe { core::arch::asm!("msr daifclr, #3") };
            let handled = crate::mm::demand_fault(far as usize);
            unsafe { core::arch::asm!("msr daifset, #3") };
            if handled {
                return;
            }
        }
    }

    // Anything else from EL0 kills the process rather than the kernel.
    if idx == 8 {
        let f = unsafe { &*frame };
        println!();
        println!(
            "  !! pid {:?} fault: {} at elr {:#x}, far {:#x}, x30 {:#x}",
            crate::sched::current_pid(), describe(esr), elr, far, f.x[30]
        );
        println!("  !! esr {:#018x} — terminating the process", esr);
        crate::sched::thread_exit();
    }

    println!("\n*** EXCEPTION: {} ***", NAMES[(idx & 15) as usize]);
    println!("  esr_el1 = {:#018x}  ({})", esr, describe(esr));
    println!("  elr_el1 = {:#018x}", elr);
    println!("  far_el1 = {:#018x}", far);
    println!("  frame   = {:#018x}", frame as usize);

    // A data or instruction abort names the address that faulted; the low bits
    // of ESR say why the translation failed.
    let ec = esr >> 26;
    if matches!(ec, 0b100000 | 0b100001 | 0b100100 | 0b100101) {
        println!("  fault   : at {:#018x}, {}", far, fault_status(esr & 0x3f));
    }

    panic!("unhandled exception");
}

/// Decode the Data/Instruction Fault Status Code.
fn fault_status(iss: u64) -> &'static str {
    match iss {
        0b000100..=0b000111 => "translation fault — nothing mapped there",
        0b001001..=0b001011 => "access flag fault",
        0b001101..=0b001111 => "permission fault",
        0b010000 => "synchronous external abort",
        0b100001 => "alignment fault",
        _ => "see ARM ARM D17.2.37",
    }
}
