//! mDNS / DNS-SD (edge-mdns): the device answers as `<hostname>.local` and
//! advertises `_http._tcp` / `_thermostat._tcp`; when it belongs to a heater
//! group it also browses `_thermostat._tcp` to find the other zones (TXT
//! `heater=<id>`).

use core::net::{Ipv4Addr, Ipv6Addr};

use core::fmt::Write as _;
use edge_mdns::buf::VecBufAccess;
use edge_mdns::domain::base::Ttl;
use edge_mdns::domain::rdata::AllRecordData;
use edge_mdns::host::{Host, Service, ServiceAnswers};
use edge_mdns::io::{self, IPV4_DEFAULT_SOCKET};

use edge_mdns::domain::base::Question;
use edge_mdns::domain::base::iana::{Class, Rtype};
use edge_mdns::{
    HostAnswer, HostAnswers, HostAnswersMdnsHandler, HostQuestion, HostQuestions, MdnsError,
    NameSlice, NoHandler, NoHostAnswers, PeerAnswer, PeerAnswers, PeerAnswersMdnsHandler,
    RecordDataChain,
};
use edge_nal::UdpSplit;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_futures::select::{Either3, select3};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::rng::Rng;
use heapless::String;
use heapless::Vec;

use crate::device::{Handle, VERSION};

const MDNS_PACKET: usize = 1500;
const ADDRESS_POLL: Duration = Duration::from_secs(2);

fn current_ip(stack: Stack<'_>) -> Option<Ipv4Addr> {
    stack.config_v4().map(|c| c.address.address())
}

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

const BROWSE_EVERY: Duration = Duration::from_secs(60);

/// PTR question for `_thermostat._tcp.local`.
struct ThermostatQuery;

impl HostQuestions for ThermostatQuery {
    fn visit<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(HostQuestion) -> Result<(), E>,
        E: From<MdnsError>,
    {
        f(Question::new(
            NameSlice::new(&["_thermostat", "_tcp", "local"]),
            Rtype::PTR,
            Class::IN,
        ))
    }
}

type Text = String<64>;

fn name_of(n: impl core::fmt::Display) -> Text {
    let mut s = Text::new();
    let _ = write!(s, "{n}");
    while s.ends_with('.') {
        s.pop();
    }
    s
}

/// Collects `_thermostat._tcp` answers and records zones of our heater group.
struct Discovery {
    dev: Handle,
    own: String<32>,
    heater: String<32>,
}

impl PeerAnswers for Discovery {
    fn answers<'a, T, A>(&self, answers: T, additional: A) -> Result<(), MdnsError>
    where
        T: IntoIterator<Item = Result<PeerAnswer<'a>, MdnsError>> + Clone + 'a,
        A: IntoIterator<Item = Result<PeerAnswer<'a>, MdnsError>> + Clone + 'a,
    {
        if self.heater.is_empty() {
            return Ok(());
        }
        let mut srv: Vec<(Text, Text, u16), 4> = Vec::new(); // instance -> target, port
        let mut grouped: Vec<Text, 4> = Vec::new(); // instances with our heater id
        let mut addrs: Vec<(Text, [u8; 4]), 4> = Vec::new(); // host -> address
        for rec in answers.into_iter().chain(additional).flatten() {
            let owner = name_of(rec.owner());
            match rec.data() {
                AllRecordData::Srv(r) => {
                    let _ = srv.push((owner, name_of(r.target()), r.port()));
                }
                AllRecordData::Txt(t) => {
                    let ours = t.iter().any(|kv| {
                        kv.strip_prefix(b"heater=")
                            .is_some_and(|v| v == self.heater.as_bytes())
                    });
                    if ours {
                        let _ = grouped.push(owner);
                    }
                }
                AllRecordData::A(a) => {
                    let _ = addrs.push((owner, a.addr().octets()));
                }
                _ => {}
            }
        }
        let now = Instant::now().as_millis();
        for (instance, target, port) in &srv {
            let Some(zone) = instance.strip_suffix("._thermostat._tcp.local") else {
                continue;
            };
            if zone.eq_ignore_ascii_case(&self.own)
                || !grouped.iter().any(|g| g.eq_ignore_ascii_case(instance))
            {
                continue;
            }
            if let Some((_, addr)) = addrs.iter().find(|(h, _)| h.eq_ignore_ascii_case(target)) {
                self.dev
                    .with(|d| d.group.discovered(zone, *addr, *port, now));
            }
        }
        Ok(())
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
        // Poll rather than `stack.wait_config_up()`: embassy-net keeps a single
        // waker for config changes, and two tasks waiting on it (link_monitor
        // already does) wake each other forever, pinning the CPU.
        let Some(ip) = current_ip(stack) else {
            Timer::after(ADDRESS_POLL).await;
            continue;
        };

        let host = Host {
            hostname: &hostname,
            ipv4: ip,
            ipv6: Ipv6Addr::UNSPECIFIED,
            ttl: Ttl::from_secs(120),
        };
        let txt_http = [("path", "/")];
        let heater_id: String<32> = dev.with(|d| d.settings.heater_id.clone());
        let mut txt_thermostat: Vec<(&str, &str), 4> = Vec::new();
        let _ = txt_thermostat.push(("version", VERSION));
        let _ = txt_thermostat.push(("mac", mac_str.as_str()));
        let _ = txt_thermostat.push(("api", "/api/status"));
        if !heater_id.is_empty() {
            let _ = txt_thermostat.push(("heater", heater_id.as_str()));
        }
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

        // Re-run (re-announcing) when the address or the heater id changes.
        let address_changed = async {
            while current_ip(stack) == Some(ip) && dev.with(|d| d.settings.heater_id == heater_id) {
                Timer::after(ADDRESS_POLL).await;
            }
        };
        let browse = async {
            Timer::after(Duration::from_secs(2)).await;
            loop {
                if !heater_id.is_empty()
                    && let Err(e) = mdns.query(|buf| ThermostatQuery.query(0, buf)).await
                {
                    log::warn!("mdns: query failed: {e:?}");
                }
                Timer::after(BROWSE_EVERY).await;
            }
        };
        let discovery = Discovery {
            dev,
            own: hostname.clone(),
            heater: heater_id.clone(),
        };
        let handler = NoHandler
            .chain(PeerAnswersMdnsHandler::new(discovery))
            .chain(HostAnswersMdnsHandler::new(answers));
        match select3(mdns.run(handler), address_changed, browse).await {
            Either3::First(Err(e)) => {
                log::warn!("mdns: {e:?}");
                Timer::after(Duration::from_secs(1)).await;
            }
            Either3::First(Ok(())) | Either3::Third(()) => {}
            // Address or heater id changed (e.g. WiFi dropped): re-announce.
            Either3::Second(()) => log::info!("mdns: address or heater id changed"),
        }
    }
}
