//! What the hardware-independent code needs from the device.
//!
//! The firmware implements these traits on top of esp-hal/embassy-boot-esp;
//! tests implement them with fakes.

use heapless::String;

use crate::auth::Sessions;
use crate::ble::BleTable;
use crate::settings::Settings;
use crate::store::StoreError;
use crate::zones::Group;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaError {
    /// Another transport is mid-update.
    Busy,
    Sequence,
    TooLarge,
    /// The image failed validation (format, chip, or SHA-256).
    Invalid,
    Flash,
    /// The running image is still on trial; it must confirm itself first.
    NotConfirmed,
    /// The image was built for a different board (see [`image_board`]).
    WrongBoard,
}

/// The board an image was built for, from its version string: the text after
/// the last `@` (e.g. `0.6.0+1a2b3c4@dual`), or `single` when there is none.
pub fn image_board(version: &str) -> &str {
    match version.rsplit_once('@') {
        Some((_, board)) if !board.is_empty() => board,
        _ => "single",
    }
}

impl OtaError {
    pub fn as_str(&self) -> &'static str {
        match self {
            OtaError::Busy => "another update is in progress",
            OtaError::Sequence => "unexpected data offset",
            OtaError::TooLarge => "image too large",
            OtaError::Invalid => "image failed verification",
            OtaError::Flash => "flash error",
            OtaError::NotConfirmed => "running firmware not yet confirmed",
            OtaError::WrongBoard => "image is for a different board",
        }
    }
}

/// Firmware update sink shared by the HTTP and serial transports.
pub trait Ota {
    fn running_version(&self) -> &str;
    fn ota_begin(&mut self, len: u32) -> Result<(), OtaError>;
    fn ota_write(&mut self, offset: u32, data: &[u8]) -> Result<(), OtaError>;
    /// Verify and stage the image; returns its version string.
    fn ota_finish(&mut self) -> Result<String<32>, OtaError>;
    fn ota_abort(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootState {
    Booted,
    Trial,
    Reverted,
    Unknown,
}

impl BootState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BootState::Booted => "booted",
            BootState::Trial => "trial",
            BootState::Reverted => "reverted",
            BootState::Unknown => "unknown",
        }
    }
}

pub struct DeviceStatus {
    pub boot: BootState,
    pub uptime_s: u64,
    /// Human-readable network state, e.g. "wifi 192.168.1.20" or "ap setup".
    pub network: String<48>,
    /// Fresh readings of the selected sensors, each with its setpoint.
    pub readings: crate::control::Readings,
    /// Per heating zone (one entry on single-zone boards). The fields above
    /// (`heating`, `heating_reason`, `hold_remaining_s`) summarise them.
    pub zones: heapless::Vec<ZoneStatus, { crate::settings::MAX_ZONES }>,
    pub heating: bool,
    /// What this zone tells its heater group: it keeps the heater on. False while
    /// releasing (output still on, waiting for another zone to confirm).
    pub heat_request: bool,
    /// Why the heating is in its current state.
    pub heating_reason: &'static str,
    /// Seconds the heater must still run (minimum on time), while held on.
    pub hold_remaining_s: Option<u64>,
    /// Deepest main-stack use since boot and the stack size, in bytes (0 = unknown).
    pub stack_used: u32,
    pub stack_size: u32,
    /// Unix time (seconds), once NTP has synchronised.
    pub unix_time: Option<i64>,
    /// Seconds since the last successful NTP sync.
    pub time_synced_ago_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZoneStatus {
    /// Relay on.
    pub heating: bool,
    pub reason: &'static str,
    /// Seconds this zone's relay must still stay on (minimum on time).
    pub hold_remaining_s: Option<u64>,
}

pub trait Platform: Ota {
    /// Heating zones on this board (one relay each).
    fn zone_count(&self) -> usize {
        1
    }
    /// A snapshot of the current settings.
    fn settings(&self) -> Settings;
    fn save_settings(&mut self, settings: Settings) -> Result<(), StoreError>;
    fn random(&mut self, buf: &mut [u8]);
    fn now_ms(&self) -> u64;
    fn status(&mut self) -> DeviceStatus;
    /// Reboot once the current response has been sent.
    fn request_reboot(&mut self);
    /// Access the login sessions shared by all web connections.
    fn sessions<R>(&mut self, f: impl FnOnce(&mut Sessions) -> R) -> R;
    /// The BLE scan results, or `None` when the build has no Bluetooth radio.
    fn ble<R>(&mut self, f: impl FnOnce(Option<&BleTable>) -> R) -> R;
    /// The other zones of this thermostat's heater group.
    fn zones<R>(&mut self, f: impl FnOnce(&mut Group) -> R) -> R;
    /// Test builds (emulator) only: inject a sensor reading. Returns false if unsupported.
    fn inject_reading(&mut self, _addr: [u8; 6], _temperature_cc: i16) -> bool {
        false
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[derive(Default)]
    pub struct FakeOta {
        pub written: std::vec::Vec<u8>,
        pub active: bool,
        pub finished: bool,
        pub expected: u32,
    }

    impl Ota for FakeOta {
        fn running_version(&self) -> &str {
            "0.1.0-test"
        }
        fn ota_begin(&mut self, len: u32) -> Result<(), OtaError> {
            if len > 1 << 20 {
                return Err(OtaError::TooLarge);
            }
            self.written.clear();
            self.active = true;
            self.expected = len;
            Ok(())
        }
        fn ota_write(&mut self, offset: u32, data: &[u8]) -> Result<(), OtaError> {
            if !self.active || offset as usize != self.written.len() {
                return Err(OtaError::Sequence);
            }
            self.written.extend_from_slice(data);
            Ok(())
        }
        fn ota_finish(&mut self) -> Result<String<32>, OtaError> {
            self.active = false;
            if self.written.len() as u32 != self.expected || self.written.first() == Some(&0xEE) {
                return Err(OtaError::Invalid);
            }
            self.finished = true;
            Ok(String::try_from("new").unwrap())
        }
        fn ota_abort(&mut self) {
            self.active = false;
        }
    }
}
