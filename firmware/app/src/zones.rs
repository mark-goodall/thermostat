//! Heater-group messaging: tell the other zones (found via mDNS) whether this
//! zone requests heat, immediately on change and as a periodic keepalive.

use core::fmt::Write as _;

use embassy_futures::select::select;
use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_io_async::Write;
use heapless::{String, Vec};
use thermostat_core::zones::{KEEPALIVE_MS, MAX_PEERS, ZoneMessage};

use crate::device::Handle;

/// Signalled when this zone's heat request changes, to notify peers at once.
pub static KICK: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const TIMEOUT: Duration = Duration::from_secs(3);

async fn post(
    stack: Stack<'static>,
    (addr, port): ([u8; 4], u16),
    body: &str,
    buf: &mut [u8; 512],
) -> Option<String<256>> {
    let mut rx = [0u8; 512];
    let mut tx = [0u8; 512];
    let mut sock = TcpSocket::new(stack, &mut rx, &mut tx);
    sock.set_timeout(Some(TIMEOUT));
    let ip = embassy_net::Ipv4Address::new(addr[0], addr[1], addr[2], addr[3]);
    with_timeout(TIMEOUT, sock.connect((ip, port)))
        .await
        .ok()?
        .ok()?;
    let mut head = String::<160>::new();
    let _ = write!(
        head,
        "POST /api/zone/event HTTP/1.1\r\nHost: {}.{}.{}.{}\r\nContent-Type: application/x-www-form-urlencoded\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        addr[0],
        addr[1],
        addr[2],
        addr[3],
        body.len()
    );
    sock.write_all(head.as_bytes()).await.ok()?;
    sock.write_all(body.as_bytes()).await.ok()?;
    sock.flush().await.ok()?;
    let mut len = 0;
    while len < buf.len() {
        match sock.read(&mut buf[len..]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n,
        }
    }
    sock.close();
    let text = core::str::from_utf8(&buf[..len]).ok()?;
    let (head, reply) = text.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200")
        .then(|| String::try_from(reply.trim()).ok())
        .flatten()
}

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>, dev: Handle) {
    let mut buf = [0u8; 512];
    loop {
        select(
            KICK.wait(),
            Timer::after(Duration::from_millis(KEEPALIVE_MS)),
        )
        .await;
        let now = Instant::now();
        let prepared = dev.with(|d| {
            let s = &d.settings;
            if s.heater_id.is_empty() {
                return None;
            }
            let msg = ZoneMessage {
                heater: s.heater_id.clone(),
                zone: s.hostname.clone(),
                heat: d.thermostat.heating(),
                heater_on_since: d.group.heater_on_since,
                ts: d.clock.unix_s(now.as_micros()),
            };
            let targets: Vec<([u8; 4], u16), MAX_PEERS> =
                d.group.targets(now.as_millis()).collect();
            Some((
                msg.encode(&s.group_key),
                targets,
                s.heater_id.clone(),
                s.hostname.clone(),
                s.group_key.clone(),
            ))
        });
        let Some((body, targets, heater, zone, key)) = prepared else {
            continue;
        };
        for target in targets {
            let Some(reply) = post(stack, target, &body, &mut buf).await else {
                log::warn!("zones: no reply from {:?}", target);
                continue;
            };
            dev.with(|d| {
                let now_unix = d.clock.unix_s(Instant::now().as_micros());
                match ZoneMessage::decode(&reply, &heater, &zone, &key, now_unix) {
                    Ok(m) => d.group.receive(&m, Instant::now().as_millis()),
                    Err(e) => log::warn!("zones: reply from {:?} rejected: {}", target, e.as_str()),
                }
            });
        }
    }
}
