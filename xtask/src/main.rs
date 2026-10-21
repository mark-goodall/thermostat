//! `cargo xtask <command>`: build images, run QEMU, flash and update devices.

mod e2e;
mod espimage;
mod layout;
mod mdns;
mod ota;
mod provision;
mod qemu;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Thermostat firmware tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build bootloader + application and assemble flash images.
    Build(BuildArgs),
    /// Boot the merged flash image in Espressif's QEMU (Ethernet via slirp).
    Qemu {
        #[command(flatten)]
        build: BuildArgs,
        /// Host port forwarded to the device's port 80.
        #[arg(long, default_value_t = 8080)]
        http_port: u16,
        /// Expose the console on a TCP port instead of stdio (for serial OTA).
        #[arg(long)]
        serial_port: Option<u16>,
        /// Keep the existing flash file (settings, staged updates) instead of rewriting it.
        #[arg(long)]
        keep_flash: bool,
    },
    /// Flash bootloader, partition table and app over a serial port.
    Flash {
        #[command(flatten)]
        build: BuildArgs,
        #[arg(long)]
        port: Option<String>,
        /// Also erase the settings partition.
        #[arg(long)]
        erase_settings: bool,
    },
    /// Send an application image over the serial update protocol.
    SerialOta {
        image: PathBuf,
        /// Serial device (e.g. /dev/ttyUSB0) or tcp:HOST:PORT for QEMU.
        #[arg(long)]
        port: String,
        #[arg(long, default_value_t = 115_200)]
        baud: u32,
        #[arg(long)]
        no_reboot: bool,
    },
    /// Upload an application image through the web API.
    HttpOta {
        image: PathBuf,
        /// Device address, e.g. 192.168.1.20 or localhost:8080.
        #[arg(long)]
        host: String,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(long, env = "THERMOSTAT_PASSWORD")]
        password: String,
    },
    /// Print the header of an application image.
    ImageInfo { image: PathBuf },
    /// Build and run the end-to-end tests in QEMU.
    TestQemu {
        #[arg(long)]
        release: bool,
        /// Only run the two-zone (shared heater) scenario.
        #[arg(long)]
        zones_only: bool,
    },
    /// Write device settings (admin login, WiFi, ...) into the settings partition over serial.
    Provision(provision::Args),
    /// Combine the bootloader, partition table and an existing app image into a flash file.
    Assemble {
        app_image: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Pack an arbitrary esp-hal ELF into an application image (bring-up aid).
    PackElf {
        elf: PathBuf,
        #[arg(long, default_value = "dev")]
        version: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(clap::Args, Clone)]
pub struct BuildArgs {
    #[arg(long)]
    pub release: bool,
    /// Firmware variant: `qemu` (emulated Ethernet) or `wifi` (hardware).
    #[arg(long, default_value = "wifi")]
    pub target: String,
    /// Override the version string embedded in the image.
    #[arg(long)]
    pub version: Option<String>,
    /// Extra cargo features for the app (comma separated).
    #[arg(long, default_value = "")]
    pub features: String,
}

pub struct Artifacts {
    pub dir: PathBuf,
    pub bootloader: PathBuf,
    pub partitions: PathBuf,
    pub app: PathBuf,
    pub merged: PathBuf,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build(args) => {
            let a = build(&args)?;
            println!("images in {}", a.dir.display());
            Ok(())
        }
        Cmd::Qemu {
            build: args,
            http_port,
            serial_port,
            keep_flash,
        } => {
            let a = build(&BuildArgs {
                target: "qemu".into(),
                ..args
            })?;
            let flash = a.dir.join("qemu-flash.bin");
            if !keep_flash || !flash.exists() {
                std::fs::copy(&a.merged, &flash)?;
            }
            let console = match serial_port {
                Some(p) => qemu::Console::Tcp(p),
                None => qemu::Console::Stdio,
            };
            let status = qemu::command(&flash, http_port, &console, None)?.status()?;
            if !status.success() {
                bail!("qemu exited with {status}");
            }
            Ok(())
        }
        Cmd::Flash {
            build: args,
            port,
            erase_settings,
        } => flash(&build(&args)?, port, erase_settings),
        Cmd::SerialOta {
            image,
            port,
            baud,
            no_reboot,
        } => ota::serial(&std::fs::read(image)?, &port, baud, !no_reboot),
        Cmd::HttpOta {
            image,
            host,
            user,
            password,
        } => ota::http(&std::fs::read(image)?, &host, &user, &password),
        Cmd::ImageInfo { image } => image_info(&image),
        Cmd::TestQemu {
            release,
            zones_only,
        } => qemu::run_tests(release, zones_only),
        Cmd::Provision(args) => provision::run(&args),
        Cmd::Assemble { app_image, out } => {
            let bl = build_bootloader()?;
            std::fs::write(out, merge(&bl, &std::fs::read(app_image)?)?)?;
            Ok(())
        }
        Cmd::PackElf { elf, version, out } => {
            let img = espimage::app_image(&espimage::read_elf(&elf)?, &version)?;
            std::fs::write(&out, img)?;
            Ok(())
        }
    }
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Run cargo with the Xtensa toolchain for one firmware package.
fn cargo_esp(
    package: &str,
    release: bool,
    features: &str,
    extra_env: &[(&str, String)],
) -> Result<PathBuf> {
    let root = workspace_root();
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&root)
        .arg("+esp")
        .args([
            "build",
            "-p",
            package,
            "--target",
            "xtensa-esp32-none-elf",
            "-Zbuild-std=core,alloc",
        ])
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env_remove("RUSTC");
    if release {
        cmd.arg("--release");
    }
    if !features.is_empty() {
        cmd.args(["--features", features]);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Make the espup environment available when it has not been sourced.
    if let Some(path) = esp_path() {
        cmd.env("PATH", path);
    }
    let status = cmd
        .status()
        .context("running cargo +esp (is espup installed?)")?;
    if !status.success() {
        bail!("building {package} failed");
    }
    let profile = if release { "release" } else { "debug" };
    Ok(root
        .join("target/xtensa-esp32-none-elf")
        .join(profile)
        .join(package))
}

/// PATH with the Xtensa GCC from ~/export-esp.sh prepended, if found.
fn esp_path() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let export = std::fs::read_to_string(Path::new(&home).join("export-esp.sh")).ok()?;
    let mut path = std::env::var("PATH").unwrap_or_default();
    for line in export.lines() {
        if let Some(v) = line
            .strip_prefix("export PATH=\"")
            .and_then(|l| l.strip_suffix(":$PATH\""))
        {
            path = format!("{v}:{path}");
        }
    }
    Some(path)
}

pub fn build(args: &BuildArgs) -> Result<Artifacts> {
    let root = workspace_root();
    let dir = root.join("target/images").join(&args.target);
    std::fs::create_dir_all(&dir)?;

    let bl = build_bootloader()?;

    let mut features = args.target.clone();
    if !args.features.is_empty() {
        features = format!("{features},{}", args.features);
    }
    let version = args.version.clone().unwrap_or_else(default_version);
    let app_elf = cargo_esp(
        "thermostat-firmware",
        args.release,
        &features,
        &[("THERMOSTAT_VERSION", version.clone())],
    )?;
    let app = espimage::app_image(&espimage::read_elf(&app_elf)?, &version)?;
    let active = layout::active();

    let table = layout::table_bin();
    let merged = merge(&bl, &app)?;

    let a = Artifacts {
        bootloader: dir.join("bootloader.bin"),
        partitions: dir.join("partitions.bin"),
        app: dir.join("thermostat.img"),
        merged: dir.join("flash.bin"),
        dir,
    };
    std::fs::write(&a.bootloader, &bl)?;
    std::fs::write(&a.partitions, &table)?;
    std::fs::write(&a.app, &app)?;
    std::fs::write(&a.merged, &merged)?;
    println!(
        "bootloader {} B, app {} B ({}% of active), version {version}",
        bl.len(),
        app.len(),
        app.len() as u64 * 100 / active.size as u64
    );
    Ok(a)
}

fn build_bootloader() -> Result<Vec<u8>> {
    let flash_mib = (layout::FLASH_SIZE >> 20).to_string();
    let elf = cargo_esp(
        "thermostat-bootloader",
        true,
        "",
        &[("THERMOSTAT_FLASH_SIZE", flash_mib)],
    )?;
    let bl = espimage::rom_image(&espimage::read_elf(&elf)?, layout::FLASH_SIZE)?;
    if bl.len() as u32 > 0x8000 - layout::BOOTLOADER_OFFSET {
        bail!("bootloader image too large ({} bytes)", bl.len());
    }
    Ok(bl)
}

/// A full flash image: bootloader, partition table, app; everything else erased.
fn merge(bootloader: &[u8], app: &[u8]) -> Result<Vec<u8>> {
    let active = layout::active();
    if app.len() as u32 > active.size {
        bail!(
            "application image ({} bytes) does not fit the active partition",
            app.len()
        );
    }
    let table = layout::table_bin();
    let mut merged = vec![0xFFu8; layout::FLASH_SIZE as usize];
    merged[layout::BOOTLOADER_OFFSET as usize..][..bootloader.len()].copy_from_slice(bootloader);
    merged[0x8000..0x8000 + table.len()].copy_from_slice(&table);
    merged[active.offset as usize..][..app.len()].copy_from_slice(app);
    Ok(merged)
}

fn default_version() -> String {
    let base = env!("CARGO_PKG_VERSION");
    let rev = Command::new("git")
        .args(["describe", "--always", "--dirty", "--exclude", "*"])
        .current_dir(workspace_root())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match rev {
        Some(r) if !r.is_empty() => format!("{base}+{r}"),
        _ => base.to_string(),
    }
}

fn flash(a: &Artifacts, port: Option<String>, erase_settings: bool) -> Result<()> {
    let mut cmd = Command::new("espflash");
    cmd.args(["write-bin", "--chip", "esp32"]);
    if let Some(p) = &port {
        cmd.args(["--port", p]);
    }
    let run = |cmd: &mut Command, addr: u32, file: &Path| -> Result<()> {
        let status = cmd
            .arg(format!("{addr:#x}"))
            .arg(file)
            .status()
            .context("running espflash")?;
        if !status.success() {
            bail!("espflash failed");
        }
        Ok(())
    };
    // espflash write-bin takes one file per invocation.
    let base: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let fresh = || {
        let mut c = Command::new("espflash");
        c.args(&base);
        c
    };
    run(&mut fresh(), layout::BOOTLOADER_OFFSET, &a.bootloader)?;
    run(&mut fresh(), 0x8000, &a.partitions)?;
    // A blank state partition means "boot the active image"; clear any stale swap state.
    let state = layout::partitions()
        .into_iter()
        .find(|p| p.label() == "bootstate")
        .unwrap();
    let blank = a.dir.join("blank-state.bin");
    std::fs::write(&blank, vec![0xFFu8; state.size as usize])?;
    run(&mut fresh(), state.offset, &blank)?;
    if erase_settings {
        let settings = layout::partitions()
            .into_iter()
            .find(|p| p.label() == "settings")
            .unwrap();
        let blank = a.dir.join("blank-settings.bin");
        std::fs::write(&blank, vec![0xFFu8; settings.size as usize])?;
        run(&mut fresh(), settings.offset, &blank)?;
    }
    run(&mut fresh(), layout::active().offset, &a.app)
}

fn image_info(path: &Path) -> Result<()> {
    use embassy_boot_esp::image;
    let data = std::fs::read(path)?;
    let info = image::inspect(&data, data.len() as u32, image::CHIP_ESP32, &image::ESP32)
        .map_err(|e| anyhow::anyhow!("invalid image: {e:?}"))?;
    println!("version   {}", info.header.version_str());
    println!("entry     {:#010x}", info.header.entry);
    println!("length    {} (+32 byte SHA-256)", info.header.image_len);
    for s in info.segments() {
        println!(
            "  {:?}\t{:#010x}  {:>7} B  @ {:#x}",
            s.kind, s.vaddr, s.len, s.offset
        );
    }
    let mut buf = [0u8; 4096];
    let res = image::verify(&info.header, &mut buf, |off, b| {
        b.copy_from_slice(&data[off as usize..off as usize + b.len()]);
        Ok::<_, ()>(())
    });
    println!("sha256    {}", if res.is_ok() { "ok" } else { "MISMATCH" });
    Ok(())
}
