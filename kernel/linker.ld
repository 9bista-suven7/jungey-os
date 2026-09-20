/* Jungey OS kernel image layout.
 *
 * The kernel is LINKED at a higher-half virtual address but LOADED at a
 * physical one: QEMU `virt` puts DRAM at 0x4000_0000 and the ARM64 boot
 * protocol places us at RAM base + text_offset (0x8_0000), so we run at
 * physical 0x4008_0000 until boot.s turns the MMU on and branches high.
 *
 *   VA = PA + 0xFFFF_0000_0000_0000   (mm::PHYS_OFFSET)
 *
 * Everything before the MMU is enabled must reach symbols PC-relatively
 * (adrp/add), which yields the physical address at runtime. See boot.s. */
ENTRY(_start)

PHYS_BASE  = 0x40080000;
PHYS_OFFSET = 0xFFFF000000000000;

SECTIONS
{
    . = PHYS_OFFSET + PHYS_BASE;
    __kernel_start = .;

    __text_start = .;
    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }
    /* Code and constants: the part of the image that should never change
     * between one boot and the next, and therefore the part worth measuring. */
    __rodata_end = .;

    . = ALIGN(4096);
    .data : { *(.data .data.*) }

    . = ALIGN(4096);
    .bss (NOLOAD) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Boot stack for the primary core. Grows down from __stack_top. */
    . = ALIGN(4096);
    . += 0x40000;
    __stack_top = .;

    . = ALIGN(4096);
    __kernel_end = .;

    /* Memory footprint advertised in the Image header. */
    __image_size = __kernel_end - __kernel_start;
    __kernel_start_phys = __kernel_start - PHYS_OFFSET;
    __kernel_end_phys   = __kernel_end - PHYS_OFFSET;

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
