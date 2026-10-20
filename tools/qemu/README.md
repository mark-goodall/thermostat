# Patched Espressif QEMU for esp-hal firmware

Espressif's QEMU fork (`-machine esp32`) is developed against ESP-IDF. Four gaps
in its device models break esp-hal / esp-rtos based firmware; the patch in this
directory fixes them:

1. **Interrupt status registers** (`DPORT_{PRO,APP}_INTR_STATUS_n`, 0x3FF000EC..)
   were not implemented and always read 0. esp-hal's interrupt dispatcher reads
   them to find the interrupting peripheral, so no peripheral handler ever ran
   (interrupt storm on the first software interrupt).
2. **Shared CPU interrupt lines**: each source drove its CPU interrupt line
   directly, so clearing one source dropped the line even while another source
   routed to the same CPU interrupt was still asserted. Lines are now the OR of
   their sources (this lost esp-rtos scheduler wake-ups).
3. **TIMG level interrupts** were gated by `INT_ENA`. On the ESP32 they are not
   (esp-hal only sets `Tx_LEVEL_INT_EN`), so timer alarms never interrupted.
4. **Past-due TIMG alarms** were dropped. Emulation is slow relative to the
   virtual clock, so short alarms (esp-rtos arms many) were often already behind
   the counter when enabled.

Separately, firmware must be started with the FPU enabled (CPENABLE): ESP-IDF's
bootloader does this and esp-hal relies on it. Our bootloader
(`embassy-boot-esp`) does the same.

Build: `tools/qemu/build.sh` (see the dependency list in the script). The
xtask finds the result in `~/.local/share/esp-qemu/patched`, or set
`QEMU_XTENSA=/path/to/qemu-system-xtensa`.

These fixes are generic and would be worth upstreaming to espressif/qemu.
