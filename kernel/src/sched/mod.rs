//! Round-robin scheduler across all online cores.
//!
//! One global thread table under one lock, and a per-CPU `current` reached
//! through `TPIDR_EL1`. Threads are not pinned: whichever core reaches the
//! scheduler first takes the next runnable thread, so work spreads without a
//! balancer. A thread already running elsewhere is skipped — `on_cpu` is what
//! keeps two cores from picking the same one.
//!
//! Per-CPU run queues are the obvious next step and deliberately not here yet:
//! they buy lock throughput this kernel has no way to measure a need for, and
//! they cost the property that makes this version easy to reason about, which
//! is that there is exactly one place a thread's state can change.
//!
//! Preemption happens from each core's timer IRQ, which is why `Context` only
//! holds callee-saved state: the interrupted thread's full register set is
//! already in the exception frame on its own stack, so swapping SP swaps
//! everything.

use crate::mm::{frames, phys_to_virt, PAGE_SIZE};
use crate::smp;
use crate::sync::{irq_restore, irq_save, SpinLock};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Kernel stack per thread. 16 KiB is generous for stage 3 and cheap to shrink.
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
    /// Created but not yet released to the run queue. A user thread lives here
    /// between `spawn_stopped` and `start`, because on a multicore machine
    /// another core will happily run a thread the instant it is visible —
    /// before the thread has been told which process, and which address space,
    /// it belongs to.
    New,
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
            State::New => "new",
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
    /// The core running it right now, so no two cores pick the same thread.
    pub on_cpu: Option<usize>,
    /// Pinned to one core. Only idle threads use this.
    pub affinity: Option<usize>,
    /// Bitmask of cores this thread has ever run on. Proof of migration.
    pub cpus_seen: u64,
    /// The process this thread runs for, if it is a user thread.
    pub pid: Option<usize>,
    /// TTBR0 to install when this thread runs.
    pub ttbr0: u64,
    #[allow(dead_code)] // freed when threads become reapable
    stack_phys: usize,
}

struct Scheduler {
    threads: Vec<*mut Thread>,
}

// Safety: the Vec of raw pointers is only reachable through the SpinLock, and
// threads are leaked for the lifetime of the kernel.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler { threads: Vec::new() });
static STARTED: AtomicBool = AtomicBool::new(false);

/// Times an idle thread woke from WFI. Non-zero proves cores actually stop.
static IDLE_WAKEUPS: AtomicU64 = AtomicU64::new(0);

/// Adopt the context `kernel_main` is running on as thread 0, then create an
/// idle thread for the boot core. Must be called once, before `spawn`.
pub fn init() {
    let boot = Box::into_raw(Box::new(Thread {
        id: 0,
        name: "boot",
        ctx: Context::empty(),
        state: State::Runnable,
        slices: 1,
        on_cpu: Some(0),
        affinity: None,
        cpus_seen: 1,
        pid: None,
        ttbr0: crate::mm::paging::empty_ttbr0(),
        stack_phys: 0, // the boot stack came from the linker, not the allocator
    }));

    {
        let mut s = SCHED.lock();
        s.threads.push(boot);
    }

    let cpu = smp::this_cpu();
    cpu.current = 0;

    let idle = spawn_pinned("idle0", idle_entry, 0, Some(0)).expect("cannot create idle thread");
    start(idle);
    cpu.idle = idle;
    STARTED.store(true, Ordering::Release);
}

/// Turn the context a secondary core booted on into that core's idle thread.
///
/// The boot stack `smp` allocated becomes the idle stack, so a secondary needs
/// no second allocation and no trampoline: it is already running the thread.
pub fn adopt_as_idle(cpu_id: usize) {
    let name: &'static str = match cpu_id {
        1 => "idle1",
        2 => "idle2",
        3 => "idle3",
        4 => "idle4",
        5 => "idle5",
        6 => "idle6",
        7 => "idle7",
        _ => "idle",
    };

    let mut s = SCHED.lock();
    let id = s.threads.len();
    s.threads.push(Box::into_raw(Box::new(Thread {
        id,
        name,
        ctx: Context::empty(),
        state: State::Runnable,
        slices: 1,
        on_cpu: Some(cpu_id),
        affinity: Some(cpu_id),
        cpus_seen: 1 << cpu_id,
        pid: None,
        ttbr0: crate::mm::paging::empty_ttbr0(),
        stack_phys: 0,
    })));
    drop(s);

    let cpu = smp::this_cpu();
    cpu.current = id;
    cpu.idle = id;
    cpu.cursor = id;
}

/// Create a runnable kernel thread. Returns its index in the thread table.
pub fn spawn(name: &'static str, entry: fn(usize), arg: usize) -> Option<usize> {
    let id = spawn_pinned(name, entry, arg, None)?;
    start(id);
    Some(id)
}

/// Create a thread that will not run until `start` is called.
pub fn spawn_stopped(name: &'static str, entry: fn(usize), arg: usize) -> Option<usize> {
    spawn_pinned(name, entry, arg, None)
}

/// Release a thread created by `spawn_stopped` into the run queue.
pub fn start(tid: usize) {
    let s = SCHED.lock();
    if let Some(&t) = s.threads.get(tid) {
        unsafe {
            if (*t).state == State::New {
                (*t).state = State::Runnable;
            }
        }
    }
}

fn spawn_pinned(
    name: &'static str,
    entry: fn(usize),
    arg: usize,
    affinity: Option<usize>,
) -> Option<usize> {
    let stack_phys = frames::alloc_contiguous(STACK_PAGES)?;
    let stack_top = phys_to_virt(stack_phys + STACK_PAGES * PAGE_SIZE);

    let mut s = SCHED.lock();
    let id = s.threads.len();
    s.threads.push(Box::into_raw(Box::new(Thread {
        id,
        name,
        ctx: Context::new(entry as usize, arg, stack_top),
        state: State::New,
        slices: 0,
        on_cpu: None,
        affinity,
        cpus_seen: 0,
        pid: None,
        ttbr0: crate::mm::paging::empty_ttbr0(),
        stack_phys,
    })));
    Some(id)
}

/// Is `t` a candidate for `cpu` right now?
fn eligible(t: *mut Thread, cpu_id: usize) -> bool {
    unsafe {
        (*t).state == State::Runnable
            && (*t).on_cpu.is_none()
            && match (*t).affinity {
                Some(a) => a == cpu_id,
                None => true,
            }
    }
}

/// Next thread for this core: the first eligible one after its cursor, falling
/// back to its own idle thread.
fn pick_next(s: &Scheduler, cpu: &smp::Cpu) -> usize {
    let n = s.threads.len();
    for step in 1..=n {
        let i = (cpu.cursor + step) % n;
        if i == cpu.idle || i == cpu.current {
            continue;
        }
        if eligible(s.threads[i], cpu.id) {
            return i;
        }
    }
    // Nothing else to run: keep going if we still can, otherwise idle.
    let cur = s.threads[cpu.current];
    if unsafe { (*cur).state } == State::Runnable && cpu.current != cpu.idle {
        cpu.current
    } else {
        cpu.idle
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
    let cpu = smp::this_cpu();

    let (prev, next) = {
        let mut s = SCHED.lock();
        let next = pick_next(&s, cpu);
        if next == cpu.current {
            drop(s);
            irq_restore(daif);
            return;
        }
        let prev = s.threads[cpu.current];
        let next_ptr = s.threads[next];
        unsafe {
            (*next_ptr).on_cpu = Some(cpu.id);
            (*next_ptr).slices += 1;
            (*next_ptr).cpus_seen |= 1 << cpu.id;
        }
        // `prev` stays claimed across the switch. It is released by whichever
        // thread this core runs next, once `cpu_switch_to` has actually saved
        // prev's context — see `Cpu::release_prev`.
        cpu.release_prev = prev as usize;
        cpu.cursor = next;
        cpu.current = next;
        cpu.switches += 1;
        let _ = &mut s;
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

    // Reached again when some core switches back to this thread — not
    // necessarily the core we left from, so everything below re-reads it.
    finish_switch();
    irq_restore(daif);
}

/// Release the thread this core switched away from.
///
/// Runs in the context of the thread that was switched *to*, which is the first
/// moment the previous thread's saved context is complete and it is safe for
/// another core to pick it up.
#[no_mangle]
pub extern "C" fn finish_switch() {
    let cpu = smp::this_cpu();
    let prev = cpu.release_prev;
    if prev == 0 {
        return;
    }
    cpu.release_prev = 0;
    let _s = SCHED.lock();
    unsafe { (*(prev as *mut Thread)).on_cpu = None };
}

/// Called from each core's timer IRQ. Every tick is a preemption point.
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

/// Yield without waiting for the tick.
pub fn yield_now() {
    schedule();
}

/// Leave the run queue for `ticks` scheduler ticks.
pub fn sleep_ticks(ticks: u64) {
    let until = crate::time::ticks() + ticks;
    with_current(|t| t.state = State::Sleeping(until));
    while crate::time::ticks() < until {
        schedule();
    }
}

/// Announce the intention to block, without giving up the CPU yet.
///
/// Blocking has to be done in two steps or wakeups get lost. A thread that
/// checks its condition, finds nothing, and *then* marks itself blocked can be
/// woken in between — by a sender on another core — and that wake lands on a
/// thread that is still runnable, so it does nothing. The thread then blocks
/// with its condition already satisfied, and waits for a second wake that is
/// never coming.
///
/// The discipline is: `prepare_block`, then re-check the condition, then
/// either `cancel_block` or `block`. A wake arriving anywhere in that window
/// makes the thread runnable, and `block` returns immediately.
pub fn prepare_block(token: u64) {
    with_current(|t| t.state = State::Blocked(token));
}

/// Abandon a prepared block: the condition was satisfied after all.
pub fn cancel_block() {
    with_current(|t| {
        if matches!(t.state, State::Blocked(_)) {
            t.state = State::Runnable;
        }
    });
}

/// Give up the CPU until the prepared block is woken.
pub fn block(token: u64) {
    loop {
        schedule();
        let still_blocked = with_current(|t| t.state == State::Blocked(token));
        if !still_blocked {
            return;
        }
    }
}

/// Prepare and block in one step. Only safe where the condition cannot become
/// true between the two — which is rarer than it looks, so prefer the pair.
pub fn block_on(token: u64) {
    prepare_block(token);
    block(token);
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

/// Run `f` against the thread this core is on, under the scheduler lock.
fn with_current<R>(f: impl FnOnce(&mut Thread) -> R) -> R {
    let daif = irq_save();
    let cpu_current = smp::this_cpu().current;
    let s = SCHED.lock();
    let t = s.threads[cpu_current];
    let r = f(unsafe { &mut *t });
    drop(s);
    irq_restore(daif);
    r
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
    with_current(|t| t.pid)
}

/// Name of the running thread.
pub fn current_name() -> &'static str {
    with_current(|t| t.name)
}

/// Mark the running thread finished and never come back.
#[no_mangle]
pub extern "C" fn thread_exit() -> ! {
    with_current(|t| t.state = State::Finished);
    loop {
        schedule();
    }
}

fn idle_entry(_: usize) {
    idle_loop()
}

/// What a core runs when it has nothing else to do.
pub fn idle_loop() -> ! {
    loop {
        // Nothing runnable: stop the core until an interrupt arrives.
        unsafe { core::arch::asm!("wfi") };
        IDLE_WAKEUPS.fetch_add(1, Ordering::Relaxed);
        schedule();
    }
}

/// How many times an idle thread has woken from WFI, across all cores.
pub fn idle_wakeups() -> u64 {
    IDLE_WAKEUPS.load(Ordering::Relaxed)
}

/// Threads that have not finished, excluding idle threads.
pub fn live_count() -> usize {
    let s = SCHED.lock();
    s.threads
        .iter()
        .filter(|&&t| unsafe { (*t).affinity.is_none() && (*t).state != State::Finished })
        .count()
}

/// Run `f` over every thread, for reporting.
pub fn for_each(mut f: impl FnMut(&Thread)) {
    let s = SCHED.lock();
    for &t in s.threads.iter() {
        f(unsafe { &*t });
    }
}
