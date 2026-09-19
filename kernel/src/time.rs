//! ARM generic timer — the scheduler's heartbeat.
//!
//! Uses the virtual timer (CNTV), which is PPI 11 in device tree numbering and
//! therefore INTID 27 at the GIC. The virtual timer is the right one: it keeps
//! working unchanged if this kernel is ever run under a hypervisor.

use crate::gic;
use core::sync::atomic::{AtomicU64, Ordering};

/// INTID of the EL1 virtual timer: PPI 11, and PPIs start at 16.
pub const TIMER_INTID: u32 = 27;

/// Scheduler tick rate.
pub const HZ: u64 = 100;

static INTERVAL: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Counter frequency in Hz, as the firmware programmed it.
pub fn frequency() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f) };
    f
}

/// Cycles since reset.
pub fn now() -> u64 {
    let c: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
    c
}

/// Ticks counted since `start`.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Milliseconds since `start`.
pub fn uptime_ms() -> u64 {
    ticks() * 1000 / HZ
}

/// Program the first interrupt and let it through the GIC.
pub fn start() {
    let interval = frequency() / HZ;
    INTERVAL.store(interval, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval);
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64); // enable, unmasked
    }
    gic::enable_ppi(TIMER_INTID);
}

/// Rearm for the next tick. Called from the IRQ handler.
pub fn rearm() {
    let interval = INTERVAL.load(Ordering::Relaxed);
    TICKS.fetch_add(1, Ordering::Relaxed);
    unsafe { core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval) };
}
