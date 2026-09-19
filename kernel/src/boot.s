// Jungey OS — AArch64 entry point.
//
// Contract from the bootloader (or QEMU -kernel):
//   x0 = physical address of the flattened device tree
//   MMU off, caches off, running at EL2 (QEMU virt) or EL1.
//
// We park every secondary core, drop to EL1 if we booted at EL2, install a
// stack, clear .bss, and hand control to kernel_main(dtb).

.section ".text.boot", "ax"
.global _start

// ---- Linux AArch64 Image header (Documentation/arch/arm64/booting.rst) ----
// Lets U-Boot `booti`, EDK2, and QEMU's -kernel path load us the same way they
// load Linux: image placed at RAM base + text_offset, entered with the DTB
// pointer in x0, MMU and caches off, interrupts masked.
_start:
    b       _entry              // code0: branch over the header
    .long   0                   // code1
    .quad   0x80000             // text_offset: load 512K into RAM
    .quad   __image_size        // image_size: bytes of RAM the image needs
    .quad   0                   // flags: LE, 4K pages, 2MiB-aligned base
    .quad   0                   // res2
    .quad   0                   // res3
    .quad   0                   // res4
    .byte   0x41, 0x52, 0x4d, 0x64  // magic "ARM\x64"
    .long   0                   // res5

_entry:
    mov     x19, x0                     // stash DTB pointer across the drop to EL1

    mrs     x1, mpidr_el1
    and     x1, x1, #0xff
    cbz     x1, .Lprimary
.Lpark:
    wfe                                 // secondaries sleep until we bring them up
    b       .Lpark

.Lprimary:
    mrs     x1, CurrentEL
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    .Lat_el1

    // ---- running at EL2: configure EL1 and eret into it ----
    mrs     x2, cnthctl_el2
    orr     x2, x2, #3                  // EL1 may read the physical timer/counter
    msr     cnthctl_el2, x2
    msr     cntvoff_el2, xzr            // virtual offset = 0

    mov     x2, #(1 << 31)              // HCR_EL2.RW: EL1 is AArch64
    msr     hcr_el2, x2

    mov     x2, #0x0800                 // SCTLR_EL1 reset value: MMU/caches off,
    movk    x2, #0x30d0, lsl #16        // RES1 bits set
    msr     sctlr_el1, x2

    mov     x2, #0x3c5                  // SPSR: EL1h, DAIF masked
    msr     spsr_el2, x2
    adr     x2, .Lat_el1
    msr     elr_el2, x2
    eret

.Lat_el1:
    // ---- stack ----
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1

    // ---- zero .bss ----
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end
.Lbss_loop:
    cmp     x1, x2
    b.hs    .Lbss_done
    str     xzr, [x1], #8
    b       .Lbss_loop
.Lbss_done:

    mov     x0, x19                     // argv[0] = DTB
    bl      kernel_main

.Lhalt:
    wfe
    b       .Lhalt
