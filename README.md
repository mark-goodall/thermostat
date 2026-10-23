# Thermostat firmware (ESP32)

Rust firmware for an ESP32 thermostat, built on esp-hal + embassy, with
power-fail-safe over-the-air updates through **embassy-boot-esp**, a Rust
second-stage bootloader.

Status: early but functional. Implemented so far are the bootloader, the update paths
(HTTP and serial), the settings store with its web pages, and emulator-based
testing. Heating is controlled from BLE temperature sensors (see "Heating control"),
driving the zone relay on GPIO16, with a status LED on GPIO23.

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
| 0x10000 | 64 KiB | `settings`: first 32 KiB settings records (WiFi SSID/key, admin login hash, thermostat settings), second 32 KiB sensor schedules. Each half is a wear-levelled record log; firmware from before the split kept settings anywhere in the 64 KiB, and the first boot of newer firmware moves the newest record into the first half. |
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

- **Sensors.** On the settings page, check up to 8 BLE temperature sensors and
  give each its own setpoint (5.0–35.0 °C) and, optionally, a name of up to 16
  characters (e.g. "Landing"). The name is shown on the thermostat's pages, in
  `/api/status` and in the log. The sensor's own advertised name (e.g.
  `TP357 (2142)`) is fixed in its firmware and stays as it is. The page lists sensors currently
  heard that report a temperature, plus any already selected. A newly checked
  sensor starts at the default setpoint. Only readings newer than 10 minutes
  count.
- **Hysteresis** (settings page, 0.1–5.0 °C), applied to each sensor against
  its own setpoint:
  - Heating turns **on** as soon as *any* sensor is at or below
    `setpoint − hysteresis/2`.
  - It turns **off** once *every* sensor is at or above `setpoint + hysteresis/2`
    (and the minimum on time has passed).
  - Otherwise it stays as it is.
  - The status page and `/api/status` (`sensors`) show each sensor's reading,
    setpoint and band (`below`, `in band`, `above`).
- **Short-cycle protection = minimum heater on time** (settings page,
  minutes; default 240 = 4 h, 0 disables it, maximum 1440).
  - A request for heat is never delayed.
  - Once the heater is on, it keeps running until it has been on this long,
    even if the room passes the off point. The status page shows the time
    to go.
  - The timer is not persisted across reboots.
- **Heater group.** Zones sharing one heater coordinate as described in
  "Multiple zones on one heater".
- **Fail-safe.** With no fresh reading from any selected sensor, or none
  selected, heating is off at once.
- **Output.** Heat demand drives the zone relay on GPIO16, active high (the
  two-zone board adds GPIO17, see below). The status LED on GPIO23 blinks at
  1 Hz while a relay is on and 0.2 Hz while none is. Pins are set in `main.rs`.
- **Status.** The status page and `/api/status` show the room temperature, the
  per-sensor readings, the heating state and the reason (`a sensor is below its
  setpoint`, `all sensors above setpoint`, `within hysteresis band`, `keeping the heater on for its minimum on time`,
  `checking that another zone keeps the heater on`, …).

Logic: `thermostat-core/src/control.rs` (host-tested). Loop: `firmware/app/src/heating.rs` (every 5 s).

Supported sensors:

| sensor | format | notes |
|---|---|---|
| ThermoPro TP357/TP357S/TP35x/TP39x | manufacturer data (company-ID field reused) | recognised by the frame format: `C2` marker, length, trailer bytes, plausible values. A name, if heard, also confirms it. Works with passive scanning. |
| Xiaomi LYWSD03MMC etc. with pvvx/ATC firmware | service data `0x181A` | |
| any BTHome v2 device (unencrypted) | service data `0xFCD2` | |

## Schedules and boost

- **Schedules.** Each selected sensor can follow a schedule instead of its
  fixed setpoint: one setpoint per hour, one row for Monday-Friday and one for
  Saturday-Sunday, 5.0-32.5 °C in 0.5 °C steps. Edit it from the sensor's
  "Schedule" link on the settings page (`/schedule?sensor=AABBCCDDEEFF`), or
  through `GET/POST /api/schedule?sensor=…` (form fields `on`, `d0`-`d23`,
  `e0`-`e23`; leaving out `on` turns the schedule off). Hours are local time
  (timezone setting); until NTP time is known the fixed setpoint applies. The
  status page and `/api/status` (`"scheduled": true`) show the setpoint in force.
- **Storage.** A schedule is one byte per hour: 48 bytes per sensor, at most
  432 bytes for eight sensors. Schedules are stored in their own record log (the
  second half of the settings partition, see "Flash layout") and the device
  keeps the only copy in RAM; the web pages handle one sensor at a time.
- **Boost.** The Boost button next to a zone on the status page (or `POST
  /api/boost` with `zone=N`; `on=0` cancels) heats that zone for an hour
  whatever its sensors say, even without a reading. Afterwards normal control
  resumes, minimum on time included, so the heater may run on past the hour.
  A boost lasts until it ends, is cancelled, or the thermostat restarts.

## Two-zone board

A second firmware drives a board with **two relays, one per heating zone, both
on one heater**: GPIO16 (zone 1) and GPIO17 (zone 2), LED on GPIO23.

```sh
cargo xtask build --release --board dual     # -> target/images/wifi-dual/
cargo xtask flash --release --board dual --port /dev/ttyUSB0
```

- **Zones.** Name them on the settings page (up to 16 characters; empty =
  "Zone 1"/"Zone 2"), and pick a zone for each selected sensor. Each zone
  decides from its own sensors with the usual rule (on when any is low, off
  when all are high).
- **Minimum on time** covers the heater as a whole, as for a heater group:
  - a zone calling for heat always gets it at once;
  - a zone leaving while the other zone heats switches off at once;
  - the last zone switching off keeps its relay on until the heater has run
    for the minimum time since it started, whichever zone started it.
  Both zones are decided in one step, so the two-step release is not needed.
- **No heater groups.** The two-zone board runs its own heater; the heater id
  settings are hidden and ignored.
- **Status.** The status page has a row per zone (relay, reason, time to go,
  its sensors); `/api/status` adds `"zones": [{name, heating, reason,
  hold_remaining_s}]`, and each sensor carries its `zone`. The top-level
  `heating` is true while any zone heats.
- **Images are board-specific.** A dual image's version ends in `@dual`
  (e.g. `0.1.0+1a2b3c4@dual`). Each firmware refuses an update built for the
  other board (HTTP: `400 image is for a different board`; serial: NAK code 7),
  so a single-zone image cannot leave the second relay unmanaged.

Logic: `control::Controller` in `thermostat-core` (host-tested); board pins and
zone count: `firmware/app/src/board.rs`.

## Multiple zones on one heater

Thermostats with the same **heater id** (settings page, or `xtask provision
--heater-id`) share one heater. Typically their outputs or zone valves are
wired in parallel, so the heater runs while *any* zone requests heat.

- **Minimum on time:** applies to the heater as a whole.
  - Any zone requesting heat gets it at once.
  - The last zone switching off keeps the heater on until it has run for the
    minimum time since it *started*, whichever zone started it.
  - A zone switching off early while another seems to heat **releases**: it
    sends `heat=0` but keeps its output on. It switches off only if a peer
    *replies* `heat=1`. That peer has seen the `heat=0`, so it knows it may be
    the last zone and will hold the heater itself. With no such reply (both
    zones released at once, a lost message, a silent peer), the zone holds the
    heater for the rest of the minimum on time. Errors therefore make the
    heater run longer, never stop early. Leaving costs one HTTP round trip.
  - Once the minimum on time has passed, any zone switches off at once.
- **Discovery:** each zone advertises `heater=<id>` in its
  `_thermostat._tcp` mDNS TXT record, and browses for peers every minute.
- **Events:** `POST /api/zone/event` (`heat=1`: the sender keeps the heater
  on, by wanting heat or holding it) with
  `v=1&heater=…&zone=<hostname>&heat=0|1&on=<unix>&ts=<unix>&sig=<hex>`.
  - Sent at once when a zone's request changes, and every 30 s as a keepalive.
  - The reply carries the receiver's own state.
  - `on` is when the heater last started, as the sender knows it. Zones keep
    the latest, so all of them time the same run; this needs NTP.
- **Liveness:** a zone not heard from for 90 s no longer counts as
  requesting heat.
- **Group key** (optional, same on all zones): messages are signed with
  HMAC-SHA256 and must be within 5 minutes of the receiver's clock.
  Unsigned groups accept any message for their heater id.
- **Inspection:** `GET /api/zone` shows this zone and its peers. The status
  page lists the zones and whether each is heating, idle or silent.

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
`/api/ble` list nearby temperature sensors with:

- RSSI, name, and maker (Bluetooth SIG company ID)
- 16-bit service UUIDs, with names for common ones
- packet count and when each was last seen

Only temperature sensors are kept: an advertisement that does not decode as a
reading (and is not from a sensor already known) is dropped and counted in
`ignored`. That keeps phones, laptops and beacons, many on rotating private
addresses, out of memory. The table holds 8 sensors; when full, the least
recently heard sensor that is not selected makes way (selected sensors are
never evicted), and one not heard for 15 minutes is dropped.

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
cargo xtask build --release --board dual     # two-zone board -> target/images/wifi-dual/
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
