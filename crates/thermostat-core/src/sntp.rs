//! SNTP (RFC 4330) client message handling.
//!
//! The request carries our own transmit timestamp; the server echoes it as
//! the originate timestamp, which lets us match the reply and compute the
//! clock offset with the usual four-timestamp formula.

pub const PACKET_LEN: usize = 48;
pub const PORT: u16 = 123;

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_DELTA: i64 = 2_208_988_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SntpError {
    TooShort,
    NotServerReply,
    /// Stratum 0: kiss-of-death or unsynchronised server.
    Unsynchronised,
    /// The reply does not answer our request.
    WrongOriginate,
}

/// NTP 64-bit timestamp to Unix microseconds. Seconds below 2^31 are taken
/// to be in era 1 (after 2036-02-07).
pub fn ntp_to_unix_us(ts: u64) -> i64 {
    let secs = (ts >> 32) as i64;
    let secs = if secs < 0x8000_0000 {
        secs + (1 << 32)
    } else {
        secs
    };
    let frac_us = ((ts & 0xFFFF_FFFF) * 1_000_000) >> 32;
    (secs - NTP_UNIX_DELTA) * 1_000_000 + frac_us as i64
}

pub fn unix_us_to_ntp(unix_us: i64) -> u64 {
    let secs = unix_us.div_euclid(1_000_000) + NTP_UNIX_DELTA;
    let frac = ((unix_us.rem_euclid(1_000_000) as u64) << 32) / 1_000_000;
    ((secs as u64 & 0xFFFF_FFFF) << 32) | frac
}

/// Build a client request. `transmit` is any value unique to this request
/// (our best guess of the time, or a counter); it identifies the reply.
pub fn request(transmit: u64) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 3; // LI 0, version 4, mode 3 (client)
    p[40..48].copy_from_slice(&transmit.to_be_bytes());
    p
}

fn ts(p: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(p[at..at + 8].try_into().unwrap())
}

/// Result of one exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Unix time (µs) corresponding to the moment the reply was received.
    pub unix_us_at_receive: i64,
    /// Round-trip delay in microseconds.
    pub delay_us: i64,
    pub stratum: u8,
}

/// Interpret a server reply.
///
/// `sent_transmit` is the value passed to [`request`]; `t1_us`/`t4_us` are
/// our local clock (any timebase, µs) when the request left and the reply
/// arrived. Only their difference matters, so a monotonic clock works.
pub fn parse_reply(
    reply: &[u8],
    sent_transmit: u64,
    t1_us: u64,
    t4_us: u64,
) -> Result<Sample, SntpError> {
    if reply.len() < PACKET_LEN {
        return Err(SntpError::TooShort);
    }
    let mode = reply[0] & 7;
    if mode != 4 && mode != 5 {
        return Err(SntpError::NotServerReply);
    }
    let stratum = reply[1];
    if stratum == 0 || stratum > 15 {
        return Err(SntpError::Unsynchronised);
    }
    if ts(reply, 24) != sent_transmit {
        return Err(SntpError::WrongOriginate);
    }
    let t2 = ntp_to_unix_us(ts(reply, 32)); // server receive
    let t3 = ntp_to_unix_us(ts(reply, 40)); // server transmit
    let rtt = t4_us.saturating_sub(t1_us) as i64;
    let delay_us = (rtt - (t3 - t2)).max(0);
    Ok(Sample {
        unix_us_at_receive: t3 + delay_us / 2,
        delay_us,
        stratum,
    })
}

/// Build a server reply (for tests and the emulator test harness).
pub fn server_reply(request: &[u8], unix_us: i64) -> Option<[u8; PACKET_LEN]> {
    if request.len() < PACKET_LEN || request[0] & 7 != 3 {
        return None;
    }
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 4; // version 4, mode 4 (server)
    p[1] = 1; // stratum 1
    p[24..32].copy_from_slice(&request[40..48]); // originate = client transmit
    let now = unix_us_to_ntp(unix_us);
    p[32..40].copy_from_slice(&now.to_be_bytes());
    p[40..48].copy_from_slice(&now.to_be_bytes());
    Some(p)
}

/// Split `host[:port]`.
pub fn split_server(server: &str) -> Option<(&str, u16)> {
    match server.rsplit_once(':') {
        Some((host, port)) => Some((host, port.parse().ok().filter(|p| *p != 0)?)),
        None => Some((server, PORT)),
    }
    .filter(|(h, _)| {
        !h.is_empty()
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_conversion() {
        // 2026-10-21T00:00:00Z
        let unix = 1_792_454_400i64 * 1_000_000 + 250_000;
        let ntp = unix_us_to_ntp(unix);
        assert_eq!(ntp >> 32, 1_792_454_400 + 2_208_988_800);
        assert!((ntp_to_unix_us(ntp) - unix).abs() <= 1);
        // After the 2036 rollover the seconds field wraps to small values.
        let unix_2040 = 2_208_988_800i64 * 1_000_000; // 2040-01-01
        let ntp = unix_us_to_ntp(unix_2040);
        assert!((ntp >> 32) < 0x8000_0000);
        assert_eq!(ntp_to_unix_us(ntp), unix_2040);
    }

    #[test]
    fn exchange() {
        let req = request(0x1122_3344_5566_7788);
        assert_eq!(req[0], 0x23);
        let server_time = 1_900_000_000i64 * 1_000_000;
        let reply = server_reply(&req, server_time).unwrap();
        // 40 ms round trip measured on our side.
        let s = parse_reply(&reply, 0x1122_3344_5566_7788, 1_000_000, 1_040_000).unwrap();
        assert_eq!(s.delay_us, 40_000);
        assert_eq!(s.unix_us_at_receive, server_time + 20_000);
        assert_eq!(s.stratum, 1);
    }

    #[test]
    fn rejects_bad_replies() {
        let req = request(42);
        let mut reply = server_reply(&req, 0).unwrap();
        assert_eq!(
            parse_reply(&reply[..40], 42, 0, 0),
            Err(SntpError::TooShort)
        );
        assert_eq!(
            parse_reply(&reply, 43, 0, 0),
            Err(SntpError::WrongOriginate)
        );
        reply[1] = 0;
        assert_eq!(
            parse_reply(&reply, 42, 0, 0),
            Err(SntpError::Unsynchronised)
        );
        assert_eq!(parse_reply(&req, 42, 0, 0), Err(SntpError::NotServerReply));
    }

    #[test]
    fn server_strings() {
        assert_eq!(split_server("pool.ntp.org"), Some(("pool.ntp.org", 123)));
        assert_eq!(split_server("10.0.2.2:12345"), Some(("10.0.2.2", 12345)));
        assert_eq!(split_server("host:0"), None);
        assert_eq!(split_server("host:x"), None);
        assert_eq!(split_server(""), None);
        assert_eq!(split_server("bad host"), None);
    }
}
