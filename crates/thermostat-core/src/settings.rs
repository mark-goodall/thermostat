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
pub const MAX_ENCODED: usize = 640;

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
    /// Default target temperature (tenths of °C) for a newly selected sensor;
    /// each sensor then has its own setpoint (`temp_sensors`).
    pub setpoint_dc: i16,
    /// Switching hysteresis in tenths of a degree Celsius.
    pub hysteresis_dc: u16,
    /// Minimum heater on time in minutes (short-cycle protection).
    pub min_on_min: u16,
    /// BLE sensors whose readings drive the thermostat, each with its setpoint.
    pub temp_sensors: Vec<TempSensor, MAX_TEMP_SENSORS>,
    /// Heater group: thermostats with the same id share one heater and its
    /// short-cycle protection. Empty = this thermostat has its own heater.
    pub heater_id: Str32,
    /// Shared secret signing zone messages within the group (empty = unsigned).
    pub group_key: Str32,
    /// NTP server, `host[:port]`.
    pub ntp_server: Str48,
    /// POSIX TZ string, e.g. `GMT0BST,M3.5.0/1,M10.5.0` (see `clock::TimeZone`).
    pub timezone: Str48,
    /// Names of the heating zones (empty = "Zone 1", "Zone 2", ...).
    pub zone_names: [ZoneName; MAX_ZONES],
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

pub const MAX_TEMP_SENSORS: usize = 8;

/// Longest sensor name, in bytes.
pub const MAX_SENSOR_NAME: usize = 16;
pub type SensorName = String<MAX_SENSOR_NAME>;

/// A temperature sensor selected to control the heating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempSensor {
    /// BLE address in on-air byte order.
    pub addr: [u8; 6],
    /// Target temperature for this sensor's room, tenths of °C.
    pub setpoint_dc: i16,
    /// Our name for it (e.g. "Landing"); empty = none. The sensor's own
    /// advertised name cannot be changed.
    pub name: SensorName,
    /// Heating zone it belongs to (0-based; boards with one zone use 0).
    pub zone: u8,
}

/// Heating zones a board can have (one relay each).
pub const MAX_ZONES: usize = 2;
pub type ZoneName = SensorName;

impl Settings {
    /// The selected sensor with this address.
    pub fn temp_sensor(&self, addr: &[u8; 6]) -> Option<&TempSensor> {
        self.temp_sensors.iter().find(|t| &t.addr == addr)
    }
}

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
            min_on_min: crate::control::DEFAULT_MIN_ON_MIN,
            temp_sensors: Vec::new(),
            heater_id: String::new(),
            group_key: String::new(),
            ntp_server: str48("pool.ntp.org"),
            timezone: str48("UTC0"),
            zone_names: Default::default(),
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
    pub const MIN_ON: u8 = 11;
    pub const HEATER_ID: u8 = 12;
    pub const GROUP_KEY: u8 = 13;
    /// One i16 per entry of TEMP_SENSORS, same order. Settings written before
    /// per-sensor setpoints lack it; those sensors take SETPOINT.
    pub const SENSOR_SETPOINTS: u8 = 14;
    /// Per entry of TEMP_SENSORS, same order: a length byte and the UTF-8 name.
    pub const SENSOR_NAMES: u8 = 15;
    /// One byte per entry of TEMP_SENSORS, same order: its zone. Absent = zone 0.
    pub const SENSOR_ZONES: u8 = 16;
    /// Per zone: a length byte and the UTF-8 name.
    pub const ZONE_NAMES: u8 = 17;
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
        w.field(tag::MIN_ON, &self.min_on_min.to_le_bytes());
        if !self.heater_id.is_empty() {
            w.field(tag::HEATER_ID, self.heater_id.as_bytes());
        }
        if !self.group_key.is_empty() {
            w.field(tag::GROUP_KEY, self.group_key.as_bytes());
        }
        w.field(tag::NTP_SERVER, self.ntp_server.as_bytes());
        w.field(tag::TIMEZONE, self.timezone.as_bytes());
        if !self.temp_sensors.is_empty() {
            let mut addrs = [0u8; 6 * MAX_TEMP_SENSORS];
            let mut setpoints = [0u8; 2 * MAX_TEMP_SENSORS];
            for (i, t) in self.temp_sensors.iter().enumerate() {
                addrs[i * 6..i * 6 + 6].copy_from_slice(&t.addr);
                setpoints[i * 2..i * 2 + 2].copy_from_slice(&t.setpoint_dc.to_le_bytes());
            }
            let n = self.temp_sensors.len();
            w.field(tag::TEMP_SENSORS, &addrs[..n * 6]);
            w.field(tag::SENSOR_SETPOINTS, &setpoints[..n * 2]);
            if self.temp_sensors.iter().any(|t| !t.name.is_empty()) {
                let mut names = [0u8; (1 + MAX_SENSOR_NAME) * MAX_TEMP_SENSORS];
                let mut len = 0;
                for t in &self.temp_sensors {
                    names[len] = t.name.len() as u8;
                    names[len + 1..len + 1 + t.name.len()].copy_from_slice(t.name.as_bytes());
                    len += 1 + t.name.len();
                }
                w.field(tag::SENSOR_NAMES, &names[..len]);
            }
            if self.temp_sensors.iter().any(|t| t.zone != 0) {
                let mut zones = [0u8; MAX_TEMP_SENSORS];
                for (i, t) in self.temp_sensors.iter().enumerate() {
                    zones[i] = t.zone;
                }
                w.field(tag::SENSOR_ZONES, &zones[..n]);
            }
        }
        if self.zone_names.iter().any(|z| !z.is_empty()) {
            let mut names = [0u8; (1 + MAX_SENSOR_NAME) * MAX_ZONES];
            let mut len = 0;
            for z in &self.zone_names {
                names[len] = z.len() as u8;
                names[len + 1..len + 1 + z.len()].copy_from_slice(z.as_bytes());
                len += 1 + z.len();
            }
            w.field(tag::ZONE_NAMES, &names[..len]);
        }
        w.pos
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, DecodeError> {
        let mut s = Settings::default();
        let mut setpoints: Option<&[u8]> = None;
        let mut names: &[u8] = &[];
        let mut zones: &[u8] = &[];
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
                tag::MIN_ON => {
                    s.min_on_min =
                        u16::from_le_bytes(v.try_into().map_err(|_| DecodeError::BadValue)?)
                }
                tag::HEATER_ID => s.heater_id = text()?,
                tag::GROUP_KEY => s.group_key = text()?,
                tag::NTP_SERVER => s.ntp_server = text48()?,
                tag::TIMEZONE => s.timezone = text48()?,
                tag::TEMP_SENSORS => {
                    if v.len() % 6 != 0 {
                        return Err(DecodeError::BadValue);
                    }
                    s.temp_sensors.clear();
                    for a in v.as_chunks::<6>().0.iter().take(MAX_TEMP_SENSORS) {
                        let _ = s.temp_sensors.push(TempSensor {
                            addr: *a,
                            setpoint_dc: i16::MIN, // filled in below
                            name: SensorName::new(),
                            zone: 0,
                        });
                    }
                }
                tag::SENSOR_SETPOINTS => {
                    if v.len() % 2 != 0 {
                        return Err(DecodeError::BadValue);
                    }
                    setpoints = Some(v);
                }
                tag::SENSOR_NAMES => names = v,
                tag::SENSOR_ZONES => zones = v,
                tag::ZONE_NAMES => {
                    let mut rest = v;
                    for z in s.zone_names.iter_mut() {
                        let Some((&len, r)) = rest.split_first() else {
                            break;
                        };
                        let name = r.get(..len as usize).ok_or(DecodeError::Truncated)?;
                        *z = core::str::from_utf8(name)
                            .ok()
                            .and_then(|n| ZoneName::try_from(n).ok())
                            .ok_or(DecodeError::BadValue)?;
                        rest = &r[len as usize..];
                    }
                }
                _ => {} // from a newer firmware; ignore
            }
        }
        let setpoints = setpoints.unwrap_or(&[]).as_chunks::<2>().0;
        for (i, t) in s.temp_sensors.iter_mut().enumerate() {
            t.zone = zones
                .get(i)
                .copied()
                .filter(|z| (*z as usize) < MAX_ZONES)
                .unwrap_or(0);
            t.setpoint_dc = setpoints
                .get(i)
                .map_or(s.setpoint_dc, |b| i16::from_le_bytes(*b));
            if let Some((&len, rest)) = names.split_first() {
                let name = rest.get(..len as usize).ok_or(DecodeError::Truncated)?;
                t.name = core::str::from_utf8(name)
                    .ok()
                    .and_then(|n| SensorName::try_from(n).ok())
                    .ok_or(DecodeError::BadValue)?;
                names = &rest[len as usize..];
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
            temp_sensors: Vec::from_slice(&[
                TempSensor {
                    addr: [1, 2, 3, 4, 5, 6],
                    setpoint_dc: 215,
                    name: SensorName::try_from("Landing").unwrap(),
                    zone: 1,
                },
                TempSensor {
                    addr: [9; 6],
                    setpoint_dc: -40,
                    name: SensorName::new(),
                    zone: 0,
                },
            ])
            .unwrap(),
            ntp_server: str48("10.0.2.2:12345"),
            min_on_min: 90,
            heater_id: str32("boiler"),
            group_key: str32("secret"),
            timezone: str48("GMT0BST,M3.5.0/1,M10.5.0"),
            zone_names: [ZoneName::try_from("Downstairs").unwrap(), ZoneName::new()],
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
    fn sensors_without_setpoints_take_the_default() {
        // Written by a firmware before per-sensor setpoints: no SENSOR_SETPOINTS.
        let mut buf = std::vec::Vec::new();
        buf.extend_from_slice(&[tag::SETPOINT, 2, 0]);
        buf.extend_from_slice(&195i16.to_le_bytes());
        buf.extend_from_slice(&[tag::TEMP_SENSORS, 12, 0]);
        buf.extend_from_slice(&[1; 6]);
        buf.extend_from_slice(&[2; 6]);
        let s = Settings::decode(&buf).unwrap();
        assert_eq!(
            s.temp_sensors.as_slice(),
            &[
                TempSensor {
                    addr: [1; 6],
                    setpoint_dc: 195,
                    name: SensorName::new(),
                    zone: 0,
                },
                TempSensor {
                    addr: [2; 6],
                    setpoint_dc: 195,
                    name: SensorName::new(),
                    zone: 0,
                }
            ]
        );
        assert_eq!(s.temp_sensor(&[2; 6]).unwrap().setpoint_dc, 195);
    }

    #[test]
    fn max_size_fits() {
        let s = Settings {
            hostname: str32(&"h".repeat(40)),
            wifi_ssid: str32(&"s".repeat(40)),
            admin_user: str32(&"u".repeat(40)),
            temp_sensors: core::iter::repeat_n(
                TempSensor {
                    addr: [0xAA; 6],
                    setpoint_dc: 0,
                    name: SensorName::try_from("n".repeat(MAX_SENSOR_NAME).as_str()).unwrap(),
                    zone: 1,
                },
                MAX_TEMP_SENSORS,
            )
            .collect(),
            ntp_server: str48(&"n".repeat(48)),
            heater_id: str32(&"h".repeat(32)),
            group_key: str32(&"k".repeat(32)),
            timezone: str48(&"t".repeat(48)),
            zone_names: core::array::from_fn(|_| {
                ZoneName::try_from("z".repeat(MAX_SENSOR_NAME).as_str()).unwrap()
            }),
            wifi_psk: Some([0; 32]),
            admin_hash: Some(PasswordHash {
                salt: [0; 16],
                iterations: 1,
                hash: [0; 32],
            }),
            ..Settings::default()
        };
        let mut buf = [0u8; MAX_ENCODED];
        let n = s.encode(&mut buf);
        assert!(n <= MAX_ENCODED);
        assert_eq!(Settings::decode(&buf[..n]).unwrap(), s);
    }
}
