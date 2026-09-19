// Kernel-thread context switch.
//
// Only the callee-saved registers and SP are saved: everything else is either
// dead across a call or already on the stack in an exception frame. That makes
// a switch 15 instructions, which matters because the timer runs at 100 Hz and
// stage 5's inference scheduler will want a much finer tick than that.

.section ".text", "ax"

// cpu_switch_to(prev: *mut Context, next: *const Context)
.global cpu_switch_to
cpu_switch_to:
    stp     x19, x20, [x0, #0]
    stp     x21, x22, [x0, #16]
    stp     x23, x24, [x0, #32]
    stp     x25, x26, [x0, #48]
    stp     x27, x28, [x0, #64]
    stp     x29, x30, [x0, #80]
    mov     x9, sp
    str     x9,       [x0, #96]

    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x9,       [x1, #96]
    mov     sp, x9
    ret                                 // returns into x30 of the *next* thread

// Where a freshly spawned thread begins. Reached by `ret` above, with the
// entry point in x19 and its argument in x20, because that is what
// Context::new planted there.
.global thread_trampoline
thread_trampoline:
    bl      finish_switch               // release the thread we were switched in over
    msr     daifclr, #3                 // a new thread runs with IRQs enabled
    mov     x0, x20
    blr     x19
    bl      thread_exit
    b       .                           // thread_exit does not return

// enter_user(entry: usize, user_sp: usize, arg: usize) -> !
//
// The one-way door into EL0. TTBR0 is already installed by the caller.
//
// Interrupts are masked for the whole sequence and not unmasked here: taking an
// exception between writing ELR_EL1 and the `eret` would let the hardware
// overwrite both ELR_EL1 and SPSR_EL1 with the interrupted kernel state, and
// the `eret` would then jump to a kernel address at EL0. `eret` restores
// PSTATE from SPSR_EL1, which is zero — EL0t with DAIF clear — so the process
// is preemptible from its first instruction anyway.
.global enter_user
enter_user:
    msr     daifset, #3
    msr     sp_el0, x1
    msr     elr_el1, x0
    msr     spsr_el1, xzr               // EL0t, all interrupts unmasked
    mov     x0, x2                      // argv[0], such as it is: the role

    // Scrub every other register. Whatever the kernel left in them — stack
    // addresses, pointers into the linear map — is not the process's business.
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    mov     x4, xzr
    mov     x5, xzr
    mov     x6, xzr
    mov     x7, xzr
    mov     x8, xzr
    mov     x9, xzr
    mov     x10, xzr
    mov     x11, xzr
    mov     x12, xzr
    mov     x13, xzr
    mov     x14, xzr
    mov     x15, xzr
    mov     x16, xzr
    mov     x17, xzr
    mov     x18, xzr
    mov     x19, xzr
    mov     x20, xzr
    mov     x21, xzr
    mov     x22, xzr
    mov     x23, xzr
    mov     x24, xzr
    mov     x25, xzr
    mov     x26, xzr
    mov     x27, xzr
    mov     x28, xzr
    mov     x29, xzr
    mov     x30, xzr
    eret
