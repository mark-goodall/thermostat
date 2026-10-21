//! Persistent device settings and their binary encoding.
//!
//! Encoding is a list of `tag (u8) | len (u16 LE) | value` fields. Unknown
//! tags are skipped on decode, so firmware versions can add settings without
//! breaking older or newer images sharing the same partition.

use heapless::{String, Vec};

use crate::auth::PasswordHash;

/// Partition type/subtype of the settings area in the partition table.
pub const SETTINGS_TYPE: u8 = 0x01; // data
pub const SETTINGS_SUBTYPE: u8 = 0x40; // custom: thermostat settings
pub const SETTINGS_LABEL: &str = "settings";

/// Upper bound of an encoded [`Settings`].
pub const MAX_ENCODED: usize = 512;

pub type Str32 = String<32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub hostname: Str32,
    pub wifi_ssid: Str32,
    /// WPA2 pairwise master key derived from the passphrase and SSID
    /// (see [`crate::auth::wpa_psk`]). The passphrase itself is never stored.
    pub wifi_psk: Option<[u8; 32]>,
    pub admin_user: Str32,
    /// `None` until the first-time setup has chosen an admin password.
    pub admin_hash: Option<PasswordHash>,
    /// Target temperature in tenths of a degree Celsius.
    pub setpoint_dc: i16,
    /// Switching hysteresis in tenths of a degree Celsius.
    pub hysteresis_dc: u16,
    /// BLE sensors (addresses in on-air byte order) whose readings drive the thermostat.
    pub temp_sensors: Vec<[u8; 6], MAX_TEMP_SENSORS>,
    /// NTP server, `host[:port]`.
    pub ntp_server: Str48,
    /// POSIX TZ string, e.g. `GMT0BST,M3.5.0/1,M10.5.0` (see `clock::TimeZone`).
    pub timezone: Str48,
}

pub type Str48 = String<48>;

pub fn str48(s: &str) -> Str48 {
    let mut out = String::new();
    for c in s.chars() {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

pub const MAX_TEMP_SENSORS: usize = 4;

impl Default for Settings {
    fn default() -> Self {
        Self {
            hostname: str32("thermostat"),
            wifi_ssid: String::new(),
            wifi_psk: None,
            admin_user: str32("admin"),
            admin_hash: None,
            setpoint_dc: 200,
            hysteresis_dc: 5,
            temp_sensors: Vec::new(),
            ntp_server: str48("pool.ntp.org"),
            timezone: str48("UTC0"),
        }
    }
}

pub fn str32(s: &str) -> Str32 {
    let mut out = String::new();
    for c in s.chars() {
        if out.push(c).is_err() {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadValue,
}

mod tag {
    pub const HOSTNAME: u8 = 1;
    pub const WIFI_SSID: u8 = 2;
    pub const WIFI_PSK: u8 = 3;
    pub const ADMIN_USER: u8 = 4;
    pub const ADMIN_HASH: u8 = 5;
    pub const SETPOINT: u8 = 6;
    pub const HYSTERESIS: u8 = 7;
    pub const TEMP_SENSORS: u8 = 8;
    pub const NTP_SERVER: u8 = 9;
    pub const TIMEZONE: u8 = 10;
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn field(&mut self, tag: u8, value: &[u8]) {
        let b = &mut self.buf[self.pos..];
        b[0] = tag;
        b[1..3].copy_from_slice(&(value.len() as u16).to_le_bytes());
        b[3..3 + value.len()].copy_from_slice(value);
        self.pos += 3 + value.len();
    }
}

impl Settings {
    /// Encode into `buf` (at least [`MAX_ENCODED`] bytes); returns the length.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer { buf, pos: 0 };
        w.field(tag::HOSTNAME, self.hostname.as_bytes());
        w.field(tag::WIFI_SSID, self.wifi_ssid.as_bytes());
        if let Some(psk) = &self.wifi_psk {
            w.field(tag::WIFI_PSK, psk);
        }
        w.field(tag::ADMIN_USER, self.admin_user.as_bytes());
        if let Some(h) = &self.admin_hash {
            w.field(tag::ADMIN_HASH, &h.to_bytes());
        }
        w.field(tag::SETPOINT, &self.setpoint_dc.to_le_bytes());
        w.field(tag::HYSTERESIS, &self.hysteresis_dc.to_le_bytes());
        w.field(tag::NTP_SERVER, self.ntp_server.as_bytes());
        w.field(tag::TIMEZONE, self.timezone.as_bytes());
        if !self.temp_sensors.is_empty() {
            let mut addrs = [0u8; 6 * MAX_TEMP_SENSORS];
            for (i, a) in self.temp_sensors.iter().enumerate() {
                addrs[i * 6..i * 6 + 6].copy_from_slice(a);
            }
            w.field(tag::TEMP_SENSORS, &addrs[..self.temp_sensors.len() * 6]);
        }
        w.pos
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, DecodeError> {
        let mut s = Settings::default();
        while !data.is_empty() {
            if data.len() < 3 {
                return Err(DecodeError::Truncated);
            }
            let t = data[0];
            let len = u16::from_le_bytes([data[1], data[2]]) as usize;
            let v = data.get(3..3 + len).ok_or(DecodeError::Truncated)?;
            data = &data[3 + len..];
            let text = || {
                core::str::from_utf8(v)
                    .map(str32)
                    .map_err(|_| DecodeError::BadValue)
            };
            let text48 = || {
                core::str::from_utf8(v)
                    .map(str48)
                    .map_err(|_| DecodeError::BadValue)
            };
            match t {
                tag::HOSTNAME => s.hostname = text()?,
                tag::WIFI_SSID => s.wifi_ssid = text()?,
                tag::WIFI_PSK => {
                    s.wifi_psk = Some(v.try_into().map_err(|_| DecodeError::BadValue)?)
                }
                tag::ADMIN_USER => s.admin_user = text()?,
                tag::ADMIN_HASH => {
                    s.admin_hash = Some(PasswordHash::from_bytes(v).ok_or(DecodeError::BadValue)?)
                }
                tag::SETPOINT => {
                    s.setpoint_dc =
                        i16::from_le_bytes(v.try_into().map_err(|_| DecodeError::BadValue)?)
                }
                tag::HYSTERESIS => {
                    s.hysteresis_dc =
                        u16::from_le_bytes(v.try_into().map_err(|_| DecodeError::BadValue)?)
                }
                tag::NTP_SERVER => s.ntp_server = text48()?,
                tag::TIMEZONE => s.timezone = text48()?,
                tag::TEMP_SENSORS => {
                    if v.len() % 6 != 0 {
                        return Err(DecodeError::BadValue);
                    }
                    s.temp_sensors.clear();
                    for a in v.as_chunks::<6>().0.iter().take(MAX_TEMP_SENSORS) {
                        let _ = s.temp_sensors.push(*a);
                    }
                }
                _ => {} // from a newer firmware; ignore
            }
        }
        Ok(s)
    }

    /// The PSK as the 64 hex digit string WPA supplicants accept in place of a passphrase.
    pub fn wifi_psk_hex(&self) -> Option<String<64>> {
        self.wifi_psk.map(|psk| crate::hex::encode::<64>(&psk))
    }

    pub fn is_provisioned(&self) -> bool {
        self.admin_hash.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Settings {
            wifi_ssid: str32("home"),
            wifi_psk: Some([7; 32]),
            admin_hash: Some(PasswordHash {
                salt: [1; 16],
                iterations: 1000,
                hash: [2; 32],
            }),
            setpoint_dc: -15,
            temp_sensors: Vec::from_slice(&[[1, 2, 3, 4, 5, 6], [9; 6]]).unwrap(),
            ntp_server: str48("10.0.2.2:12345"),
            timezone: str48("GMT0BST,M3.5.0/1,M10.5.0"),
            ..Settings::default()
        };
        let mut buf = [0u8; MAX_ENCODED];
        let n = s.encode(&mut buf);
        assert!(n < MAX_ENCODED);
        assert_eq!(Settings::decode(&buf[..n]).unwrap(), s);
    }

    #[test]
    fn unknown_tags_are_skipped() {
        let mut buf = [0u8; MAX_ENCODED];
        let n = Settings::default().encode(&mut buf);
        buf[n..n + 5].copy_from_slice(&[200, 2, 0, 9, 9]);
        assert_eq!(
            Settings::decode(&buf[..n + 5]).unwrap(),
            Settings::default()
        );
        assert_eq!(Settings::decode(&buf[..n + 4]), Err(DecodeError::Truncated));
    }

    #[test]
    fn max_size_fits() {
        let s = Settings {
            hostname: str32(&"h".repeat(40)),
            wifi_ssid: str32(&"s".repeat(40)),
            admin_user: str32(&"u".repeat(40)),
            temp_sensors: Vec::from_slice(&[[0xAA; 6]; MAX_TEMP_SENSORS]).unwrap(),
            ntp_server: str48(&"n".repeat(48)),
            timezone: str48(&"t".repeat(48)),
            wifi_psk: Some([0; 32]),
            admin_hash: Some(PasswordHash {
                salt: [0; 16],
                iterations: 1,
                hash: [0; 32],
            }),
            ..Settings::default()
        };
        let mut buf = [0u8; MAX_ENCODED];
        assert!(s.encode(&mut buf) <= MAX_ENCODED);
    }
}
