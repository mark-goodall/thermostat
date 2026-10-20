/* Second-stage bootloader layout for the ESP32.
 *
 * The mask ROM copies these sections into RAM and jumps to _start on its own
 * stack (0x3FFE1320..0x3FFE3F20). Both regions are outside everything the
 * application image may load into, so loading the app cannot overwrite the
 * running bootloader. IRAM 0x40078000 is the APP CPU's flash cache memory,
 * which stays disabled until the application turns it on.
 */
ENTRY(_start)

MEMORY
{
  iram (RX) : ORIGIN = 0x40078000, LENGTH = 0x8000
  dram (RW) : ORIGIN = 0x3FFF0000, LENGTH = 0x8000
}

SECTIONS
{
  .text : ALIGN(4)
  {
    *(.literal .text .literal.* .text.* .iram1 .iram1.*)
    . = ALIGN(4);
  } > iram

  /* IRAM only supports 32-bit loads, so constants live in DRAM. */
  .rodata : ALIGN(4)
  {
    *(.rodata .rodata.* .srodata .srodata.*)
    . = ALIGN(4);
  } > dram

  .data : ALIGN(4)
  {
    *(.data .data.* .sdata .sdata.* .dram1 .dram1.*)
    . = ALIGN(4);
  } > dram

  .bss (NOLOAD) : ALIGN(4)
  {
    _bss_start = .;
    *(.bss .bss.* .sbss .sbss.* COMMON)
    . = ALIGN(4);
    _bss_end = .;
  } > dram

  /DISCARD/ : { *(.eh_frame .eh_frame_hdr .xt.prop .xt.prop.* .xt.lit .xt.lit.* .comment) }
}
