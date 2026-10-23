//! Heating control loop: readings from the selected BLE sensors, the per-zone
//! decision (`control::Controller`), the zone relays and the status LED.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use thermostat_core::clock::TimeZone;
use thermostat_core::control::{GroupState, apply_schedules, cc_to_dc, readings};
use thermostat_core::http::Tenths;

use crate::board;
use crate::device::Handle;

const PERIOD: Duration = Duration::from_secs(5);

/// Whether any relay is on, for the status LED.
static HEAT_ON: AtomicBool = AtomicBool::new(false);

/// Signalled when the group answered a release, to act on it at once.
pub static RECHECK: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// One zone's change, for the log.
struct Change {
    zone: usize,
    heat: bool,
    reason: &'static str,
    /// e.g. "; Landing 19.0/21.0 °C, Bedroom 18.6/18.0 °C"
    temps: heapless::String<160>,
    /// Zone name, on boards with several zones.
    name: heapless::String<24>,
}

#[embassy_executor::task]
pub async fn task(mut relays: [Output<'static>; board::ZONES], dev: Handle) {
    let mut last: [Option<bool>; board::ZONES] = [None; board::ZONES];
    let mut last_request = None;
    loop {
        let now = Instant::now().as_millis();
        let (heat, request, changes, grouped) = dev.with(|d| {
            d.ble
                .set_keep(d.settings.temp_sensors.iter().map(|t| t.addr));
            let mut room = readings(&d.ble, &d.settings.temp_sensors, now);
            // Scheduled setpoints, once the local time is known.
            let local = d.clock.unix_s(Instant::now().as_micros()).map(|t| {
                TimeZone::parse(&d.settings.timezone)
                    .unwrap_or(TimeZone::UTC)
                    .local(t)
            });
            apply_schedules(&mut room, &d.schedule.schedules, local.as_ref());
            d.readings = room.clone();
            let min_on_ms = d.settings.min_on_min as u64 * 60_000;
            let now_unix = d.clock.unix_s(Instant::now().as_micros());
            // Heater groups (other boards) only exist on single-zone boards; the
            // dual board's settings never carry a heater id.
            let grouped = !d.settings.heater_id.is_empty();
            let group = if grouped {
                GroupState {
                    others_heating: d.group.others_heating(now),
                    // The group shares the heater start time as Unix time; map to our clock.
                    heater_on_since_ms: match (d.group.heater_on_since, now_unix) {
                        (Some(last), Some(unix)) => {
                            Some(now.saturating_sub((unix - last).max(0) as u64 * 1000))
                        }
                        _ => None,
                    },
                }
            } else {
                GroupState::STANDALONE
            };
            let out = d
                .heating
                .update(&room, d.settings.hysteresis_dc, min_on_ms, group, now);
            if d.heating.take_heater_started() {
                d.group.note_heater_on(now_unix);
            }
            let request = (
                d.heating.requesting(),
                d.heating.zones()[0].release_pending(),
            );
            let mut heat = [false; board::ZONES];
            heat.copy_from_slice(&out[..board::ZONES]);
            let mut changes: heapless::Vec<Change, { board::ZONES }> = heapless::Vec::new();
            for (z, &h) in heat.iter().enumerate() {
                if last[z] == Some(h) {
                    continue;
                }
                let mut temps = heapless::String::new();
                for r in room.iter().filter(|r| r.zone as usize == z) {
                    let sep = if temps.is_empty() { "; " } else { ", " };
                    let name = d
                        .settings
                        .temp_sensor(&r.addr)
                        .map_or("", |t| t.name.as_str());
                    let _ = core::fmt::write(
                        &mut temps,
                        format_args!(
                            "{sep}{name}{}{}/{} °C",
                            if name.is_empty() { "" } else { " " },
                            Tenths(cc_to_dc(r.temperature_cc) as i32),
                            Tenths(r.setpoint_dc as i32)
                        ),
                    );
                }
                let mut name = heapless::String::new();
                if board::ZONES > 1 {
                    let _ = match d.settings.zone_names[z].as_str() {
                        "" => core::fmt::write(&mut name, format_args!(" zone {}", z + 1)),
                        n => core::fmt::write(&mut name, format_args!(" {n}")),
                    };
                }
                let _ = changes.push(Change {
                    zone: z,
                    heat: h,
                    reason: d.heating.zones()[z].reason.as_str(),
                    temps,
                    name,
                });
            }
            (heat, request, changes, grouped)
        });
        for (relay, &h) in relays.iter_mut().zip(&heat) {
            if h {
                relay.set_high();
            } else {
                relay.set_low();
            }
        }
        HEAT_ON.store(heat.iter().any(|&h| h), Ordering::Relaxed);
        // Tell the group at once when our request changes or a release starts.
        if grouped && last_request != Some(request) {
            crate::zones::KICK.signal(());
            last_request = Some(request);
        }
        for c in changes {
            log::info!(
                "heating{} {} ({}{})",
                c.name,
                if c.heat { "ON" } else { "off" },
                c.reason,
                c.temps
            );
            last[c.zone] = Some(c.heat);
        }
        select(Timer::after(PERIOD), RECHECK.wait()).await;
    }
}

/// Status LED: blinks at 1 Hz while heating, 0.2 Hz while not (50 % duty).
#[embassy_executor::task]
pub async fn led_task(mut led: Output<'static>) {
    const TICK: Duration = Duration::from_millis(250);
    let mut heat = false;
    let mut ticks = 0u32;
    loop {
        let now = HEAT_ON.load(Ordering::Relaxed);
        if now != heat {
            // Start the new rate from the beginning of an "on" phase.
            heat = now;
            ticks = 0;
        }
        // Half periods in ticks: 500 ms (1 Hz) or 2.5 s (0.2 Hz).
        let half = if heat { 2 } else { 10 };
        if ticks % half == 0 {
            if (ticks / half) % 2 == 0 {
                led.set_high();
            } else {
                led.set_low();
            }
        }
        ticks = ticks.wrapping_add(1);
        Timer::after(TICK).await;
    }
}
