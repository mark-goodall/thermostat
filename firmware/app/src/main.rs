//! Thermostat firmware for the ESP32.
//!
//! Boot flow: the embassy-boot-esp bootloader has already swapped in any
//! staged update. If this image runs on trial, it confirms itself once the
//! network is up; if it crashes or hangs before that, the watchdog resets
//! the chip and the bootloader rolls back to the previous image.
#![no_std]
#![no_main]

extern crate alloc;

#[cfg(feature = "wifi")]
mod ble;
mod board;
mod device;
mod heating;
mod mdns;
mod net;
mod serial;
mod sntp;
mod web;
mod zones;

use core::cell::RefCell;

use embassy_boot_esp::partition::{self, BootPartitions};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::{MwdtStage, TimerGroup, Wdt};
use esp_storage::FlashStorage;
use static_cell::StaticCell;
use thermostat_core::platform::BootState;
use thermostat_core::settings::{SETTINGS_SUBTYPE, SETTINGS_TYPE};
use thermostat_core::store::SettingsStore;

use device::{Device, Flash, Handle, Shared, VERSION};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("\n!! panic: {info}\n!! resetting");
    esp_hal::system::software_reset()
}

/// A trial image confirms itself after the network has been up this long.
const CONFIRM_AFTER: Duration = Duration::from_secs(10);
const WATCHDOG_TIMEOUT: esp_hal::time::Duration = esp_hal::time::Duration::from_secs(15);

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    // Raising the CPU clock also changes the APB clock the ROM programmed the
    // console UART's baud divider for; reconfigure UART0 before logging.
    let console_rx = esp_hal::uart::UartRx::new(peripherals.UART0, Default::default())
        .expect("uart")
        .with_rx(peripherals.GPIO3)
        .into_async();
    // DRAM used by the ROM loader and our bootloader is free now (esp-hal's
    // `ram(reclaimed)` attribute does the same but insists on ESP-IDF's bootloader).
    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 98768);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    let mut wdt = timg0.wdt;
    wdt.set_timeout(MwdtStage::Stage0, WATCHDOG_TIMEOUT);
    wdt.enable();

    log::info!(
        "thermostat {VERSION} starting ({} board, {} zone(s))",
        board::NAME,
        board::ZONES
    );

    static FLASH: StaticCell<Flash> = StaticCell::new();
    let flash: &'static Flash = FLASH.init(Mutex::new(RefCell::new(FlashStorage::new(
        peripherals.FLASH,
    ))));

    let mut table = [0u8; partition::TABLE_LEN];
    flash
        .lock(|f| partition::read_table(&mut *f.borrow_mut(), &mut table))
        .expect("read partition table");
    let parts = BootPartitions::from_table(&table).expect("boot partitions");
    let settings_part =
        partition::find(&table, SETTINGS_TYPE, SETTINGS_SUBTYPE).expect("settings partition");

    let mut settings_flash =
        BlockingPartition::new(flash, settings_part.offset, settings_part.size);
    let (store, settings) = match SettingsStore::load(&mut settings_flash) {
        Ok((store, Some(s))) => {
            log::info!(
                "settings loaded: hostname '{}', wifi '{}', setpoint {}",
                s.hostname,
                s.wifi_ssid,
                thermostat_core::http::Tenths(s.setpoint_dc as i32)
            );
            (store, s)
        }
        Ok((store, None)) => {
            log::info!("no stored settings; using defaults");
            (store, Default::default())
        }
        Err(e) => panic!("settings partition: {e:?}"),
    };
    let settings = board::fit(settings);

    static ALIGNED: StaticCell<embassy_boot::AlignedBuffer<4>> = StaticCell::new();
    let aligned = &mut ALIGNED.init(embassy_boot::AlignedBuffer([0; 4])).0;
    let updater = embassy_boot_esp::Updater::new(
        flash,
        &parts,
        aligned,
        embassy_boot_esp::image::CHIP_ESP32,
        &embassy_boot_esp::image::ESP32,
    );

    static DEVICE: StaticCell<Shared> = StaticCell::new();
    let dev = Handle(DEVICE.init(Mutex::new(RefCell::new(Device::new(
        settings,
        store,
        settings_flash,
        updater,
    )))));
    let boot = dev.with(|d| d.boot);
    log::info!("boot state: {}", boot.as_str());
    if dev.with(|d| !d.settings.is_provisioned()) {
        log::info!("not provisioned: open the web UI to create the admin account");
    }

    let stack = net::start(
        &spawner,
        net::Peripherals::take(peripherals.WIFI, peripherals.ADC2),
        dev,
    )
    .await;

    spawner.spawn(mdns::task(stack, dev).unwrap());
    spawner.spawn(sntp::task(stack, dev).unwrap());
    spawner.spawn(zones::task(stack, dev).unwrap());
    // Board pins: zone relays on GPIO16 (and GPIO17 on the dual board), active
    // high; status LED on GPIO23.
    let out = |pin: esp_hal::gpio::AnyPin<'static>| {
        esp_hal::gpio::Output::new(
            pin,
            esp_hal::gpio::Level::Low,
            esp_hal::gpio::OutputConfig::default(),
        )
    };
    #[cfg(not(feature = "board-dual"))]
    let relays = [out(peripherals.GPIO16.into())];
    #[cfg(feature = "board-dual")]
    let relays = [
        out(peripherals.GPIO16.into()),
        out(peripherals.GPIO17.into()),
    ];
    spawner.spawn(heating::task(relays, dev).unwrap());
    spawner.spawn(heating::led_task(out(peripherals.GPIO23.into())).unwrap());
    #[cfg(feature = "wifi")]
    spawner.spawn(ble::task(peripherals.BT, dev).unwrap());
    for _ in 0..web::TASKS {
        spawner.spawn(web::task(stack, dev).unwrap());
    }
    spawner.spawn(serial::task(console_rx, dev).unwrap());

    supervise(stack, dev, &mut wdt, boot).await
}

/// Feed the watchdog, confirm trial images and carry out reboot requests.
async fn supervise(
    stack: Stack<'static>,
    dev: Handle,
    wdt: &mut Wdt<esp_hal::peripherals::TIMG0<'static>>,
    boot: BootState,
) -> ! {
    let mut up_since: Option<embassy_time::Instant> = None;
    let mut confirmed = boot == BootState::Booted;
    loop {
        wdt.feed();
        if !confirmed {
            if stack.is_config_up() {
                let since = *up_since.get_or_insert_with(embassy_time::Instant::now);
                if since.elapsed() >= CONFIRM_AFTER {
                    if cfg!(feature = "fail-selftest") {
                        log::warn!(
                            "fail-selftest: refusing to confirm; hanging until the watchdog fires"
                        );
                        loop {
                            core::hint::spin_loop();
                        }
                    }
                    match dev.with(|d| d.confirm_boot()) {
                        Ok(()) => log::info!("image confirmed"),
                        Err(e) => log::error!("confirming image failed: {e:?}"),
                    }
                    confirmed = true;
                }
            } else {
                up_since = None;
            }
        }
        if dev.with(|d| d.reboot_requested) {
            log::info!("rebooting");
            // Give the HTTP response / serial ACK time to leave.
            Timer::after(Duration::from_millis(700)).await;
            esp_hal::system::software_reset();
        }
        Timer::after(Duration::from_millis(250)).await;
    }
}
