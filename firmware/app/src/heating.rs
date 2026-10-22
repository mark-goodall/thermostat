//! Heating control loop: room temperature from the selected BLE sensors,
//! hysteresis decision, and the heat-demand output.

use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use thermostat_core::control::{GroupState, room_temperature};

use crate::device::Handle;

const PERIOD: Duration = Duration::from_secs(5);

#[embassy_executor::task]
pub async fn task(mut output: Output<'static>, dev: Handle) {
    let mut last = None;
    loop {
        let now = Instant::now().as_millis();
        let (heat, room, reason, grouped) = dev.with(|d| {
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
            (heat, room, d.thermostat.reason.as_str(), grouped)
        });
        if heat {
            output.set_high();
        } else {
            output.set_low();
        }
        if last != Some(heat) {
            if grouped {
                crate::zones::KICK.signal(());
            }
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
        Timer::after(PERIOD).await;
    }
}
