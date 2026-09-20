//! ARM generic timer — the scheduler's heartbeat.
//!
//! Uses the virtual timer (CNTV), which is PPI 11 in device tree numbering and
//! therefore INTID 27 at the GIC. The virtual timer is the right one: it keeps
//! working unchanged if this kernel is ever run under a hypervisor.

use crate::gic;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// INTID of the EL1 virtual timer: PPI 11, and PPIs start at 16.
pub const TIMER_INTID: u32 = 27;

/// Scheduler tick rate.
pub const HZ: u64 = 100;

static INTERVAL: AtomicU64 = AtomicU64::new(0);
/// The counter value the first core saw. Everything since is measured from
/// here, by subtraction, rather than by counting interrupts.
static BOOT_CYCLES: AtomicU64 = AtomicU64::new(0);
/// Timer interrupts actually taken, across all cores. This is the number
/// tickless idle is trying to make small, so it is worth counting honestly.
static TIMER_IRQS: AtomicU64 = AtomicU64::new(0);

/// Whether an idle core may sleep past its next scheduling tick.
///
/// Runtime rather than compile-time, so the exit test can measure the same
/// machine with it off and on instead of asking you to believe a number from
/// a different build.
static TICKLESS: AtomicBool = AtomicBool::new(false);

/// The longest an idle core will sleep with nothing at all to wake for. Not a
/// correctness bound — a thread becoming runnable sends an interrupt that
/// wakes every core — but a backstop, so a bug that loses a wakeup shows up as
/// a stutter rather than a hang.
const MAX_IDLE_TICKS: u64 = 100; // one second

pub fn set_tickless(on: bool) {
    TICKLESS.store(on, Ordering::Relaxed);
}

pub fn tickless() -> bool {
    TICKLESS.load(Ordering::Relaxed)
}

pub fn timer_irqs() -> u64 {
    TIMER_IRQS.load(Ordering::Relaxed)
}

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

/// Scheduler ticks since boot.
///
/// Computed from the counter, not by counting interrupts. That is what makes
/// tickless idle possible at all: if the clock is the number of times a core
/// was interrupted, then a core that stops being interrupted stops time, and
/// every sleep in the system becomes wrong. Reading the counter means the
/// interrupt is only a *wakeup* — it carries no information the clock needs.
pub fn ticks() -> u64 {
    let interval = INTERVAL.load(Ordering::Relaxed);
    if interval == 0 {
        return 0;
    }
    now().saturating_sub(BOOT_CYCLES.load(Ordering::Relaxed)) / interval
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
    // Whichever core gets here first sets the origin; the others adopt it, so
    // all four agree on what tick it is.
    let _ = BOOT_CYCLES.compare_exchange(0, now(), Ordering::AcqRel, Ordering::Relaxed);
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval);
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64); // enable, unmasked
    }
    gic::enable_ppi(TIMER_INTID);
}

/// Program this core's timer to fire at an absolute counter value.
///
/// CVAL rather than TVAL: a deadline that might be far away is an absolute
/// instant, and computing a delta to it invites getting the sign wrong when
/// it has already passed.
fn arm_at(cycles: u64) {
    unsafe { core::arch::asm!("msr cntv_cval_el0, {}", in(reg) cycles) };
}

/// Decide when this core next needs to be interrupted, and say so.
///
/// With work to run, that is one scheduling quantum: preemption is the whole
/// reason for a periodic tick. With nothing to run, it is whenever the
/// earliest sleeper is due — which may be a hundred quanta away, and there is
/// no reason to wake ninety-nine times to find out nothing has changed.
pub fn arm_next(have_work: bool, earliest_sleeper: Option<u64>) {
    let interval = INTERVAL.load(Ordering::Relaxed);
    if interval == 0 {
        return;
    }
    let origin = BOOT_CYCLES.load(Ordering::Relaxed);
    let now_cycles = now();

    if have_work || !tickless() {
        arm_at(now_cycles + interval);
        return;
    }

    let target = earliest_sleeper.unwrap_or_else(|| ticks() + MAX_IDLE_TICKS);
    let at = origin + target * interval;
    // Never program the past, and never program so close that the interrupt
    // lands before the core has got to its WFI.
    arm_at(at.max(now_cycles + interval / 4));
}

/// Account for a timer interrupt on this core.
///
/// It no longer advances a clock — the clock is the counter — so all this does
/// is count. Rearming is left to `arm_next`, once the scheduler has looked at
/// what there is to do: the answer to "when next?" depends on that, and asking
/// before looking is how a tickless kernel ends up waking every 10 ms anyway.
pub fn took_interrupt() {
    let cpu = crate::smp::this_cpu();
    cpu.ticks += 1;
    TIMER_IRQS.fetch_add(1, Ordering::Relaxed);
}
