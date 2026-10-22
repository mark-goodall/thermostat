//! Multi-zone heating: thermostats that share one heater ("heater id").
//!
//! Every thermostat (zone) decides on its own whether it wants heat. The heater
//! runs while any zone in the group requests heat. Short-cycle protection is a
//! minimum heater *on* time: a zone requesting heat always gets it at once, and
//! only the last zone switching off has to wait until the heater has run for
//! the minimum time since it started. Zones tell each other their state with
//! small signed messages (`POST /api/zone/event`):
//!
//! ```text
//! v=1&heater=boiler&zone=kitchen&heat=1&on=1792613236&ts=1792613300&sig=<hex>
//! ```
//!
//! `on` is the Unix time the heater last started, as far as the sender knows
//! (0 = unknown); zones keep the latest, so all of them time the same run.
//! `ts` is the sender's clock (replay protection when both sides have NTP
//! time). `sig` is HMAC-SHA256 over everything before `&sig=`, keyed with the
//! group key, and required when a key is configured.

use core::fmt::Write as _;

use heapless::{String, Vec};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::codec::hex;
use crate::http::{form_value, url_decode};

pub const MAX_PEERS: usize = 8;
/// A peer not heard from for this long no longer counts as requesting heat.
pub const PEER_TIMEOUT_MS: u64 = 90 * 1000;
/// Peers re-announce their state at least this often.
pub const KEEPALIVE_MS: u64 = 30 * 1000;
/// Accepted clock difference for signed messages when both sides have NTP time.
pub const MAX_CLOCK_SKEW_S: i64 = 5 * 60;

pub type Name = String<32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMessage {
    pub heater: Name,
    pub zone: Name,
    pub heat: bool,
    /// When the heater last started, as known to the sender (Unix seconds).
    pub heater_on_since: Option<i64>,
    /// Sender's clock (Unix seconds), if synchronised.
    pub ts: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneError {
    Malformed,
    /// Not for our heater group.
    OtherGroup,
    /// Missing or wrong signature.
    BadSignature,
    /// Timestamp too far from our clock (possible replay).
    Stale,
    /// The message claims to come from this device.
    FromSelf,
}

impl ZoneError {
    pub fn as_str(&self) -> &'static str {
        match self {
            ZoneError::Malformed => "malformed zone message",
            ZoneError::OtherGroup => "different heater id",
            ZoneError::BadSignature => "bad or missing signature",
            ZoneError::Stale => "timestamp outside the allowed window",
            ZoneError::FromSelf => "message from this zone",
        }
    }
}

fn sign(key: &str, payload: &str) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes any key length");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Percent-encode characters that are not safe in form values.
fn push_encoded<const N: usize>(out: &mut String<N>, s: &str) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
            let _ = out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

impl ZoneMessage {
    /// Encode as a form body, signed when `key` is non-empty.
    pub fn encode(&self, key: &str) -> String<256> {
        let mut s = String::new();
        s.push_str("v=1&heater=").ok();
        push_encoded(&mut s, &self.heater);
        s.push_str("&zone=").ok();
        push_encoded(&mut s, &self.zone);
        let _ = write!(
            s,
            "&heat={}&on={}&ts={}",
            self.heat as u8,
            self.heater_on_since.unwrap_or(0),
            self.ts.unwrap_or(0)
        );
        if !key.is_empty() {
            let sig = hex::encode::<64>(&sign(key, &s));
            let _ = write!(s, "&sig={sig}");
        }
        s
    }

    /// Decode and authenticate a message for our group.
    pub fn decode(
        body: &str,
        our_heater: &str,
        our_zone: &str,
        key: &str,
        now_unix: Option<i64>,
    ) -> Result<Self, ZoneError> {
        let (payload, sig) = match body.rsplit_once("&sig=") {
            Some((p, s)) => (p, Some(s.trim())),
            None => (body.trim(), None),
        };
        if form_value(payload, "v") != Some("1") {
            return Err(ZoneError::Malformed);
        }
        let text = |k| {
            url_decode::<32>(form_value(payload, k).ok_or(ZoneError::Malformed)?)
                .ok_or(ZoneError::Malformed)
        };
        let num = |k| -> Result<i64, ZoneError> {
            form_value(payload, k)
                .and_then(|v| v.parse().ok())
                .ok_or(ZoneError::Malformed)
        };
        let msg = ZoneMessage {
            heater: text("heater")?,
            zone: text("zone")?,
            heat: match form_value(payload, "heat") {
                Some("1") => true,
                Some("0") => false,
                _ => return Err(ZoneError::Malformed),
            },
            heater_on_since: Some(num("on")?).filter(|t| *t > 0),
            ts: Some(num("ts")?).filter(|t| *t > 0),
        };
        if msg.zone.is_empty() {
            return Err(ZoneError::Malformed);
        }
        if our_heater.is_empty() || msg.heater.as_str() != our_heater {
            return Err(ZoneError::OtherGroup);
        }
        if msg.zone.as_str() == our_zone {
            return Err(ZoneError::FromSelf);
        }
        if !key.is_empty() {
            let mut got = [0u8; 32];
            let ok = sig.is_some_and(|s| hex::decode(s, &mut got).is_some())
                && crate::auth::constant_time_eq(&got, &sign(key, payload));
            if !ok {
                return Err(ZoneError::BadSignature);
            }
            if let (Some(now), Some(ts)) = (now_unix, msg.ts)
                && (now - ts).abs() > MAX_CLOCK_SKEW_S
            {
                return Err(ZoneError::Stale);
            }
        }
        Ok(msg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub zone: Name,
    /// Address and port learned through mDNS (where to send our events).
    pub addr: Option<([u8; 4], u16)>,
    pub heat: bool,
    /// When we last received a message from this zone (monotonic ms).
    pub heard_ms: Option<u64>,
    /// When mDNS last reported this zone.
    pub discovered_ms: Option<u64>,
}

/// What this device knows about the other zones of its heater group.
#[derive(Debug, Default)]
pub struct Group {
    peers: Vec<Peer, MAX_PEERS>,
    /// When the heater last started, latest known in the group (Unix seconds).
    pub heater_on_since: Option<i64>,
    pub rejected: u32,
}

impl Group {
    pub const fn new() -> Self {
        Self {
            peers: Vec::new(),
            heater_on_since: None,
            rejected: 0,
        }
    }

    fn peer_mut(&mut self, zone: &str, now_ms: u64) -> Option<&mut Peer> {
        if let Some(i) = self.peers.iter().position(|p| p.zone.as_str() == zone) {
            return Some(&mut self.peers[i]);
        }
        let fresh = Peer {
            zone: crate::settings::str32(zone),
            addr: None,
            heat: false,
            heard_ms: None,
            discovered_ms: None,
        };
        if self.peers.is_full() {
            // Replace the peer heard from least recently.
            let i = (0..self.peers.len()).min_by_key(|&i| {
                self.peers[i]
                    .heard_ms
                    .max(self.peers[i].discovered_ms)
                    .unwrap_or(0)
            })?;
            if self.peers[i]
                .heard_ms
                .max(self.peers[i].discovered_ms)
                .unwrap_or(0)
                + PEER_TIMEOUT_MS
                > now_ms
            {
                return None; // everyone is active; ignore newcomers
            }
            self.peers[i] = fresh;
            return Some(&mut self.peers[i]);
        }
        self.peers.push(fresh).ok()?;
        self.peers.last_mut()
    }

    /// Apply a message from another zone.
    pub fn receive(&mut self, msg: &ZoneMessage, now_ms: u64) {
        if let Some(p) = self.peer_mut(&msg.zone, now_ms) {
            p.heat = msg.heat;
            p.heard_ms = Some(now_ms);
        }
        self.note_heater_on(msg.heater_on_since);
    }

    /// Record a peer found through mDNS.
    pub fn discovered(&mut self, zone: &str, addr: [u8; 4], port: u16, now_ms: u64) {
        if let Some(p) = self.peer_mut(zone, now_ms) {
            p.addr = Some((addr, port));
            p.discovered_ms = Some(now_ms);
        }
    }

    /// Keep the most recent heater start time.
    pub fn note_heater_on(&mut self, unix: Option<i64>) {
        if let Some(t) = unix {
            self.heater_on_since = Some(self.heater_on_since.map_or(t, |l| l.max(t)));
        }
    }

    fn fresh(p: &Peer, now_ms: u64) -> bool {
        p.heard_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < PEER_TIMEOUT_MS)
    }

    /// Whether any other zone currently requests heat.
    pub fn others_heating(&self, now_ms: u64) -> bool {
        self.peers.iter().any(|p| p.heat && Self::fresh(p, now_ms))
    }

    /// Peers we can send events to.
    pub fn targets(&self, now_ms: u64) -> impl Iterator<Item = ([u8; 4], u16)> + '_ {
        self.peers.iter().filter_map(move |p| {
            let recent = p
                .discovered_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < 10 * 60 * 1000);
            p.addr.filter(|_| recent || Self::fresh(p, now_ms))
        })
    }

    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }

    /// Whether `peer` counts as active (heard from recently).
    pub fn is_fresh(&self, peer: &Peer, now_ms: u64) -> bool {
        Self::fresh(peer, now_ms)
    }

    pub fn clear(&mut self) {
        self.peers.clear();
        self.heater_on_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::str32;

    fn msg(zone: &str, heat: bool, last: Option<i64>, ts: Option<i64>) -> ZoneMessage {
        ZoneMessage {
            heater: str32("boiler"),
            zone: str32(zone),
            heat,
            heater_on_since: last,
            ts,
        }
    }

    #[test]
    fn encode_decode_signed() {
        let m = msg("Kitchen 1", true, Some(1_792_600_000), Some(1_792_613_300));
        let body = m.encode("s3cret");
        assert!(
            body.starts_with(
                "v=1&heater=boiler&zone=Kitchen%201&heat=1&on=1792600000&ts=1792613300&sig="
            ),
            "{body}"
        );
        let got =
            ZoneMessage::decode(&body, "boiler", "bedroom", "s3cret", Some(1_792_613_310)).unwrap();
        assert_eq!(got, m);
        // Unsigned groups work without a key.
        let plain = msg("hall", false, None, None).encode("");
        assert_eq!(
            plain.as_str(),
            "v=1&heater=boiler&zone=hall&heat=0&on=0&ts=0"
        );
        assert_eq!(
            ZoneMessage::decode(&plain, "boiler", "bedroom", "", None)
                .unwrap()
                .heater_on_since,
            None
        );
    }

    #[test]
    fn rejects_bad_messages() {
        let body = msg("kitchen", true, None, Some(1_000_000)).encode("key");
        let d =
            |b: &str, heater, zone, key, now| ZoneMessage::decode(b, heater, zone, key, now).err();
        assert_eq!(
            d(&body, "boiler", "bedroom", "other", None),
            Some(ZoneError::BadSignature)
        );
        assert_eq!(
            d(
                &body.replace("heat=1", "heat=0"),
                "boiler",
                "bedroom",
                "key",
                None
            ),
            Some(ZoneError::BadSignature)
        );
        let unsigned = msg("kitchen", true, None, None).encode("");
        assert_eq!(
            d(&unsigned, "boiler", "bedroom", "key", None),
            Some(ZoneError::BadSignature)
        );
        assert_eq!(
            d(&body, "boiler", "bedroom", "key", Some(1_000_000 + 301)),
            Some(ZoneError::Stale)
        );
        assert_eq!(
            d(&body, "garage", "bedroom", "key", None),
            Some(ZoneError::OtherGroup)
        );
        assert_eq!(
            d(&body, "", "bedroom", "key", None),
            Some(ZoneError::OtherGroup)
        );
        assert_eq!(
            d(&body, "boiler", "kitchen", "key", None),
            Some(ZoneError::FromSelf)
        );
        assert_eq!(
            d("v=2&heater=boiler", "boiler", "x", "", None),
            Some(ZoneError::Malformed)
        );
        assert_eq!(
            d(
                "v=1&heater=boiler&zone=a&heat=2&on=0&ts=0",
                "boiler",
                "x",
                "",
                None
            ),
            Some(ZoneError::Malformed)
        );
    }

    #[test]
    fn group_tracks_peers_and_latest_switch() {
        let mut g = Group::new();
        assert!(!g.others_heating(0));
        g.discovered("kitchen", [10, 0, 2, 16], 80, 0);
        assert_eq!(
            g.targets(0).collect::<std::vec::Vec<_>>(),
            [([10, 0, 2, 16], 80)]
        );
        g.receive(&msg("kitchen", true, Some(500), None), 1_000);
        assert!(g.others_heating(1_000));
        g.receive(&msg("hall", false, Some(400), None), 1_000);
        assert_eq!(g.heater_on_since, Some(500), "latest wins");
        // Silence counts as not heating.
        assert!(!g.others_heating(1_000 + PEER_TIMEOUT_MS));
        g.receive(&msg("kitchen", false, None, None), 2_000);
        assert!(!g.others_heating(2_000));
        assert_eq!(g.peers().len(), 2);
    }

    #[test]
    fn group_is_bounded() {
        let mut g = Group::new();
        for i in 0..MAX_PEERS {
            g.receive(&msg(&std::format!("z{i}"), false, None, None), 0);
        }
        // All active: a newcomer is ignored rather than evicting someone.
        g.receive(&msg("late", true, None, None), 1_000);
        assert!(!g.peers().iter().any(|p| p.zone == "late"));
        // Once others go quiet, newcomers replace them.
        g.receive(&msg("late", true, None, None), 1_000 + PEER_TIMEOUT_MS);
        assert!(g.peers().iter().any(|p| p.zone == "late"));
    }
}
