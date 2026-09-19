//! Round-robin kernel-thread scheduler.
//!
//! Threads here are kernel threads: one address space, no privilege boundary.
//! Stage 2 gives them an `AddressSpace` and an EL0 context; the switch path and
//! the run queue stay as they are.
//!
//! Preemption happens from the timer IRQ, which is why `Context` only holds
//! callee-saved state: the interrupted thread's full register set is already in
//! the exception frame on its own stack, so swapping SP swaps everything.

use crate::mm::{frames, phys_to_virt, PAGE_SIZE};
use crate::sync::{irq_restore, irq_save, SpinLock};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Kernel stack per thread. 16 KiB is generous for stage 1 and cheap to shrink.
const STACK_PAGES: usize = 4;

extern "C" {
    fn cpu_switch_to(prev: *mut Context, next: *const Context);
    fn thread_trampoline();
}

/// Callee-saved state, laid out to match `switch.s`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Context {
    /// x19..x28
    regs: [u64; 10],
    x29: u64,
    x30: u64,
    sp: u64,
}

impl Context {
    const fn empty() -> Self {
        Context { regs: [0; 10], x29: 0, x30: 0, sp: 0 }
    }

    /// A context that, when switched to, enters `thread_trampoline` and from
    /// there calls `entry(arg)` on a stack topped at `stack_top`.
    fn new(entry: usize, arg: usize, stack_top: usize) -> Self {
        let mut c = Context::empty();
        c.regs[0] = entry as u64; // x19
        c.regs[1] = arg as u64; // x20
        c.x30 = thread_trampoline as *const () as usize as u64;
        c.sp = stack_top as u64;
        c
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Runnable,
    /// Off the run queue until the tick counter reaches this value.
    Sleeping(u64),
    /// Off the run queue until someone wakes this token — a channel id today.
    Blocked(u64),
    Finished,
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Runnable => "runnable",
            State::Sleeping(_) => "sleeping",
            State::Blocked(_) => "blocked",
            State::Finished => "finished",
        }
    }
}

pub struct Thread {
    pub id: usize,
    pub name: &'static str,
    pub ctx: Context,
    pub state: State,
    /// Times this thread has been switched to — the round-robin fairness metric.
    pub slices: u64,
    /// The process this thread runs for, if it is a user thread.
    pub pid: Option<usize>,
    /// TTBR0 to install when this thread runs. The empty space for kernel
    /// threads, so a stray low access faults instead of reading whatever
    /// process ran last.
    pub ttbr0: u64,
    #[allow(dead_code)] // freed when threads become reapable in stage 2
    stack_phys: usize,
}

struct Scheduler {
    threads: Vec<*mut Thread>,
    current: usize,
    /// Index of the thread that runs when nothing else can.
    idle: usize,
}

// Safety: the Vec of raw pointers is only reachable through the SpinLock, and
// threads are leaked for the lifetime of the kernel.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    threads: Vec::new(),
    current: 0,
    idle: 0,
});

static STARTED: AtomicBool = AtomicBool::new(false);

/// Times the idle thread woke from WFI. Non-zero proves the core actually
/// stopped rather than spinning through an empty run queue.
static IDLE_WAKEUPS: AtomicU64 = AtomicU64::new(0);

/// Adopt the context `kernel_main` is already running on as thread 0, then
/// create the idle thread. Must be called once, before `spawn`.
pub fn init() {
    let boot = Box::into_raw(Box::new(Thread {
        id: 0,
        name: "boot",
        ctx: Context::empty(),
        state: State::Runnable,
        slices: 1,
        pid: None,
        ttbr0: crate::mm::paging::empty_ttbr0(),
        stack_phys: 0, // the boot stack came from the linker, not the allocator
    }));

    {
        let mut s = SCHED.lock();
        s.threads.push(boot);
        s.current = 0;
    }

    let idle = spawn("idle", idle_thread, 0).expect("cannot create idle thread");
    SCHED.lock().idle = idle;
    STARTED.store(true, Ordering::Release);
}

/// Create a runnable kernel thread. Returns its index in the run queue.
pub fn spawn(name: &'static str, entry: fn(usize), arg: usize) -> Option<usize> {
    let stack_phys = frames::alloc_contiguous(STACK_PAGES)?;
    let stack_top = phys_to_virt(stack_phys + STACK_PAGES * PAGE_SIZE);

    let mut s = SCHED.lock();
    let id = s.threads.len();
    let t = Box::into_raw(Box::new(Thread {
        id,
        name,
        ctx: Context::new(entry as usize, arg, stack_top),
        state: State::Runnable,
        slices: 0,
        pid: None,
        ttbr0: crate::mm::paging::empty_ttbr0(),
        stack_phys,
    }));
    s.threads.push(t);
    Some(id)
}

/// Pick the next runnable thread after `current`, skipping idle unless it is
/// the only candidate left.
fn pick_next(s: &Scheduler) -> usize {
    let n = s.threads.len();
    for step in 1..=n {
        let i = (s.current + step) % n;
        if i == s.idle {
            continue;
        }
        if unsafe { (*s.threads[i]).state } == State::Runnable {
            return i;
        }
    }
    // Nothing else to run: stay put if we still can, otherwise go idle.
    if unsafe { (*s.threads[s.current]).state } == State::Runnable && s.current != s.idle {
        s.current
    } else {
        s.idle
    }
}

/// Give up the CPU. Safe to call with interrupts enabled or masked.
pub fn schedule() {
    if !STARTED.load(Ordering::Acquire) {
        return;
    }

    // Mask for the whole decision *and* the switch: an interrupt landing
    // between the two would schedule on top of a half-finished switch.
    let daif = irq_save();

    let (prev, next) = {
        let mut s = SCHED.lock();
        let next = pick_next(&s);
        if next == s.current {
            drop(s);
            irq_restore(daif);
            return;
        }
        let prev = s.threads[s.current];
        let next_ptr = s.threads[next];
        s.current = next;
        unsafe { (*next_ptr).slices += 1 };
        (prev, next_ptr)
    };

    // Address space first: the new thread must not run a single instruction
    // against the old one's user mappings.
    unsafe {
        if (*next).ttbr0 != (*prev).ttbr0 {
            crate::mm::paging::activate((*next).ttbr0);
        }
        cpu_switch_to(&mut (*prev).ctx, &(*next).ctx)
    };

    // Reached again when someone switches back to this thread.
    irq_restore(daif);
}

/// Called from the timer IRQ. Every tick wakes due sleepers, then preempts.
pub fn tick() {
    let now = crate::time::ticks();
    {
        let s = SCHED.lock();
        for &t in s.threads.iter() {
            unsafe {
                if let State::Sleeping(until) = (*t).state {
                    if now >= until {
                        (*t).state = State::Runnable;
                    }
                }
            }
        }
    }
    schedule();
}

/// Leave the run queue for `ticks` scheduler ticks.
///
/// The point of a real sleep rather than a spin: with every thread asleep the
/// run queue is empty, the idle thread runs, and the core stops in WFI. That is
/// the behaviour a phone lives or dies by.
pub fn sleep_ticks(ticks: u64) {
    let until = crate::time::ticks() + ticks;
    {
        let s = SCHED.lock();
        let cur = s.threads[s.current];
        unsafe { (*cur).state = State::Sleeping(until) };
    }
    while crate::time::ticks() < until {
        schedule();
    }
}

/// How many times the idle thread has woken from WFI.
pub fn idle_wakeups() -> u64 {
    IDLE_WAKEUPS.load(Ordering::Relaxed)
}

/// Leave the run queue until someone wakes `token`.
pub fn block_on(token: u64) {
    {
        let s = SCHED.lock();
        let cur = s.threads[s.current];
        unsafe { (*cur).state = State::Blocked(token) };
    }
    loop {
        schedule();
        let s = SCHED.lock();
        let cur = s.threads[s.current];
        if unsafe { (*cur).state } != State::Blocked(token) {
            return;
        }
    }
}

/// Make every thread blocked on `token` runnable again.
pub fn wake_all_on(token: u64) {
    let s = SCHED.lock();
    for &t in s.threads.iter() {
        unsafe {
            if (*t).state == State::Blocked(token) {
                (*t).state = State::Runnable;
            }
        }
    }
}

/// Bind a thread to a process, so its syscalls resolve against that process's
/// capability table and its user mappings are installed when it is scheduled.
pub fn attach_process(tid: usize, pid: usize, ttbr0: u64) {
    let s = SCHED.lock();
    if let Some(&t) = s.threads.get(tid) {
        unsafe {
            (*t).pid = Some(pid);
            (*t).ttbr0 = ttbr0;
        }
    }
}

/// Process the running thread belongs to.
pub fn current_pid() -> Option<usize> {
    let s = SCHED.lock();
    unsafe { (*s.threads[s.current]).pid }
}

/// Yield without waiting for the tick.
pub fn yield_now() {
    schedule();
}

/// Mark the running thread finished and never come back.
#[no_mangle]
pub extern "C" fn thread_exit() -> ! {
    {
        let s = SCHED.lock();
        let cur = s.threads[s.current];
        unsafe { (*cur).state = State::Finished };
    }
    loop {
        schedule();
    }
}

fn idle_thread(_: usize) {
    loop {
        // Nothing runnable: stop the core until an interrupt arrives.
        unsafe { core::arch::asm!("wfi") };
        IDLE_WAKEUPS.fetch_add(1, Ordering::Relaxed);
        schedule();
    }
}

/// How many threads have not finished, excluding idle.
pub fn live_count() -> usize {
    let s = SCHED.lock();
    s.threads
        .iter()
        .enumerate()
        .filter(|(i, &t)| *i != s.idle && unsafe { (*t).state } != State::Finished)
        .count()
}

/// Run `f` over every thread, for reporting.
pub fn for_each(mut f: impl FnMut(&Thread)) {
    let s = SCHED.lock();
    for &t in s.threads.iter() {
        f(unsafe { &*t });
    }
}

/// Name of the running thread.
pub fn current_name() -> &'static str {
    let s = SCHED.lock();
    unsafe { (*s.threads[s.current]).name }
}
