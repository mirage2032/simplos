/* Multiboot wants its header in the first 8 KiB of the image, so .boot leads.
   1 MiB is the conventional load address and is what v86's loader expects. */
ENTRY(_start)

SECTIONS {
    . = 1M;

    .boot : { KEEP(*(.multiboot)) }
    .text : { *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    .bss : {
        *(COMMON)
        *(.bss .bss.*)
    }
}
