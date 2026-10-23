//! Device state shared by all tasks, and its [`Platform`] implementation.

use core::cell::RefCell;

use embassy_boot_esp::{BootStatus, UpdateError, Updater};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_time::{Duration, Instant};
use esp_hal::rng::Rng;
use esp_storage::FlashStorage;
use heapless::String;
use thermostat_core::auth::Sessions;
use thermostat_core::ble::BleTable;
use thermostat_core::clock::WallClock;
use thermostat_core::control::{Controller, Readings};
use thermostat_core::platform::{
    BootState, DeviceStatus, Ota, OtaError, Platform, ZoneStatus, image_board,
};
use thermostat_core::settings::MAX_ZONES;
use thermostat_core::settings::Settings;
use thermostat_core::store::{SettingsStore, StoreError};
use thermostat_core::zones::Group;

pub const VERSION: &str = match option_env!("THERMOSTAT_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// The SPI flash, shared by the updater and the settings store. Each
/// operation takes the lock briefly; long jobs (image verification) are made
/// of many short reads, so interrupts are never held off for long.
pub type Flash = Mutex<CriticalSectionRawMutex, RefCell<FlashStorage<'static>>>;
pub type FlashPartition =
    BlockingPartition<'static, CriticalSectionRawMutex, FlashStorage<'static>>;
pub type FirmwareUpdater = Updater<'static, CriticalSectionRawMutex, FlashStorage<'static>>;

/// An abandoned transfer (client vanished) stops blocking new ones after this.
const OTA_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Device {
    pub settings: Settings,
    pub store: SettingsStore,
    pub settings_flash: FlashPartition,
    pub updater: FirmwareUpdater,
    pub rng: Rng,
    pub sessions: Sessions,
    /// Nearby BLE advertisers (filled by the scanner in WiFi builds).
    pub ble: BleTable,
    pub heating: Controller,
    pub readings: Readings,
    /// Wall-clock time, set by SNTP.
    pub clock: WallClock,
    /// Other zones sharing this thermostat's heater.
    pub group: Group,
    pub boot: BootState,
    pub network: String<48>,
    pub reboot_requested: bool,
    ota_activity: Option<Instant>,
}

impl Device {
    pub fn new(
        settings: Settings,
        store: SettingsStore,
        settings_flash: FlashPartition,
        mut updater: FirmwareUpdater,
    ) -> Self {
        let boot = match updater.status() {
            Ok(BootStatus::Booted) => BootState::Booted,
            Ok(BootStatus::Trial) => BootState::Trial,
            Ok(BootStatus::Reverted) => BootState::Reverted,
            _ => BootState::Unknown,
        };
        Self {
            settings,
            store,
            settings_flash,
            updater,
            rng: Rng::new(),
            sessions: Sessions::new(),
            ble: BleTable::new(),
            heating: Controller::new(crate::board::ZONES),
            readings: Readings::new(),
            clock: WallClock::new(),
            group: Group::new(),
            boot,
            network: String::try_from("starting").unwrap(),
            reboot_requested: false,
            ota_activity: None,
        }
    }

    /// Confirm a trial image so the bootloader keeps it.
    pub fn confirm_boot(&mut self) -> Result<(), UpdateError> {
        if matches!(self.boot, BootState::Trial | BootState::Reverted) {
            self.updater.mark_booted()?;
            self.boot = BootState::Booted;
        }
        Ok(())
    }

    pub fn set_network(&mut self, s: &str) {
        self.network.clear();
        let _ = self.network.push_str(&s[..s.len().min(48)]);
    }
}

pub type Shared = Mutex<NoopRawMutex, RefCell<Device>>;

/// Cheap handle passed to tasks. All tasks run on the same executor, so a
/// `NoopRawMutex` suffices; each call borrows the device only briefly.
#[derive(Clone, Copy)]
pub struct Handle(pub &'static Shared);

impl Handle {
    pub fn with<R>(&self, f: impl FnOnce(&mut Device) -> R) -> R {
        self.0.lock(|d| f(&mut d.borrow_mut()))
    }
}

fn ota_error(e: UpdateError) -> OtaError {
    match e {
        UpdateError::Sequence => OtaError::Sequence,
        UpdateError::TooLarge => OtaError::TooLarge,
        UpdateError::Image(_) => OtaError::Invalid,
        UpdateError::Flash => OtaError::Flash,
        UpdateError::NotBooted => OtaError::NotConfirmed,
        UpdateError::Rejected => OtaError::WrongBoard,
    }
}

impl Ota for Handle {
    fn running_version(&self) -> &str {
        VERSION
    }

    fn ota_begin(&mut self, len: u32) -> Result<(), OtaError> {
        self.with(|d| {
            if d.ota_activity
                .is_some_and(|t| t.elapsed() < OTA_IDLE_TIMEOUT)
            {
                return Err(OtaError::Busy);
            }
            d.updater.begin(len).map_err(ota_error)?;
            d.ota_activity = Some(Instant::now());
            log::info!("update started ({len} bytes)");
            Ok(())
        })
    }

    fn ota_write(&mut self, offset: u32, data: &[u8]) -> Result<(), OtaError> {
        self.with(|d| {
            d.ota_activity = Some(Instant::now());
            d.updater.write(offset, data).map_err(ota_error)
        })
    }

    fn ota_finish(&mut self) -> Result<String<32>, OtaError> {
        self.with(|d| {
            d.ota_activity = None;
            let header = d
                .updater
                .finish_checked(|h| {
                    let board = image_board(h.version_str());
                    if board != crate::board::NAME {
                        log::warn!(
                            "update {} is for the {board} board; this is the {} board",
                            h.version_str(),
                            crate::board::NAME
                        );
                    }
                    board == crate::board::NAME
                })
                .map_err(|e| {
                    log::warn!("update rejected: {e:?}");
                    ota_error(e)
                })?;
            log::info!(
                "update {} staged; swapped in on next boot",
                header.version_str()
            );
            let mut v = String::new();
            let _ = v.push_str(header.version_str());
            Ok(v)
        })
    }

    fn ota_abort(&mut self) {
        self.with(|d| {
            d.ota_activity = None;
            d.updater.abort();
        })
    }
}

impl Platform for Handle {
    fn settings(&self) -> Settings {
        self.with(|d| d.settings.clone())
    }

    fn save_settings(&mut self, settings: &Settings) -> Result<(), StoreError> {
        self.with(|d| {
            d.store.save(&mut d.settings_flash, settings)?;
            d.settings.clone_from(settings);
            Ok(())
        })
    }

    fn random(&mut self, buf: &mut [u8]) {
        self.with(|d| {
            for chunk in buf.chunks_mut(4) {
                let r = d.rng.random().to_le_bytes();
                chunk.copy_from_slice(&r[..chunk.len()]);
            }
        })
    }

    fn now_ms(&self) -> u64 {
        Instant::now().as_millis()
    }

    fn status(&mut self) -> DeviceStatus {
        self.with(|d| {
            let now = Instant::now().as_millis();
            let zones: heapless::Vec<ZoneStatus, MAX_ZONES> = d
                .heating
                .zones()
                .iter()
                .map(|z| ZoneStatus {
                    heating: z.heating(),
                    reason: z.reason.as_str(),
                    hold_remaining_s: z.hold_remaining_ms(now).map(|ms| ms / 1000),
                })
                .collect();
            // Summary: the first heating zone (or the first zone) and the longest hold.
            let lead = zones.iter().find(|z| z.heating).unwrap_or(&zones[0]);
            DeviceStatus {
                boot: d.boot,
                uptime_s: Instant::now().as_secs(),
                network: d.network.clone(),
                readings: d.readings.clone(),
                heating: d.heating.heating(),
                heat_request: d.heating.requesting(),
                heating_reason: lead.reason,
                hold_remaining_s: zones.iter().filter_map(|z| z.hold_remaining_s).max(),
                zones: zones.clone(),
                stack_used: crate::stack::peak_used() as u32,
                stack_size: crate::stack::size() as u32,
                unix_time: d.clock.unix_s(Instant::now().as_micros()),
                time_synced_ago_s: d.clock.since_sync_s(Instant::now().as_micros()),
            }
        })
    }

    fn zone_count(&self) -> usize {
        crate::board::ZONES
    }

    fn request_reboot(&mut self) {
        self.with(|d| d.reboot_requested = true)
    }

    fn sessions<R>(&mut self, f: impl FnOnce(&mut Sessions) -> R) -> R {
        self.with(|d| f(&mut d.sessions))
    }

    fn zones<R>(&mut self, f: impl FnOnce(&mut Group) -> R) -> R {
        self.with(|d| f(&mut d.group))
    }

    #[cfg(feature = "qemu")]
    fn inject_reading(&mut self, addr: [u8; 6], temperature_cc: i16) -> bool {
        let now = Instant::now().as_millis();
        self.with(|d| d.ble.inject(addr, temperature_cc, now));
        // Act on it now rather than at the next control period, so tests can
        // make two zones decide at the same moment.
        crate::heating::RECHECK.signal(());
        true
    }

    fn ble<R>(&mut self, f: impl FnOnce(Option<&BleTable>) -> R) -> R {
        // The emulator has no Bluetooth radio (only injected test readings).
        self.with(|d| f(Some(&d.ble)))
    }
}
