OUTPUT_ARCH(riscv)
ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
    stack PT_GNU_STACK FLAGS(6);
}

SECTIONS
{
    . = 0x10000;
    .text : { *(.text .text.*) } :text
    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        *(.eh_frame_hdr)
        *(.eh_frame)
    } :text

    . = ALIGN(0x1000);
    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    } :data
    .bss : {
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        *(COMMON)
    } :data

    /DISCARD/ : { *(.comment) *(.note*) }
}
