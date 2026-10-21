//! What the hardware-independent code needs from the device.
//!
//! The firmware implements these traits on top of esp-hal/embassy-boot-esp;
//! tests implement them with fakes.

use heapless::String;

use crate::auth::Sessions;
use crate::ble::BleTable;
use crate::settings::Settings;
use crate::store::StoreError;

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
    /// Room temperature in tenths of °C from the selected sensors, if any is fresh.
    pub temperature_dc: Option<i16>,
    /// Number of selected sensors that contributed to `temperature_dc`.
    pub sensors_used: u8,
    pub heating: bool,
    /// Why the heating is in its current state.
    pub heating_reason: &'static str,
}

pub trait Platform: Ota {
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
