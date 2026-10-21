//! Heating control loop: room temperature from the selected BLE sensors,
//! hysteresis decision, and the heat-demand output.

use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use thermostat_core::control::room_temperature;

use crate::device::Handle;

const PERIOD: Duration = Duration::from_secs(5);

#[embassy_executor::task]
pub async fn task(mut output: Output<'static>, dev: Handle) {
    let mut last = None;
    loop {
        let now = Instant::now().as_millis();
        let (heat, room, reason) = dev.with(|d| {
            let room = room_temperature(&d.ble, &d.settings.temp_sensors, now);
            d.room = room;
            let heat =
                d.thermostat
                    .update(room, d.settings.setpoint_dc, d.settings.hysteresis_dc, now);
            (heat, room, d.thermostat.reason.as_str())
        });
        if heat {
            output.set_high();
        } else {
            output.set_low();
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
        Timer::after(PERIOD).await;
    }
}
