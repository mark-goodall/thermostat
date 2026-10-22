//! Heating control loop: room temperature from the selected BLE sensors,
//! hysteresis decision, the heat-demand output (zone relay) and the status LED.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use thermostat_core::control::{GroupState, room_temperature};

use crate::device::Handle;

const PERIOD: Duration = Duration::from_secs(5);

/// Whether the heat output is on, for the status LED.
static HEAT_ON: AtomicBool = AtomicBool::new(false);

/// Signalled when the group answered a release, to act on it at once.
pub static RECHECK: Signal<CriticalSectionRawMutex, ()> = Signal::new();

#[embassy_executor::task]
pub async fn task(mut output: Output<'static>, dev: Handle) {
    let mut last = None;
    let mut last_request = None;
    loop {
        let now = Instant::now().as_millis();
        let (heat, request, room, reason, grouped) = dev.with(|d| {
            let room = room_temperature(&d.ble, &d.settings.temp_sensors, now);
            d.room = room;
            let min_on_ms = d.settings.min_on_min as u64 * 60_000;
            let now_unix = d.clock.unix_s(Instant::now().as_micros());
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
            let heat = d.thermostat.update(
                room,
                d.settings.setpoint_dc,
                d.settings.hysteresis_dc,
                min_on_ms,
                group,
                now,
            );
            if d.thermostat.take_heater_started() {
                d.group.note_heater_on(now_unix);
            }
            let request = (d.thermostat.requesting(), d.thermostat.release_pending());
            (heat, request, room, d.thermostat.reason.as_str(), grouped)
        });
        if heat {
            output.set_high();
        } else {
            output.set_low();
        }
        HEAT_ON.store(heat, Ordering::Relaxed);
        // Tell the group at once when our request changes or a release starts.
        if grouped && last_request != Some(request) {
            crate::zones::KICK.signal(());
            last_request = Some(request);
        }
        if last != Some(heat) {
            match room {
                Some(r) => log::info!(
                    "heating {} ({reason}; room {}.{} °C from {} sensor(s))",
                    if heat { "ON" } else { "off" },
                    r.tenths() / 10,
                    (r.tenths() % 10).abs(),
                    r.sensors
                ),
                None => log::info!("heating {} ({reason})", if heat { "ON" } else { "off" }),
            }
            last = Some(heat);
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
