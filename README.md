# Thermostat firmware (ESP32)

Rust firmware for an ESP32 thermostat, built on esp-hal + embassy, with
power-fail-safe over-the-air updates through **embassy-boot-esp**, a Rust
second-stage bootloader.

Status: groundwork. Implemented so far are the bootloader, the update paths
(HTTP and serial), the settings store with its web pages, and emulator-based
testing. The thermostat control loop is still a placeholder: setpoint and
hysteresis are stored, but no sensor or relay is driven yet.

## Layout

| path | what |
|------|------|
| `crates/embassy-boot-esp` | embassy-boot for Espressif chips: ROM-loaded 2nd-stage bootloader runtime, app-side `Updater`, image format, partition table |
| `crates/thermostat-core` | hardware-independent logic: settings + wear-levelled flash store, password hashing, web UI/API, serial update protocol, DHCP server (host-tested) |
| `crates/esp-openeth` | embassy-net driver for the Ethernet MAC emulated by QEMU (`open_eth`) |
| `firmware/bootloader` | the bootloader binary (flashed at 0x1000 in place of ESP-IDF's) |
| `firmware/app` | the application; feature `wifi` (hardware) or `qemu` (emulator) |
| `xtask` | build/flash/emulate/update tooling and the end-to-end tests |
| `tools/qemu` | patch + build script for Espressif QEMU so it can run esp-hal firmware |

## Flash layout (4 MiB)

| offset | size | contents |
|--------|------|----------|
| 0x1000 | 28 KiB | bootloader |
| 0x8000 | 4 KiB | partition table (ESP-IDF format, readable by espflash/esptool) |
| 0x9000 | 8 KiB | `bootstate`: embassy-boot swap progress |
| 0x10000 | 64 KiB | `settings`: parameter store (WiFi SSID/key, admin login hash, thermostat settings) |
| 0x20000 | 1.75 MiB | `active`: running application (executed in place) |
| 0x1E0000 | 1.75 MiB + 4 KiB | `dfu`: staged update, and the previous image after a swap |

## How updates work

1. The app streams a new image (HTTP `POST /api/update` or the serial
   protocol) into `dfu`, verifies its SHA-256 and target chip, then marks it
   for update.
2. On reset the bootloader swaps `dfu` and `active` page by page. Progress
   is journalled in `bootstate`, so a power cut during the swap resumes on
   the next boot.
3. The new image boots on **trial**. It confirms itself once the network has
   been up for 10 s. If it crashes or hangs first, the watchdog resets the
   chip and the bootloader swaps the old image back.

Images use a small format (`embassy_boot_esp::image`): header, segment
table, segments (flash segments aligned for the 64 KiB MMU) and a SHA-256
trailer. `cargo xtask image-info <file>` decodes one.

## Discovery

An mDNS responder (edge-mdns) answers `<hostname>.local` and advertises DNS-SD
services `_http._tcp` (the web UI) and `_thermostat._tcp` (TXT: `version`,
`mac`, `api`). It runs in both station and setup-AP mode:

```sh
avahi-browse -rt _thermostat._tcp     # or: dns-sd -B _thermostat._tcp
curl http://<hostname>.local/api/status
```

The hostname is set on the settings page or with `xtask provision --hostname`.
A change takes effect after a reboot.

## Bluetooth (BLE scanner)

The WiFi build also runs a passive BLE scan on the same radio (esp-radio
`ble` + `coex`). It drives the controller with plain HCI through `bt-hci`, so
no host stack is needed. `/ble` in the web UI (auto-refreshing) and
`/api/ble` list nearby advertisers with:

- RSSI, name, and maker (Bluetooth SIG company ID)
- 16-bit service UUIDs, with names for common ones
- packet count and when each was last seen

Readings from BLE thermometers using the pvvx/ATC1441 custom firmware or
BTHome v2 are decoded into temperature, humidity and battery. Stock Xiaomi
MiBeacon (`FE95`) payloads are encrypted and are not decoded.

Bench numbers with WiFi connected:

| scan window | advertisements/s | HTTP upload |
|---|---|---|
| 30 ms of 100 ms | ~0.3 | — |
| continuous (chosen) | ~3.4 | 36 KB/s (66 KB/s without BLE) |

Coexistence requires WiFi modem sleep, which the firmware enables. Ping RTT
rises to 20–120 ms; HTTP stays at about 60–120 ms.

Memory cost: the BT controller reserves 64 KB of DRAM, so the main stack drops
to about 30 KB. The heap had 62 KB used and 37 KB free after BLE start (logged
on the console every 5 minutes). The emulator build has no Bluetooth; there
`/ble` reports "no Bluetooth radio".

esp-rom-sys 0.1.5 lacks three BT ROM symbols that the controller blob needs.
They are provided in `firmware/app/ld/bt_rom_extra.x`, with addresses from
ESP-IDF.

## Settings and security

- WiFi: only the WPA2 PSK derived from passphrase + SSID is stored, never
  the passphrase itself.
- Admin login: PBKDF2-HMAC-SHA256 (10k rounds, random salt). Sessions are
  `HttpOnly; SameSite=Strict` cookies. Scripts can use HTTP Basic.
- First boot: every page redirects to `/setup` to create the admin account.
- No WiFi configured (or the network can't be joined for 3 minutes): the
  device opens an open setup AP `thermostat-XXXX` at http://192.168.4.1/
  for 10 minutes.
- Settings records rotate across the 16 sectors of the partition, so wear is
  spread. A torn write leaves the previous record valid.

Not done yet: TLS, and signed images. Anyone holding the admin password can
install any image built for the ESP32. embassy-boot's ed25519 verification is
the natural next step.

## Prerequisites

- Rust stable plus the Xtensa toolchain: `espup install` (provides `cargo +esp`).
  xtask reads `~/export-esp.sh` itself.
- `espflash` for flashing hardware.
- For emulation: the patched QEMU, built with `tools/qemu/build.sh`
  (see `tools/qemu/README.md` for why it's needed).

## Commands

```sh
cargo test                                   # host tests (core, bootloader logic, tooling)
cargo xtask build --release                  # hardware images -> target/images/wifi/
cargo xtask flash --release --port /dev/ttyUSB0 [--erase-settings]
cargo xtask qemu                             # boot in QEMU; web UI at http://localhost:8080
cargo xtask qemu --serial-port 5555          # console on TCP (waits for a client)
cargo xtask test-qemu                        # end-to-end tests in QEMU
cargo xtask http-ota target/images/wifi/thermostat.img --host 192.168.1.20 --password ...
cargo xtask serial-ota target/images/wifi/thermostat.img --port /dev/ttyUSB0
cargo xtask image-info target/images/wifi/thermostat.img
cargo xtask provision --port /dev/ttyUSB0 --admin-password ... [--ssid NET --wifi-password ...] [--hostname H]
```

`provision` builds a settings record on the host (same store code as the
firmware) and writes the settings partition over USB. It's useful for factory
setup or for getting a device onto WiFi without the setup AP.

`flash` writes the bootloader, the partition table, a blank boot state and the
app as separate regions, so settings survive unless `--erase-settings` is
given. `flash.bin` in the image directory is a complete 4 MiB image.

## Emulation and CI

`cargo xtask test-qemu` builds four firmware variants and drives one emulated
device through: first-boot setup → settings persisted → HTTP update (trial
boot, then confirmation) → corrupt image rejected → an image that never
confirms itself is rolled back after a watchdog reset → serial update over the
console. The QEMU console log goes to `target/e2e/console.log`.

The emulator exercises the real bootloader, MMU mapping, flash and swap
logic, the network stack, the web server and the serial protocol. WiFi is not
emulated: QEMU provides an Ethernet MAC instead, selected by the `qemu`
feature. The WiFi code is compile-checked and will need checking on hardware.

`.github/workflows/ci.yml` runs host tests, both firmware builds, and the QEMU
end-to-end suite (with the patched QEMU cached).

## Hardware results (ESP32 rev 1.0, 4 MB, CH340 dev board, 2026-10-19)

Verified on the bench: our bootloader boots the app, the setup AP comes up,
serial OTA works (684 KB in 70 s), the bootloader swaps the update in (~41 s),
the trial image confirms itself, provisioning over USB works, and settings
persist across resets. A real watchdog rollback also works: a non-confirming
image hangs, TG0 resets the chip 15 s later, and the revert plus SHA-256
check takes ~50 s before the old image re-confirms.

Over WiFi (2026-10-20, station mode on the user's network):
- mDNS: `bench-thermostat.local` resolves, and `avahi-browse` lists
  `_thermostat._tcp` (TXT: version, MAC, API path) and `_http._tcp`.
- Web UI: login, settings, the status and update pages, and logout all work.
  Pages load in about 65 ms; login takes about 0.8 s (PBKDF2).
- HTTP OTA: 726 KB uploads in 11 s, the swap takes 41 s, WiFi rejoins about
  0.1 s after boot, and the trial image confirms 10 s later. Settings survive.
- Ping works (smoltcp `auto-icmp-echo-reply`).

Opening the serial port resets the board: the CH340's DTR/RTS lines drive the
auto-reset circuit.

## Hardware notes

- Chip revision: `ESP_HAL_CONFIG_MIN_CHIP_REVISION = 0` (`.cargo/config.toml`), so
  older ESP32 boards and QEMU are accepted.
- The app reconfigures UART0 right after `esp_hal::init`. Raising the CPU
  clock changes the APB clock that the ROM's baud divider was set up for.
  (ESP-IDF's bootloader normally hides this.)
- Console/serial updates use UART0 (GPIO1/3) at 115200 baud: the USB-serial
  port on dev boards.
