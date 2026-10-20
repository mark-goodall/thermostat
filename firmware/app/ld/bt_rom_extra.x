/* Bluetooth controller ROM callbacks referenced by libbtdm_app (esp-wifi-sys-esp32 0.3)
 * but missing from esp-rom-sys 0.1.5's esp32.rom.ld. Addresses from ESP-IDF's
 * components/esp_rom/esp32/ld/esp32.rom.ld. */
PROVIDE ( ld_iscan_evt_start_cbk = 0x4003b58c );
PROVIDE ( ld_page_evt_start_cbk = 0x4003cf40 );
PROVIDE ( ld_pscan_evt_start_cbk = 0x4003e924 );
