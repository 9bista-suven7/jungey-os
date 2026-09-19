//! Interrupt dispatch.
//!
//! Stage 1 handles exactly one interrupt — the scheduler tick — and counts
//! everything else. Stage 3 turns this into a table that forwards an IRQ to the
//! userspace driver holding the matching `Irq` capability, which is why the
//! dispatch point is its own module rather than a branch inside the vector.

use crate::sync::SpinLock;
use crate::{gic, sched, time};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

static SPURIOUS: AtomicU64 = AtomicU64::new(0);
static UNCLAIMED: AtomicU64 = AtomicU64::new(0);

/// Device interrupt handlers, by INTID.
///
/// A table rather than a match arm because stage 3d replaces each entry with a
/// message to the process holding that interrupt's capability, and the dispatch
/// point should not have to change shape for that.
static HANDLERS: SpinLock<Vec<(u32, fn())>> = SpinLock::new(Vec::new());

/// Claim an interrupt. The handler runs with interrupts masked.
pub fn register(intid: u32, handler: fn()) {
    HANDLERS.lock().push((intid, handler));
}

/// Interrupts owned by a userspace driver, with a count of how many have
/// arrived. The count is the message: a driver asks "has it fired since I last
/// looked", which is the only question a level-triggered line can answer
/// honestly after the fact.
static USER_IRQS: SpinLock<Vec<(u32, u64)>> = SpinLock::new(Vec::new());

/// Wake token for a userspace interrupt.
pub const fn token(intid: u32) -> u64 {
    0x1249_0000_0000 | intid as u64
}

/// Hand an interrupt to userspace. The kernel will never handle it again.
pub fn register_user(intid: u32) {
    let mut u = USER_IRQS.lock();
    if !u.iter().any(|&(i, _)| i == intid) {
        u.push((intid, 0));
    }
}

/// How many times `intid` has fired.
pub fn sequence(intid: u32) -> Option<u64> {
    USER_IRQS.lock().iter().find(|&&(i, _)| i == intid).map(|&(_, n)| n)
}

/// Record an interrupt destined for userspace and mask it until the driver
/// comes back for the next one.
fn user_irq_fired(intid: u32) -> bool {
    let mut u = USER_IRQS.lock();
    let Some(slot) = u.iter_mut().find(|(i, _)| *i == intid) else {
        return false;
    };
    slot.1 += 1;
    drop(u);
    // The kernel cannot quiet the device — only its driver can. Masking the
    // line here is what stops a level-triggered interrupt from re-asserting
    // immediately and starving everything else.
    gic::disable_spi(intid);
    sched::wake_all_on(token(intid));
    true
}

fn handler_for(intid: u32) -> Option<fn()> {
    HANDLERS.lock().iter().find(|&&(i, _)| i == intid).map(|&(_, h)| h)
}

/// Called from the IRQ vector with interrupts masked.
pub fn dispatch() {
    let mut handled = 0u32;
    loop {
        let intid = gic::acknowledge();
        if intid >= 1020 {
            // Reading 1023 is how you learn the queue is empty; only an
            // immediate one means the core was woken for nothing.
            if handled == 0 {
                SPURIOUS.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        handled += 1;

        match intid {
            time::TIMER_INTID => {
                time::rearm();
                gic::end_of_interrupt(intid);
                // Preemption point. Safe here because the full register state,
                // including ELR and SPSR, is already on this thread's stack.
                sched::tick();
            }
            _ => {
                if let Some(h) = handler_for(intid) {
                    h();
                } else if !user_irq_fired(intid) {
                    UNCLAIMED.fetch_add(1, Ordering::Relaxed);
                }
                gic::end_of_interrupt(intid);
            }
        }
    }
}

/// (spurious acknowledgements, interrupts with no handler)
pub fn stats() -> (u64, u64) {
    (
        SPURIOUS.load(Ordering::Relaxed),
        UNCLAIMED.load(Ordering::Relaxed),
    )
}
