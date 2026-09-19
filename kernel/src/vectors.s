// AArch64 exception vector table for EL1.
//
// The table is 2 KiB aligned and holds 16 entries of 0x80 bytes each:
//   offset  0x000  Current EL, SP_EL0   (sync / irq / fiq / serror)
//   offset  0x200  Current EL, SP_ELx   <- where the kernel itself faults
//   offset  0x400  Lower EL, AArch64    <- where user tasks trap in
//   offset  0x600  Lower EL, AArch32
//
// Each entry only has room for a register save and a branch, so the real work
// happens in __exception_common.

// Exception frame: x0-x30 at 0..247, then ELR_EL1, SPSR_EL1 and SP_EL0.
//
// ELR/SPSR must be in the frame, not left in the system registers: the
// scheduler switches threads from inside the IRQ handler, and the thread we
// switch to would otherwise clobber the return state of the one we left.
//
// SP_EL0 for the same reason, and it is easier to miss: there is exactly one
// SP_EL0 for the whole core. Preempt a user thread inside a syscall, return to
// a different one, and it resumes on the *other* process's stack pointer —
// which reads a stale return address and branches into nowhere.
.set FRAME_SIZE,   288
.set FRAME_ELR,    256
.set FRAME_SPSR,   264
.set FRAME_SP_EL0, 272

.macro SAVE_REGS
    sub     sp, sp, #FRAME_SIZE
    stp     x0,  x1,  [sp, #16 * 0]
    stp     x2,  x3,  [sp, #16 * 1]
    stp     x4,  x5,  [sp, #16 * 2]
    stp     x6,  x7,  [sp, #16 * 3]
    stp     x8,  x9,  [sp, #16 * 4]
    stp     x10, x11, [sp, #16 * 5]
    stp     x12, x13, [sp, #16 * 6]
    stp     x14, x15, [sp, #16 * 7]
    stp     x16, x17, [sp, #16 * 8]
    stp     x18, x19, [sp, #16 * 9]
    stp     x20, x21, [sp, #16 * 10]
    stp     x22, x23, [sp, #16 * 11]
    stp     x24, x25, [sp, #16 * 12]
    stp     x26, x27, [sp, #16 * 13]
    stp     x28, x29, [sp, #16 * 14]
    str     x30,      [sp, #16 * 15]
    mrs     x9,  elr_el1
    mrs     x10, spsr_el1
    stp     x9,  x10, [sp, #FRAME_ELR]
    mrs     x9,  sp_el0
    str     x9,       [sp, #FRAME_SP_EL0]
.endm

.macro RESTORE_REGS
    ldp     x9,  x10, [sp, #FRAME_ELR]
    msr     elr_el1,  x9
    msr     spsr_el1, x10
    ldr     x9,       [sp, #FRAME_SP_EL0]
    msr     sp_el0,   x9
    ldp     x0,  x1,  [sp, #16 * 0]
    ldp     x2,  x3,  [sp, #16 * 1]
    ldp     x4,  x5,  [sp, #16 * 2]
    ldp     x6,  x7,  [sp, #16 * 3]
    ldp     x8,  x9,  [sp, #16 * 4]
    ldp     x10, x11, [sp, #16 * 5]
    ldp     x12, x13, [sp, #16 * 6]
    ldp     x14, x15, [sp, #16 * 7]
    ldp     x16, x17, [sp, #16 * 8]
    ldp     x18, x19, [sp, #16 * 9]
    ldp     x20, x21, [sp, #16 * 10]
    ldp     x22, x23, [sp, #16 * 11]
    ldp     x24, x25, [sp, #16 * 12]
    ldp     x26, x27, [sp, #16 * 13]
    ldp     x28, x29, [sp, #16 * 14]
    ldr     x30,      [sp, #16 * 15]
    add     sp, sp, #FRAME_SIZE
.endm

.macro VENTRY idx
.align 7
    SAVE_REGS
    mov     x0, #\idx
    b       __exception_common
.endm

.section ".text", "ax"
.align 11
.global __vectors
__vectors:
    VENTRY 0    // Current EL, SP_EL0: synchronous
    VENTRY 1    // Current EL, SP_EL0: IRQ
    VENTRY 2    // Current EL, SP_EL0: FIQ
    VENTRY 3    // Current EL, SP_EL0: SError

    VENTRY 4    // Current EL, SP_ELx: synchronous
    VENTRY 5    // Current EL, SP_ELx: IRQ
    VENTRY 6    // Current EL, SP_ELx: FIQ
    VENTRY 7    // Current EL, SP_ELx: SError

    VENTRY 8    // Lower EL, AArch64: synchronous
    VENTRY 9    // Lower EL, AArch64: IRQ
    VENTRY 10   // Lower EL, AArch64: FIQ
    VENTRY 11   // Lower EL, AArch64: SError

    VENTRY 12   // Lower EL, AArch32: synchronous
    VENTRY 13   // Lower EL, AArch32: IRQ
    VENTRY 14   // Lower EL, AArch32: FIQ
    VENTRY 15   // Lower EL, AArch32: SError

__exception_common:
    mrs     x1, esr_el1
    mrs     x2, elr_el1
    mrs     x3, far_el1
    mov     x4, sp
    bl      rust_exception
    RESTORE_REGS
    eret
