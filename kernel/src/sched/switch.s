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
    msr     daifclr, #3                 // a new thread runs with IRQs enabled
    mov     x0, x20
    blr     x19
    bl      thread_exit
    b       .                           // thread_exit does not return
