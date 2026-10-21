# Thermostat firmware (ESP32)

Rust firmware for an ESP32 thermostat, built on esp-hal + embassy, with
power-fail-safe over-the-air updates through **embassy-boot-esp**, a Rust
second-stage bootloader.

Status: early but functional. Implemented so far are the bootloader, the update paths
(HTTP and serial), the settings store with its web pages, and emulator-based
testing. Heating is controlled from BLE temperature sensors (see "Heating control"),
driving GPIO2 as a stand-in for a relay output.

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

## Heating control

- **Sensors.** On the settings page, check up to 4 BLE temperature sensors.
  The page lists sensors currently heard that report a temperature, plus any
  already selected. The room temperature is the average of their readings
  newer than 10 minutes.
- **Hysteresis** (settings page, 0.1–5.0 °C). Heating turns on at or below
  `setpoint − hysteresis/2` and off at or above `setpoint + hysteresis/2`.
- **Short-cycle protection** (settings page, minutes; default 240 = 4 h, 0
  disables it, maximum 1440). Switches are at least this far apart; the
  status page shows the time left while a switch is held back. The first
  switch after boot is immediate, since the interval is not persisted across
  reboots.
- **Heater group.** Zones sharing one heater coordinate as described in
  "Multiple zones on one heater".
- **Fail-safe.** With no fresh reading from any selected sensor, or none
  selected, heating is off at once.
- **Output.** Heat demand drives GPIO2, active high. That is the on-board LED on
  many ESP32 dev boards; put a relay driver there (or change the pin in
  `main.rs`).
- **Status.** The status page and `/api/status` show the room temperature, the
  number of sensors used, the heating state and the reason (`below setpoint`,
  `within hysteresis band`, `waiting for minimum switch interval`, …).

Logic: `thermostat-core/src/control.rs` (host-tested). Loop: `firmware/app/src/heating.rs` (every 5 s).

Supported sensors:

| sensor | format | notes |
|---|---|---|
| ThermoPro TP357/TP357S/TP35x/TP39x | manufacturer data (company-ID field reused) | recognised by the frame format: `C2` marker, length, trailer bytes, plausible values. A name, if heard, also confirms it. Works with passive scanning. |
| Xiaomi LYWSD03MMC etc. with pvvx/ATC firmware | service data `0x181A` | |
| any BTHome v2 device (unencrypted) | service data `0xFCD2` | |

## Multiple zones on one heater

Thermostats with the same **heater id** (settings page, or `xtask provision
--heater-id`) share one heater. Typically their outputs or zone valves are
wired in parallel, so the heater runs while *any* zone requests heat.

- **Short-cycle protection:** applies to heater transitions only. A zone
  switching on while another zone already heats, or off while another keeps
  heating, switches at once. The first zone on and the last zone off wait
  for the shared interval since the latest heater transition in the group.
- **Discovery:** each zone advertises `heater=<id>` in its
  `_thermostat._tcp` mDNS TXT record, and browses for peers every minute.
- **Events:** `POST /api/zone/event` with
  `v=1&heater=…&zone=<hostname>&heat=0|1&last=<unix>&ts=<unix>&sig=<hex>`.
  - Sent at once when a zone's request changes, and every 30 s as a keepalive.
  - The reply carries the receiver's own state.
  - `last` is the latest heater transition the sender knows of. Zones keep
    the maximum, so everyone shares one timer; this needs NTP.
- **Liveness:** a zone not heard from for 90 s no longer counts as
  requesting heat.
- **Group key** (optional, same on all zones): messages are signed with
  HMAC-SHA256 and must be within 5 minutes of the receiver's clock.
  Unsigned groups accept any message for their heater id.
- **Inspection:** `GET /api/zone` shows this zone and its peers. The status
  page lists the zones and whether each is heating, idle or silent.

Known limit: if two zones decide to switch off within the same few seconds,
each may still believe the other is heating, so the heater can stop slightly
early.

Tested in QEMU with two emulated zones behind a learning switch (`xtask
test-qemu`), and on hardware against a second zone emulated by a Python
script on the LAN.

## Time (NTP)

The firmware keeps wall-clock time with SNTP:

- **Server and timezone:** set on the settings page.
  - NTP server: default `pool.ntp.org`, `host[:port]`, resolved via the DNS
    server DHCP provides.
  - Timezone: a POSIX TZ string, default `UTC0`. Examples:
    `GMT0BST,M3.5.0/1,M10.5.0` (UK), `CET-1CEST,M3.5.0,M10.5.0/3` (central
    Europe), `EST5EDT,M3.2.0,M11.1.0` (US Eastern).
- **Sync timing:** the first sync follows network up. After that, every hour
  and immediately when the server setting changes. Failures retry with
  backoff (10 s up to 5 min).
- **Status:** the status page and `/api/status` show local time
  (`local_time`, RFC 3339 with offset), Unix `time` and `time_synced_ago_s`.
- **For schedules:** `thermostat_core::clock` provides `WallClock`,
  `TimeZone::local(unix) -> DateTime` (weekday, `minute_of_day()`, DST flag),
  and civil-date helpers. These are the building blocks for time-of-day
  heating programmes.

Before the first sync there is no wall-clock time; anything time-based has to
fall back to plain setpoint control.

## Bluetooth (BLE scanner)

The WiFi build also runs a passive BLE scan on the same radio (esp-radio
`ble` + `coex`). It drives the controller with plain HCI through `bt-hci`, so
no host stack is needed. `/ble` in the web UI (auto-refreshing) and
`/api/ble` list nearby advertisers with:

- RSSI, name, and maker (Bluetooth SIG company ID)
- 16-bit service UUIDs, with names for common ones
- packet count and when each was last seen

Readings from supported thermometers (table above) are decoded into
temperature, humidity and battery. Stock Xiaomi
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
