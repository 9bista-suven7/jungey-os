/* Userspace image layout.
 *
 * Linked at 4 MiB: low enough to be obviously in the user half, high enough
 * that a null dereference has a whole unmapped page below it to land in.
 * Sections are page-aligned and grouped by permission so the kernel's ELF
 * loader can map text read-execute and data read-write with nothing in
 * between sharing a page. */
ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(4096);
    .data : { *(.data .data.*) }
    .bss  : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
