/* Jungey OS kernel image layout.
 * QEMU `virt` puts DRAM at 0x4000_0000; 0x4008_0000 is the conventional
 * kernel load address on AArch64 (leaves the first 512K for firmware/DTB). */
ENTRY(_start)

SECTIONS
{
    . = 0x40080000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }

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

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
