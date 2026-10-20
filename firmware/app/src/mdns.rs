//! mDNS / DNS-SD responder (edge-mdns), so the device can be found as
//! `<hostname>.local` and browsed as `_http._tcp` / `_thermostat._tcp`.

use core::net::{Ipv4Addr, Ipv6Addr};

use edge_mdns::buf::VecBufAccess;
use edge_mdns::domain::base::Ttl;
use edge_mdns::domain::rdata::AllRecordData;
use edge_mdns::host::{Host, Service, ServiceAnswers};
use edge_mdns::io::{self, IPV4_DEFAULT_SOCKET};
use edge_mdns::{
    HostAnswer, HostAnswers, HostAnswersMdnsHandler, MdnsError, NoHostAnswers, RecordDataChain,
};
use edge_nal::UdpSplit;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use heapless::String;

use crate::device::{Handle, VERSION};

const MDNS_PACKET: usize = 1500;

/// edge-mdns' `ServiceAnswers` also emit the host's address records; drop them
/// so the address is announced once, by the `Host` entry.
struct WithoutAddress<T>(T);

impl<T: HostAnswers> HostAnswers for WithoutAddress<T> {
    fn visit<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(HostAnswer) -> Result<(), E>,
        E: From<MdnsError>,
    {
        self.0.visit(|answer| match answer.data() {
            RecordDataChain::Next(AllRecordData::A(_) | AllRecordData::Aaaa(_)) => Ok(()),
            _ => f(answer),
        })
    }
}

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>, dev: Handle) {
    let hostname: String<32> =
        dev.with(|d| String::try_from(d.settings.hostname.as_str()).unwrap_or_default());
    let mut mac_str = String::<17>::new();
    let embassy_net::HardwareAddress::Ethernet(addr) = stack.hardware_address();
    let mac = addr.as_bytes();
    let _ = core::fmt::write(
        &mut mac_str,
        format_args!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ),
    );

    let buffers = UdpBuffers::<1, MDNS_PACKET, MDNS_PACKET, 2>::new();
    let udp = Udp::new(stack, &buffers);
    let recv_buf = VecBufAccess::<NoopRawMutex, MDNS_PACKET>::new();
    let send_buf = VecBufAccess::<NoopRawMutex, MDNS_PACKET>::new();

    loop {
        stack.wait_config_up().await;
        let Some(ip) = stack.config_v4().map(|c| c.address.address()) else {
            continue;
        };
        let ip: Ipv4Addr = ip.into();

        let host = Host {
            hostname: &hostname,
            ipv4: ip,
            ipv6: Ipv6Addr::UNSPECIFIED,
            ttl: Ttl::from_secs(120),
        };
        let txt_http = [("path", "/")];
        let txt_thermostat = [
            ("version", VERSION),
            ("mac", mac_str.as_str()),
            ("api", "/api/status"),
        ];
        let http = Service {
            name: &hostname,
            priority: 0,
            weight: 0,
            service: "_http",
            protocol: "_tcp",
            port: 80,
            service_subtypes: &[],
            txt_kvs: &txt_http,
        };
        let thermostat = Service {
            name: &hostname,
            priority: 0,
            weight: 0,
            service: "_thermostat",
            protocol: "_tcp",
            port: 80,
            service_subtypes: &[],
            txt_kvs: &txt_thermostat,
        };
        let answers = NoHostAnswers
            .chain(&host)
            .chain(WithoutAddress(ServiceAnswers::new(&host, &http)))
            .chain(WithoutAddress(ServiceAnswers::new(&host, &thermostat)));

        let mut socket =
            match io::bind(&udp, IPV4_DEFAULT_SOCKET, Some(Ipv4Addr::UNSPECIFIED), None).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("mdns: bind failed: {e:?}");
                    Timer::after(Duration::from_secs(5)).await;
                    continue;
                }
            };
        let (recv, send) = socket.split();
        let signal = Signal::<NoopRawMutex, ()>::new();
        let mdns = io::Mdns::<_, _, _, _, _, NoopRawMutex>::new(
            Some(Ipv4Addr::UNSPECIFIED),
            None,
            recv,
            send,
            &recv_buf,
            &send_buf,
            Rng::new(),
            &signal,
        );
        log::info!("mdns: http://{hostname}.local/ ({ip})");

        match select(
            mdns.run(HostAnswersMdnsHandler::new(answers)),
            stack.wait_config_down(),
        )
        .await
        {
            Either::First(Err(e)) => {
                log::warn!("mdns: {e:?}");
                Timer::after(Duration::from_secs(1)).await;
            }
            Either::First(Ok(())) => {}
            // Address lost (e.g. WiFi dropped): wait for the new one.
            Either::Second(()) => {}
        }
    }
}
