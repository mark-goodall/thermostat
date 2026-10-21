//! Keep the wall clock in sync with an NTP server (SNTP).

use core::net::Ipv4Addr;

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use heapless::String;
use thermostat_core::sntp;

use crate::device::Handle;

const RESYNC: Duration = Duration::from_secs(60 * 60);
const CHECK_EVERY: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Reported through `Debug` in log messages.
#[derive(Debug)]
#[allow(dead_code)]
enum SyncError {
    BadServer,
    Dns,
    Io,
    Timeout,
    Reply(sntp::SntpError),
}

async fn resolve(stack: Stack<'static>, host: &str) -> Result<Ipv4Addr, SyncError> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    let addrs = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| SyncError::Dns)?;
    addrs
        .iter()
        .find_map(|a| match a {
            embassy_net::IpAddress::Ipv4(v4) => Some(*v4),
            #[allow(unreachable_patterns)]
            _ => None,
        })
        .ok_or(SyncError::Dns)
}

/// One request/reply exchange; returns the sample and the monotonic receive time.
async fn exchange(
    stack: Stack<'static>,
    dev: Handle,
    server: &str,
) -> Result<(sntp::Sample, u64), SyncError> {
    let (host, port) = sntp::split_server(server).ok_or(SyncError::BadServer)?;
    let ip = resolve(stack, host).await?;

    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx = [0u8; 128];
    let mut tx = [0u8; 128];
    let mut sock = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    sock.bind(0).map_err(|_| SyncError::Io)?;

    // A random transmit timestamp identifies our request.
    let transmit = dev.with(|d| (d.rng.random() as u64) << 32 | d.rng.random() as u64);
    let t1 = Instant::now().as_micros();
    sock.send_to(&sntp::request(transmit), (ip, port))
        .await
        .map_err(|_| SyncError::Io)?;
    let mut buf = [0u8; 96];
    loop {
        let (n, from) = with_timeout(REPLY_TIMEOUT, sock.recv_from(&mut buf))
            .await
            .map_err(|_| SyncError::Timeout)?
            .map_err(|_| SyncError::Io)?;
        let t4 = Instant::now().as_micros();
        if from.endpoint.addr != ip.into() {
            continue;
        }
        return sntp::parse_reply(&buf[..n], transmit, t1, t4)
            .map(|s| (s, t4))
            .map_err(SyncError::Reply);
    }
}

#[embassy_executor::task]
pub async fn task(stack: Stack<'static>, dev: Handle) {
    let mut last_server: Option<String<48>> = None;
    let mut last_sync: Option<Instant> = None;
    let mut retry_at = Instant::now();
    let mut backoff = Duration::from_secs(10);
    loop {
        // Poll the link state: embassy-net's config waker only supports one waiter.
        if stack.config_v4().is_some() {
            let server = dev.with(|d| d.settings.ntp_server.clone());
            let changed = last_server.as_ref() != Some(&server);
            let due = last_sync.is_none_or(|t| t.elapsed() >= RESYNC);
            if (changed || due) && (changed || Instant::now() >= retry_at) {
                match exchange(stack, dev, &server).await {
                    Ok((sample, t4)) => {
                        let first = last_sync.is_none() || changed;
                        let drift = dev.with(|d| {
                            let before = d.clock.unix_us(t4);
                            d.clock.set(sample.unix_us_at_receive, t4);
                            before.map(|b| sample.unix_us_at_receive - b)
                        });
                        match (first, drift) {
                            (false, Some(d)) => {
                                log::info!("ntp: resynced with {server}, corrected {} ms", d / 1000)
                            }
                            _ => log::info!(
                                "ntp: time set from {server} (unix {}, stratum {}, delay {} ms)",
                                sample.unix_us_at_receive / 1_000_000,
                                sample.stratum,
                                sample.delay_us / 1000
                            ),
                        }
                        last_sync = Some(Instant::now());
                        backoff = Duration::from_secs(10);
                    }
                    Err(e) => {
                        log::warn!(
                            "ntp: sync with {server} failed: {e:?}; retry in {} s",
                            backoff.as_secs()
                        );
                        retry_at = Instant::now() + backoff;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
                last_server = Some(server);
            }
        }
        Timer::after(CHECK_EVERY).await;
    }
}
