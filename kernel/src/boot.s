// Jungey OS — AArch64 entry point.
//
// Contract from the bootloader (or QEMU -kernel):
//   x0 = physical address of the flattened device tree
//   MMU off, caches off, running at EL2 (QEMU virt) or EL1.
//
// The kernel is linked high (PHYS_OFFSET + 0x4008_0000) but entered low, so
// everything up to `_high_entry` must address symbols PC-relatively: `adrp`
// encodes a link-time *difference*, so at runtime it yields the physical
// address. The one place we deliberately want a link-time absolute is the
// branch into the higher half, which is why that uses a literal pool.

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
    mov     x19, x0                     // stash DTB pointer (physical) for later

    mrs     x1, mpidr_el1
    and     x1, x1, #0xff
    cbz     x1, .Lprimary
.Lpark:
    wfe                                 // secondaries sleep until stage 3
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
    // ---- physical stack, just until we branch high ----
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1

    // ---- zero .bss (this is also what zeroes the page tables) ----
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

    bl      .Lbuild_tables
    bl      .Lenable_mmu

    // ---- leave the identity map behind ----
    ldr     x0, =_high_entry            // link-time absolute: the high VA
    br      x0

// ---------------------------------------------------------------------------
// Page tables.
//
// 4 KiB granule, 48-bit VAs. L0[0] covers the low 512 GiB of *both* halves
// (VA[47:39] == 0 for identity addresses and for PHYS_OFFSET + x alike), so a
// single L0/L1 pair serves TTBR0 and TTBR1 until userspace needs its own.
//
// L1 maps 1 GiB blocks:
//   [0] 0x0000_0000  device  — UART, GIC, virtio live here
//   [1] 0x4000_0000  normal  — DRAM, and where the kernel itself lives
//   [2] 0x8000_0000  normal
//   [3] 0xC000_0000  normal
// ---------------------------------------------------------------------------

// Descriptor bits
.set DESC_VALID,   (1 << 0)
.set DESC_TABLE,   (1 << 1)         // with VALID => table descriptor
.set DESC_BLOCK,   0                // with VALID => block descriptor at L1/L2
.set BLK_AF,       (1 << 10)        // access flag; a fault without it, otherwise
.set BLK_SH_INNER, (3 << 8)         // inner shareable
.set BLK_ATTR_DEV, (0 << 2)         // AttrIndx = 0 -> MAIR attr0, Device-nGnRnE
.set BLK_ATTR_MEM, (1 << 2)         // AttrIndx = 1 -> MAIR attr1, Normal WB
.set BLK_UXN,      (1 << 54)        // never executable at EL0
.set BLK_PXN,      (1 << 53)        // never executable at EL1

.Lbuild_tables:
    adrp    x0, __l0_table              // physical, via PC-relative adrp
    adrp    x1, __l1_table

    orr     x2, x1, #(DESC_VALID | DESC_TABLE)
    str     x2, [x0]                    // L0[0] -> L1

    // L1[0]: MMIO at 0x0000_0000, device memory, no execute at either EL
    mov     x2, #(DESC_VALID | BLK_AF | BLK_ATTR_DEV)
    movk    x2, #0x0060, lsl #48        // UXN | PXN
    str     x2, [x1, #0]

    // L1[1..3]: DRAM, normal write-back, inner shareable, EL1-executable
    mov     x3, #(DESC_VALID | BLK_AF | BLK_SH_INNER | BLK_ATTR_MEM)
    movk    x3, #0x0040, lsl #48        // UXN only: EL1 runs code from here
    mov     x4, #0x40000000
    orr     x2, x3, x4
    str     x2, [x1, #8]
    mov     x4, #0x80000000
    orr     x2, x3, x4
    str     x2, [x1, #16]
    mov     x4, #0xC0000000
    orr     x2, x3, x4
    str     x2, [x1, #24]

    ret

.Lenable_mmu:
    // MAIR: attr0 = Device-nGnRnE (0x00), attr1 = Normal WB RA/WA (0xFF)
    mov     x0, #0xff00
    msr     mair_el1, x0

    // TCR: T0SZ = T1SZ = 16 (48-bit), 4 KiB granule both halves,
    //      inner/outer write-back write-allocate, inner shareable.
    mov     x0, #0x3510
    movk    x0, #0xb510, lsl #16
    mrs     x1, id_aa64mmfr0_el1        // IPS := min(PARange, 48-bit)
    and     x1, x1, #0xf
    mov     x2, #5
    cmp     x1, x2
    csel    x1, x1, x2, lo
    lsl     x1, x1, #32
    orr     x0, x0, x1
    msr     tcr_el1, x0

    adrp    x0, __l0_table
    msr     ttbr0_el1, x0               // identity, dropped in _high_entry
    msr     ttbr1_el1, x0               // higher half
    isb

    tlbi    vmalle1
    dsb     ish
    isb

    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)           // M:  MMU on
    orr     x0, x0, #(1 << 2)           // C:  data cache on
    orr     x0, x0, #(1 << 12)          // I:  instruction cache on
    msr     sctlr_el1, x0
    isb

    ret

// ---------------------------------------------------------------------------
// From here on the PC is a higher-half virtual address.
// ---------------------------------------------------------------------------
.section ".text", "ax"
_high_entry:
    ldr     x1, =__stack_top            // the same stack, now by its VA
    mov     sp, x1

    // Unmap the low half: TCR.EPD0 disables TTBR0 walks outright, so a null
    // dereference is a translation fault instead of a poke at MMIO.
    mrs     x0, tcr_el1
    orr     x0, x0, #(1 << 7)           // EPD0
    msr     tcr_el1, x0
    isb
    tlbi    vmalle1
    dsb     ish
    isb

    mov     x0, x19                     // argv[0] = DTB, still a physical address
    bl      kernel_main

.Lhalt:
    wfe
    b       .Lhalt

.section ".bss.pagetables", "aw", @nobits
.align 12
__l0_table:
    .space 4096
__l1_table:
    .space 4096
