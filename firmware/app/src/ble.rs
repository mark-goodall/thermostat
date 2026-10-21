//! BLE scanner sharing the radio with WiFi (esp-radio coexistence).
//!
//! Talks HCI to the controller directly through bt-hci: no host stack is
//! needed to listen to advertisements.

use bt_hci::ControllerToHostPacket;
use bt_hci::cmd::controller_baseband::{Reset, SetEventMask};
use bt_hci::cmd::le::{LeSetEventMask, LeSetScanEnable, LeSetScanParams};
use bt_hci::controller::Controller;
use bt_hci::controller::{ControllerCmdSync, ExternalController};
use bt_hci::event::Event;
use bt_hci::event::le::LeEvent;
use bt_hci::param::{
    AddrKind, Duration as HciDuration, EventMask, LeEventMask, LeScanKind, ScanningFilterPolicy,
};
use embassy_futures::join::join;
use embassy_time::{Duration, Instant, Timer};
use esp_radio::ble::controller::BleConnector;

use crate::device::Handle;

/// Scan continuously and let the coexistence arbiter share the radio with
/// WiFi. Measured on the bench with WiFi connected: a 30 ms / 100 ms window
/// caught ~0.3 advertisements/s, continuous scanning ~3.4/s, while HTTP stayed
/// responsive (requires WiFi modem sleep, see net.rs).
const SCAN_INTERVAL_MS: u32 = 100;
const SCAN_WINDOW_MS: u32 = 100;

type Hci = ExternalController<BleConnector<'static>, 4>;

async fn start_scan(hci: &Hci) -> Result<(), &'static str> {
    hci.exec(&Reset::new())
        .await
        .map_err(|_| "HCI reset failed")?;
    hci.exec(&SetEventMask::new(EventMask::new().enable_le_meta(true)))
        .await
        .map_err(|_| "set event mask failed")?;
    hci.exec(&LeSetEventMask::new(
        LeEventMask::new().enable_le_adv_report(true),
    ))
    .await
    .map_err(|_| "set LE event mask failed")?;
    // Active scanning: some sensors (ThermoPro) only send their name, which
    // identifies the payload format, in the scan response.
    hci.exec(&LeSetScanParams::new(
        LeScanKind::Active,
        HciDuration::from_millis(SCAN_INTERVAL_MS),
        HciDuration::from_millis(SCAN_WINDOW_MS),
        AddrKind::PUBLIC,
        ScanningFilterPolicy::BasicUnfiltered,
    ))
    .await
    .map_err(|_| "set scan parameters failed")?;
    // No duplicate filtering: we want RSSI and sensor updates over time.
    hci.exec(&LeSetScanEnable::new(true, false))
        .await
        .map_err(|_| "scan enable failed")?;
    Ok(())
}

#[embassy_executor::task]
pub async fn task(bt: esp_hal::peripherals::BT<'static>, dev: Handle) {
    let connector = match BleConnector::new(bt, Default::default()) {
        Ok(c) => c,
        Err(e) => {
            log::error!("ble: init failed: {e:?}");
            dev.with(|d| d.ble.error = Some("BLE controller init failed"));
            return;
        }
    };
    let hci: Hci = ExternalController::new(connector);

    let setup = async {
        match start_scan(&hci).await {
            Ok(()) => {
                log::info!(
                    "ble: scan started (heap used {} / free {})",
                    esp_alloc::HEAP.used(),
                    esp_alloc::HEAP.free()
                );
                dev.with(|d| d.ble.scanning = true);
            }
            Err(e) => {
                log::error!("ble: {e}");
                dev.with(|d| d.ble.error = Some(e));
            }
        }
        // Drop stale devices now and then; report memory every 5 minutes.
        for round in 1u32.. {
            Timer::after(Duration::from_secs(30)).await;
            let now = Instant::now().as_millis();
            let (devices, reports) = dev.with(|d| {
                d.ble.expire(now);
                (d.ble.len(), d.ble.reports)
            });
            if round % 10 == 0 {
                log::info!(
                    "ble: {devices} devices, {reports} reports; heap used {} / free {}",
                    esp_alloc::HEAP.used(),
                    esp_alloc::HEAP.free()
                );
            }
        }
    };

    let reader = async {
        let mut buf = [0u8; 259];
        loop {
            let packet = match hci.read(&mut buf).await {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("ble: read error {e:?}");
                    Timer::after(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let ControllerToHostPacket::Event(event) = packet else {
                continue;
            };
            let Ok(Event::Le(LeEvent::LeAdvertisingReport(adv))) = Event::try_from(event) else {
                continue;
            };
            let now = Instant::now().as_millis();
            dev.with(|d| {
                for report in adv.reports.iter().flatten() {
                    let mut addr = [0u8; 6];
                    addr.copy_from_slice(report.addr.raw());
                    d.ble.observe(
                        addr,
                        report.addr_kind.as_raw(),
                        report.rssi,
                        report.data,
                        now,
                    );
                }
            });
        }
    };

    join(setup, reader).await;
}
