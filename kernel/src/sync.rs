//! Kernel synchronisation primitives.
//!
//! One CPU runs kernel code today, but interrupts already preempt it, so a
//! plain `AtomicBool` spinlock is not enough: if an IRQ handler takes a lock the
//! interrupted thread already holds, the core deadlocks against itself. Every
//! lock here masks IRQs for the duration and restores the caller's mask on drop.
//!
//! Stage 3 makes these real multi-core locks; the API does not change.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Read DAIF and mask IRQ+FIQ. Returns the previous DAIF for `irq_restore`.
#[inline]
pub fn irq_save() -> u64 {
    let daif: u64;
    unsafe {
        core::arch::asm!("mrs {}, daif", "msr daifset, #3", out(reg) daif, options(nomem, nostack));
    }
    daif
}

/// Restore a DAIF value saved by `irq_save`.
#[inline]
pub fn irq_restore(daif: u64) {
    unsafe {
        core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
    }
}

/// Run `f` with interrupts masked.
#[inline]
pub fn without_irq<R>(f: impl FnOnce() -> R) -> R {
    let daif = irq_save();
    let r = f();
    irq_restore(daif);
    r
}

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// Safety: access to `data` is serialised by `locked`, and IRQs are masked while
// the lock is held so the holder cannot be preempted into a re-entrant acquire.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> SpinGuard<'_, T> {
        let daif = irq_save();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self, daif }
    }

    /// Acquire without blocking. Used by the panic path, which must never wedge
    /// on a lock whose holder is the code that just panicked.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let daif = irq_save();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinGuard { lock: self, daif })
        } else {
            irq_restore(daif);
            None
        }
    }

    /// Break the lock open. Only for the panic path, where ordering no longer
    /// matters and printing the reason for the panic does.
    ///
    /// # Safety
    /// The caller must be the last code that will ever run on this core.
    pub unsafe fn force(&self) -> &mut T {
        &mut *self.data.get()
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    daif: u64,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        irq_restore(self.daif);
    }
}
