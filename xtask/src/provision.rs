//! Factory provisioning: build a settings partition image on the host and
//! write it over serial, so a device can be configured without the web UI.
//! Uses the firmware's own settings store code, so the format always matches.

use std::process::Command;

use anyhow::{Context, Result, bail};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use thermostat_core::auth::{DEFAULT_ITERATIONS, PasswordHash, wpa_psk};
use thermostat_core::settings::{Settings, str32};
use thermostat_core::store::SettingsStore;

use crate::layout;

#[derive(clap::Args)]
pub struct Args {
    /// Serial port; omit to only write the image file.
    #[arg(long)]
    port: Option<String>,
    #[arg(long, default_value = "admin")]
    admin_user: String,
    #[arg(long, env = "THERMOSTAT_PASSWORD")]
    admin_password: String,
    #[arg(long, default_value = "thermostat")]
    hostname: String,
    /// WiFi network to join; omit to keep the device in setup-AP mode.
    #[arg(long)]
    ssid: Option<String>,
    #[arg(long, env = "THERMOSTAT_WIFI_PASSWORD", requires = "ssid")]
    wifi_password: Option<String>,
    /// Setpoint in °C, e.g. 20.5
    #[arg(long, default_value = "20.0")]
    setpoint: String,
    /// Heater group id shared by zones on one heater.
    #[arg(long, default_value = "")]
    heater_id: String,
    /// Group key (same on every zone of the group) signing zone messages.
    #[arg(long, env = "THERMOSTAT_GROUP_KEY", default_value = "")]
    group_key: String,
    /// POSIX TZ string, e.g. GMT0BST,M3.5.0/1,M10.5.0
    #[arg(long, default_value = "UTC0")]
    timezone: String,
}

struct RamFlash(Vec<u8>);

#[derive(Debug)]
struct RamError;
impl NorFlashError for RamError {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}
impl ErrorType for RamFlash {
    type Error = RamError;
}
impl ReadNorFlash for RamFlash {
    const READ_SIZE: usize = 4;
    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), RamError> {
        bytes.copy_from_slice(&self.0[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }
    fn capacity(&self) -> usize {
        self.0.len()
    }
}
impl NorFlash for RamFlash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;
    fn erase(&mut self, from: u32, to: u32) -> Result<(), RamError> {
        self.0[from as usize..to as usize].fill(0xFF);
        Ok(())
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), RamError> {
        for (d, s) in self.0[offset as usize..].iter_mut().zip(bytes) {
            *d &= s;
        }
        Ok(())
    }
}

/// The settings partition contents holding `settings` (one record).
pub fn settings_image(settings: &Settings) -> Result<Vec<u8>> {
    let part = settings_partition();
    let mut flash = RamFlash(vec![0xFF; part.size as usize]);
    let (mut store, _) = SettingsStore::load(&mut flash).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    store
        .save(&mut flash, settings)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(flash.0)
}

pub fn settings_partition() -> embassy_boot_esp::partition::Partition {
    layout::partitions()
        .into_iter()
        .find(|p| p.label() == "settings")
        .unwrap()
}

pub fn random_salt() -> Result<[u8; 16]> {
    use std::io::Read;
    let mut salt = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut salt)?;
    Ok(salt)
}

pub fn run(args: &Args) -> Result<()> {
    if args.admin_password.len() < 8 {
        bail!("admin password must be at least 8 characters");
    }
    let setpoint =
        thermostat_core::http::parse_tenths(&args.setpoint).context("setpoint like 20.5")?;
    let mut settings = Settings {
        hostname: str32(&args.hostname),
        admin_user: str32(&args.admin_user),
        admin_hash: Some(PasswordHash::new(
            &args.admin_password,
            random_salt()?,
            DEFAULT_ITERATIONS,
        )),
        setpoint_dc: setpoint as i16,
        heater_id: str32(&args.heater_id),
        group_key: str32(&args.group_key),
        timezone: thermostat_core::settings::str48(&args.timezone),
        ..Settings::default()
    };
    if !args.group_key.is_empty() && args.group_key.len() < 8 {
        bail!("group key must be at least 8 characters");
    }
    thermostat_core::clock::TimeZone::parse(&args.timezone)
        .map_err(|_| anyhow::anyhow!("invalid POSIX TZ string"))?;
    if let (Some(ssid), Some(pass)) = (&args.ssid, &args.wifi_password) {
        if !(8..=63).contains(&pass.len()) {
            bail!("WiFi password must be 8-63 characters");
        }
        settings.wifi_ssid = str32(ssid);
        settings.wifi_psk = Some(wpa_psk(pass, ssid));
    }

    let image = settings_image(&settings)?;
    let part = settings_partition();
    let out = crate::workspace_root().join("target/images/settings.bin");
    std::fs::create_dir_all(out.parent().unwrap())?;
    std::fs::write(&out, &image)?;
    eprintln!("settings image: {}", out.display());

    if let Some(port) = &args.port {
        let status = Command::new("espflash")
            .args(["write-bin", "--chip", "esp32", "--port", port])
            .arg(format!("{:#x}", part.offset))
            .arg(&out)
            .status()
            .context("running espflash")?;
        if !status.success() {
            bail!("espflash failed");
        }
    }
    Ok(())
}
