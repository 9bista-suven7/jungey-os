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
            _ => match handler_for(intid) {
                Some(h) => {
                    h();
                    gic::end_of_interrupt(intid);
                }
                None => {
                    UNCLAIMED.fetch_add(1, Ordering::Relaxed);
                    gic::end_of_interrupt(intid);
                }
            },
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
