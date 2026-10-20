//! Network bring-up. Exactly one of the `qemu` / `wifi` features selects the link.

use embassy_net::{Stack, StackResources};
use static_cell::StaticCell;

use crate::device::Handle;

#[cfg(all(feature = "qemu", feature = "wifi"))]
compile_error!("enable only one of the `qemu` and `wifi` features");
#[cfg(not(any(feature = "qemu", feature = "wifi")))]
compile_error!("enable one of the `qemu` (emulator) or `wifi` (hardware) features");

pub use imp::{Peripherals, start};

/// DHCP client/server, 2 web connections, mDNS, plus headroom.
const SOCKETS: usize = 8;

fn resources() -> &'static mut StackResources<SOCKETS> {
    static RES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    RES.init(StackResources::new())
}

fn seed(dev: Handle) -> u64 {
    dev.with(|d| (d.rng.random() as u64) << 32 | d.rng.random() as u64)
}

fn dhcp_config(dev: Handle) -> embassy_net::DhcpConfig {
    let mut cfg = embassy_net::DhcpConfig::default();
    cfg.hostname = dev.with(|d| heapless::String::try_from(d.settings.hostname.as_str()).ok());
    cfg
}

/// Publish the address once DHCP completes.
#[embassy_executor::task]
async fn link_monitor(stack: Stack<'static>, dev: Handle, label: &'static str) {
    loop {
        stack.wait_config_up().await;
        if let Some(cfg) = stack.config_v4() {
            let mut s = heapless::String::<48>::new();
            let _ = core::fmt::write(&mut s, format_args!("{label} {}", cfg.address.address()));
            log::info!("network up: {s}");
            dev.with(|d| d.set_network(&s));
        }
        stack.wait_config_down().await;
        dev.with(|d| d.set_network("down"));
    }
}

#[cfg(feature = "qemu")]
mod imp {
    use embassy_executor::Spawner;
    use embassy_net::{Runner, Stack};
    use esp_hal::handler;
    use esp_hal::interrupt::{self, Priority};
    use esp_hal::peripherals::Interrupt;
    use esp_openeth::OpenEth;

    use super::*;

    /// Nothing to hand over: the emulated MAC has no esp-hal peripheral.
    pub struct Peripherals;

    impl Peripherals {
        pub fn take(
            _wifi: esp_hal::peripherals::WIFI<'static>,
            _adc2: esp_hal::peripherals::ADC2<'static>,
        ) -> Self {
            Self
        }
    }

    #[handler(priority = Priority::Priority1)]
    fn eth_interrupt() {
        esp_openeth::on_interrupt();
    }

    #[embassy_executor::task]
    async fn net_task(mut runner: Runner<'static, OpenEth>) -> ! {
        runner.run().await
    }

    pub async fn start(spawner: &Spawner, _p: Peripherals, dev: Handle) -> Stack<'static> {
        // Locally administered address; slirp does not care.
        let driver = OpenEth::new([0x02, 0x00, 0x00, 0x12, 0x34, 0x56])
            .expect("open_eth MAC missing: run under QEMU with `-nic user,model=open_eth`");
        interrupt::bind_handler(Interrupt::ETH_MAC, eth_interrupt);
        let config = embassy_net::Config::dhcpv4(dhcp_config(dev));
        let (stack, runner) = embassy_net::new(driver, config, resources(), seed(dev));
        spawner.spawn(net_task(runner).unwrap());
        spawner.spawn(link_monitor(stack, dev, "eth").unwrap());
        stack
    }
}

#[cfg(feature = "wifi")]
mod imp {
    use embassy_executor::Spawner;
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use embassy_net::{Ipv4Cidr, Runner, Stack, StaticConfigV4};
    use embassy_time::{Duration, Instant, Timer};
    use esp_radio::wifi::ap::AccessPointConfig;
    use esp_radio::wifi::sta::StationConfig;
    use esp_radio::wifi::{
        AuthenticationMethodConfig, Config, Interface, PowerSaveMode, WifiController,
    };
    use thermostat_core::dhcp::{self, DhcpServer};

    use super::*;

    pub struct Peripherals {
        wifi: esp_hal::peripherals::WIFI<'static>,
    }

    impl Peripherals {
        pub fn take(
            wifi: esp_hal::peripherals::WIFI<'static>,
            _adc2: esp_hal::peripherals::ADC2<'static>,
        ) -> Self {
            Self { wifi }
        }
    }

    /// Set before a reset to come up as the setup access point once, e.g.
    /// when the configured network cannot be joined. Lives in RTC memory,
    /// which survives a software reset.
    #[esp_hal::ram(unstable(rtc_fast, persistent))]
    static mut FORCE_AP: u32 = 0;
    const FORCE_AP_MAGIC: u32 = 0x4150_4150;

    /// Give up on the configured network after this long and offer the setup AP.
    const STA_GIVE_UP: Duration = Duration::from_secs(180);
    /// The setup AP reboots back into station mode after this long.
    const AP_LIFETIME: Duration = Duration::from_secs(600);
    const AP_IP: [u8; 4] = [192, 168, 4, 1];

    #[embassy_executor::task]
    async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
        runner.run().await
    }

    pub async fn start(spawner: &Spawner, p: Peripherals, dev: Handle) -> Stack<'static> {
        let controller = WifiController::new(p.wifi, Default::default()).expect("wifi init");
        let (ssid, psk) = dev.with(|d| (d.settings.wifi_ssid.clone(), d.settings.wifi_psk_hex()));
        let forced_ap = unsafe { core::ptr::replace(&raw mut FORCE_AP, 0) } == FORCE_AP_MAGIC;

        match psk {
            Some(psk) if !ssid.is_empty() && !forced_ap => {
                let iface = Interface::station();
                let config = embassy_net::Config::dhcpv4(dhcp_config(dev));
                let (stack, runner) = embassy_net::new(iface, config, resources(), seed(dev));
                spawner.spawn(net_task(runner).unwrap());
                spawner.spawn(link_monitor(stack, dev, "wifi").unwrap());
                spawner.spawn(station(controller, ssid, psk, dev).unwrap());
                stack
            }
            _ => {
                let iface = Interface::access_point();
                let mac = iface.mac_address();
                let config = embassy_net::Config::ipv4_static(StaticConfigV4 {
                    address: Ipv4Cidr::new(AP_IP.into(), 24),
                    gateway: None,
                    dns_servers: Default::default(),
                });
                let (stack, runner) = embassy_net::new(iface, config, resources(), seed(dev));
                spawner.spawn(net_task(runner).unwrap());
                spawner.spawn(access_point(controller, mac, dev).unwrap());
                spawner.spawn(dhcp_server(stack).unwrap());
                stack
            }
        }
    }

    #[embassy_executor::task]
    async fn station(
        mut controller: WifiController<'static>,
        ssid: heapless::String<32>,
        psk: heapless::String<64>,
        dev: Handle,
    ) {
        // A 64 hex digit password is taken as the raw PSK, so the passphrase
        // never has to be stored on the device.
        let config = Config::Station(
            StationConfig::default()
                .with_ssid(ssid.as_str().try_into().expect("ssid"))
                .with_authentication(AuthenticationMethodConfig::Wpa2Personal(
                    psk.as_str().try_into().expect("psk"),
                )),
        );
        if let Err(e) = controller.set_config(&config) {
            log::error!("wifi config: {e:?}");
        }
        // Modem sleep between beacons frees the shared radio for the BLE
        // scanner; with power saving off WiFi starves Bluetooth coexistence.
        if let Err(e) = controller.set_power_saving(PowerSaveMode::Minimum) {
            log::warn!("wifi power save: {e:?}");
        }
        let mut last_connected = Instant::now();
        loop {
            log::info!("joining {ssid}");
            dev.with(|d| d.set_network("wifi connecting"));
            match controller.connect_async().await {
                Ok(_) => {
                    log::info!("wifi connected");
                    let _ = controller.wait_for_disconnect_async().await;
                    log::warn!("wifi disconnected");
                    last_connected = Instant::now();
                }
                Err(e) => {
                    log::warn!("wifi connect failed: {e:?}");
                    if last_connected.elapsed() > STA_GIVE_UP {
                        log::warn!("cannot join {ssid}; restarting into setup access point");
                        unsafe { core::ptr::write(&raw mut FORCE_AP, FORCE_AP_MAGIC) };
                        dev.with(|d| d.reboot_requested = true);
                        return;
                    }
                    Timer::after(Duration::from_secs(5)).await;
                }
            }
        }
    }

    #[embassy_executor::task]
    async fn access_point(mut controller: WifiController<'static>, mac: [u8; 6], dev: Handle) {
        let mut name = heapless::String::<32>::new();
        let _ = core::fmt::write(
            &mut name,
            format_args!("thermostat-{:02x}{:02x}", mac[4], mac[5]),
        );
        let config = Config::AccessPoint(
            AccessPointConfig::default()
                .with_ssid(name.as_str().try_into().expect("ssid"))
                .with_authentication(AuthenticationMethodConfig::Open),
        );
        match controller.set_config(&config) {
            Ok(()) => {
                log::info!("setup access point '{name}' at http://192.168.4.1/");
                let mut s = heapless::String::<48>::new();
                let _ = core::fmt::write(&mut s, format_args!("setup AP {name} 192.168.4.1"));
                dev.with(|d| d.set_network(&s));
            }
            Err(e) => log::error!("access point: {e:?}"),
        }
        Timer::after(AP_LIFETIME).await;
        if dev.with(|d| !d.settings.wifi_ssid.is_empty()) {
            log::info!("setup window over; retrying the configured network");
            dev.with(|d| d.reboot_requested = true);
        }
        core::mem::forget(controller);
    }

    #[embassy_executor::task]
    async fn dhcp_server(stack: Stack<'static>) {
        let mut rx_meta = [PacketMetadata::EMPTY; 4];
        let mut tx_meta = [PacketMetadata::EMPTY; 4];
        let mut rx = [0u8; 600];
        let mut tx = [0u8; 600];
        let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
        sock.bind(dhcp::SERVER_PORT).expect("bind dhcp");
        let mut server = DhcpServer::new(AP_IP);
        let mut req = [0u8; 600];
        let mut reply = [0u8; 400];
        loop {
            let Ok((n, _)) = sock.recv_from(&mut req).await else {
                continue;
            };
            if let Some(len) = server.handle(&req[..n], &mut reply) {
                let to = (embassy_net::Ipv4Address::BROADCAST, dhcp::CLIENT_PORT);
                let _ = sock.send_to(&reply[..len], to).await;
            }
        }
    }
}
