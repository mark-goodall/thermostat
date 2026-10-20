//! Serial firmware update on the console UART (see `thermostat_core::proto`).
//!
//! Logs keep flowing on the same UART; replies go through esp-println's lock
//! so they are never interleaved with a log line.

use esp_hal::Async;
use esp_hal::uart::UartRx;
use thermostat_core::proto::{FrameReader, MAX_FRAME, SerialOta};

use crate::device::Handle;

#[embassy_executor::task]
pub async fn task(mut rx: UartRx<'static, Async>, dev: Handle) {
    let mut reader = FrameReader::new();
    let mut ota = SerialOta::new();
    let mut dev = dev;
    let mut reply = [0u8; MAX_FRAME];
    let mut buf = [0u8; 256];
    loop {
        let n = match rx.read_async(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("uart: {e:?}");
                continue;
            }
        };
        for &b in &buf[..n] {
            if let Some(frame) = reader.push(b)
                && let Some(len) = ota.handle(frame, &mut dev, &mut reply)
            {
                esp_println::Printer::write_bytes(&reply[..len]);
            }
        }
        if ota.reboot_requested {
            ota.reboot_requested = false;
            dev.with(|d| d.reboot_requested = true);
        }
    }
}
