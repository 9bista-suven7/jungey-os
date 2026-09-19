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

/// Microseconds since boot, from the cycle counter rather than the tick.
///
/// The scheduler tick is 10 ms, which is useless for saying whether a job met a
/// 50 ms deadline. The generic timer counts at tens of megahertz and is the
/// only clock here with the resolution to judge that.
pub fn now_us() -> u64 {
    let f = frequency();
    if f == 0 {
        return 0;
    }
    now() / (f / 1_000_000)
}

/// Milliseconds since `start`.
pub fn uptime_ms() -> u64 {
    ticks() * 1000 / HZ
}

/// Program this core's first interrupt and let it through the GIC. Every core
/// calls this; the generic timer is per-core hardware.
pub fn start() {
    let interval = frequency() / HZ;
    INTERVAL.store(interval, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval);
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64); // enable, unmasked
    }
    gic::enable_ppi(TIMER_INTID);
}

/// Rearm for the next tick. Called from each core's IRQ handler.
///
/// Every core takes its own timer interrupt, but the system tick counter is
/// advanced only by the boot core — otherwise four cores would make the clock
/// run four times as fast, and `sleep_ticks` would be wrong by the core count.
pub fn rearm() {
    let interval = INTERVAL.load(Ordering::Relaxed);
    let cpu = crate::smp::this_cpu();
    cpu.ticks += 1;
    if cpu.id == 0 {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
    unsafe { core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval) };
}
