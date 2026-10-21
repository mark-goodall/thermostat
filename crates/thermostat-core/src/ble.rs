//! BLE advertisement parsing and a bounded table of recently seen devices.
//!
//! The firmware feeds raw advertising reports from the controller in here;
//! the web UI renders the table. Common BLE thermometer broadcast formats
//! (pvvx / ATC1441 custom firmware on Xiaomi sensors, BTHome v2, and
//! ThermoPro TP35x/TP39x) are decoded so remote sensors show their readings.

use heapless::{String, Vec};

/// Devices remembered at once. When full, the least recently seen is replaced.
pub const MAX_DEVICES: usize = 24;
/// Devices not heard from for this long are dropped. Longer than
/// `control::READING_MAX_AGE_MS`, so that rule decides when a selected
/// sensor's reading stops counting.
pub const EXPIRY_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrKind {
    Public,
    Random,
    Other(u8),
}

impl AddrKind {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0 | 2 => AddrKind::Public,
            1 | 3 => AddrKind::Random,
            v => AddrKind::Other(v),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AddrKind::Public => "public",
            AddrKind::Random => "random",
            AddrKind::Other(_) => "other",
        }
    }
}

/// Environmental reading decoded from a sensor broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SensorReading {
    /// Hundredths of a degree Celsius.
    pub temperature_cc: Option<i16>,
    /// Hundredths of a percent relative humidity.
    pub humidity_cp: Option<u16>,
    pub battery_pct: Option<u8>,
}

impl SensorReading {
    pub fn is_empty(&self) -> bool {
        self.temperature_cc.is_none() && self.humidity_cp.is_none() && self.battery_pct.is_none()
    }
}

/// What one advertisement payload says about a device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdvInfo {
    pub name: Option<String<24>>,
    pub company: Option<u16>,
    pub tx_power: Option<i8>,
    /// First few 16-bit service UUIDs (from UUID lists or service data).
    pub services: Vec<u16, 4>,
    pub sensor: Option<SensorReading>,
    /// Raw manufacturer specific data, including the 2-byte company ID.
    pub manufacturer: Option<Vec<u8, 16>>,
}

/// Parse the AD structures of an advertising (or scan response) payload.
/// Malformed trailing data is ignored.
pub fn parse_adv(data: &[u8]) -> AdvInfo {
    let mut info = AdvInfo::default();
    let mut rest = data;
    while let [len, tail @ ..] = rest {
        let len = *len as usize;
        if len == 0 || len > tail.len() {
            break;
        }
        let (field, next) = tail.split_at(len);
        rest = next;
        let (kind, value) = (field[0], &field[1..]);
        match kind {
            // Shortened / complete local name; prefer the complete one.
            0x08 | 0x09 if info.name.is_none() || kind == 0x09 => {
                let text = core::str::from_utf8(value).unwrap_or("");
                let mut name = String::new();
                for c in text.chars().filter(|c| !c.is_control()) {
                    if name.push(c).is_err() {
                        break;
                    }
                }
                if !name.is_empty() {
                    info.name = Some(name);
                }
            }
            // Incomplete / complete list of 16-bit service UUIDs.
            0x02 | 0x03 => {
                for uuid in value.as_chunks::<2>().0 {
                    add_service(&mut info, u16::from_le_bytes([uuid[0], uuid[1]]));
                }
            }
            0x0A if value.len() == 1 => info.tx_power = Some(value[0] as i8),
            // Service data, 16-bit UUID.
            0x16 if value.len() >= 2 => {
                let uuid = u16::from_le_bytes([value[0], value[1]]);
                add_service(&mut info, uuid);
                if let Some(r) = decode_service_data(uuid, &value[2..]) {
                    info.sensor = Some(r);
                }
            }
            0xFF if value.len() >= 2 => {
                info.company = Some(u16::from_le_bytes([value[0], value[1]]));
                info.manufacturer = Vec::from_slice(&value[..value.len().min(16)]).ok();
            }
            _ => {}
        }
    }
    info
}

fn add_service(info: &mut AdvInfo, uuid: u16) {
    if !info.services.contains(&uuid) {
        let _ = info.services.push(uuid);
    }
}

/// Decode known sensor broadcast formats carried in service data.
pub fn decode_service_data(uuid: u16, d: &[u8]) -> Option<SensorReading> {
    match (uuid, d.len()) {
        // pvvx custom firmware: MAC(6, LE) temp i16 LE 0.01°C, hum u16 LE 0.01%,
        // battery mV u16, battery %, counter, flags.
        (0x181A, 15) => Some(SensorReading {
            temperature_cc: Some(i16::from_le_bytes([d[6], d[7]])),
            humidity_cp: Some(u16::from_le_bytes([d[8], d[9]])),
            battery_pct: Some(d[12]),
        }),
        // ATC1441 format: MAC(6, BE) temp i16 BE 0.1°C, hum %, battery %, mV u16 BE, counter.
        (0x181A, 13) => Some(SensorReading {
            temperature_cc: Some(i16::from_be_bytes([d[6], d[7]]).saturating_mul(10)),
            humidity_cp: Some(d[8] as u16 * 100),
            battery_pct: Some(d[9]),
        }),
        (0xFCD2, _) => decode_bthome(d),
        _ => None,
    }
}

/// BTHome v2 (unencrypted): device info byte, then (object id, value) pairs.
fn decode_bthome(d: &[u8]) -> Option<SensorReading> {
    let (&info, mut objs) = d.split_first()?;
    if info & 0x01 != 0 || info >> 5 != 2 {
        return None; // encrypted, or not version 2
    }
    let mut r = SensorReading::default();
    while let [id, rest @ ..] = objs {
        let len = match id {
            0x00 | 0x01 | 0x2E => 1,        // packet id, battery %, humidity (1%)
            0x02 | 0x03 | 0x0C | 0x45 => 2, // temp 0.01, humidity 0.01, voltage, temp 0.1
            0x04 | 0x0B => 3,               // pressure, power
            _ => return (!r.is_empty()).then_some(r), // unknown size: stop here
        };
        let v = rest.get(..len)?;
        match id {
            0x01 => r.battery_pct = Some(v[0]),
            0x02 => r.temperature_cc = Some(i16::from_le_bytes([v[0], v[1]])),
            0x45 => r.temperature_cc = Some(i16::from_le_bytes([v[0], v[1]]).saturating_mul(10)),
            0x03 => r.humidity_cp = Some(u16::from_le_bytes([v[0], v[1]])),
            0x2E => r.humidity_cp = Some(v[0] as u16 * 100),
            _ => {}
        }
        objs = &rest[len..];
    }
    (!r.is_empty()).then_some(r)
}

/// ThermoPro TP35x / TP39x / TP59 thermo-hygrometers.
///
/// They abuse the manufacturer data's company ID field: the whole payload is
/// `marker, temp i16 LE (0.1 °C), humidity %, battery (low 2 bits), ...`.
/// Identification needs the advertised name (`TP357 (2142)`), as in
/// thermopro-ble / Theengs Decoder.
pub fn decode_thermopro(name: &str, manufacturer: &[u8]) -> Option<SensorReading> {
    if !(name.starts_with("TP35") || name.starts_with("TP39") || name.starts_with("TP59")) {
        return None;
    }
    let d = manufacturer.get(..5)?;
    if d[1..4] == [0xFF, 0xFF, 0xFF] {
        return None; // the TP357S reports this while it has no valid reading
    }
    let battery = match d[4] & 3 {
        0 => Some(1),
        1 => Some(50),
        2 => Some(100),
        _ => None,
    };
    Some(SensorReading {
        temperature_cc: Some(i16::from_le_bytes([d[1], d[2]]).saturating_mul(10)),
        humidity_cp: Some(d[3] as u16 * 100),
        battery_pct: battery,
    })
}

pub fn is_thermopro(name: Option<&str>) -> bool {
    name.is_some_and(|n| n.starts_with("TP35") || n.starts_with("TP39") || n.starts_with("TP59"))
}

/// Names for some common Bluetooth SIG company identifiers.
pub fn company_name(id: u16) -> Option<&'static str> {
    Some(match id {
        0x0006 => "Microsoft",
        0x004C => "Apple",
        0x0059 => "Nordic",
        0x0075 => "Samsung",
        0x0087 => "Garmin",
        0x00E0 => "Google",
        0x0157 => "Huami",
        0x02E5 => "Espressif",
        0x038F => "Xiaomi",
        0x0499 => "Ruuvi",
        0x0822 => "adafruit",
        0x0969 => "SwitchBot",
        _ => return None,
    })
}

/// Names for some 16-bit service UUIDs seen in advertisements.
pub fn service_name(uuid: u16) -> Option<&'static str> {
    Some(match uuid {
        0x180F => "Battery",
        0x181A => "Environmental",
        0xFCD2 => "BTHome",
        0xFD6F => "Exposure Notif.",
        0xFE2C | 0xFCF1 | 0xFEF3 | 0xFE9F => "Google",
        0xFE95 => "Xiaomi MiBeacon",
        0xFEAA => "Eddystone",
        0xFE07 => "Sonos",
        0xFD5A => "Samsung",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleDevice {
    pub addr: [u8; 6],
    pub addr_kind: AddrKind,
    pub rssi: i8,
    pub name: Option<String<24>>,
    pub company: Option<u16>,
    pub tx_power: Option<i8>,
    pub services: Vec<u16, 4>,
    pub sensor: Option<SensorReading>,
    /// When `sensor` was last updated from an advertisement.
    pub sensor_ms: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub packets: u32,
}

impl BleDevice {
    /// Address in the usual most-significant-first notation.
    pub fn addr_string(&self) -> String<17> {
        format_addr(&self.addr)
    }
}

/// Parse `AA:BB:CC:DD:EE:FF` (as shown by [`BleDevice::addr_string`]) into
/// on-air byte order.
pub fn parse_addr(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for i in (0..6).rev() {
        let p = parts.next()?;
        if p.len() != 2 {
            return None;
        }
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

pub fn format_addr(a: &[u8; 6]) -> String<17> {
    let mut s = String::new();
    let _ = core::fmt::write(
        &mut s,
        format_args!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            a[5], a[4], a[3], a[2], a[1], a[0]
        ),
    );
    s
}

#[derive(Debug, Default)]
pub struct BleTable {
    devices: Vec<BleDevice, MAX_DEVICES>,
    /// Advertising reports processed since start.
    pub reports: u32,
    pub scanning: bool,
    pub error: Option<&'static str>,
}

impl BleTable {
    pub const fn new() -> Self {
        Self {
            devices: Vec::new(),
            reports: 0,
            scanning: false,
            error: None,
        }
    }

    /// Record one advertising report. `addr` is little-endian as on the wire.
    pub fn observe(&mut self, addr: [u8; 6], addr_kind: u8, rssi: i8, data: &[u8], now_ms: u64) {
        self.reports = self.reports.wrapping_add(1);
        let info = parse_adv(data);
        let dev = match self.devices.iter().position(|d| d.addr == addr) {
            Some(i) => &mut self.devices[i],
            None => {
                let fresh = BleDevice {
                    addr,
                    addr_kind: AddrKind::from_raw(addr_kind),
                    rssi,
                    name: None,
                    company: None,
                    tx_power: None,
                    services: Vec::new(),
                    sensor: None,
                    sensor_ms: 0,
                    first_seen_ms: now_ms,
                    last_seen_ms: now_ms,
                    packets: 0,
                };
                if self.devices.is_full() {
                    let oldest = (0..self.devices.len())
                        .min_by_key(|&i| self.devices[i].last_seen_ms)
                        .unwrap();
                    self.devices[oldest] = fresh;
                    &mut self.devices[oldest]
                } else {
                    let _ = self.devices.push(fresh);
                    self.devices.last_mut().unwrap()
                }
            }
        };
        dev.rssi = rssi;
        dev.last_seen_ms = now_ms;
        dev.packets = dev.packets.saturating_add(1);
        // Advertisements and scan responses carry different fields: merge.
        if info.name.is_some() {
            dev.name = info.name;
        }
        if info.company.is_some() {
            dev.company = info.company;
        }
        if info.tx_power.is_some() {
            dev.tx_power = info.tx_power;
        }
        for s in info.services {
            if !dev.services.contains(&s) {
                let _ = dev.services.push(s);
            }
        }
        let mut reading = info.sensor;
        // ThermoPro readings live in the manufacturer data; the name that
        // identifies the model may have arrived in an earlier scan response.
        if let (Some(name), Some(mfr)) = (&dev.name, &info.manufacturer)
            && let Some(r) = decode_thermopro(name, mfr)
        {
            reading = Some(r);
        }
        if is_thermopro(dev.name.as_deref()) {
            dev.company = None; // the "company ID" is really sensor data
        }
        if reading.is_some() {
            dev.sensor = reading;
            dev.sensor_ms = now_ms;
        }
    }

    pub fn get(&self, addr: &[u8; 6]) -> Option<&BleDevice> {
        self.devices.iter().find(|d| &d.addr == addr)
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.devices
            .retain(|d| now_ms.saturating_sub(d.last_seen_ms) < EXPIRY_MS);
    }

    /// Devices ordered by signal strength, strongest first.
    pub fn sorted(&self) -> Vec<BleDevice, MAX_DEVICES> {
        let mut v = self.devices.clone();
        v.sort_unstable_by(|a, b| b.rssi.cmp(&a.rssi).then(a.addr.cmp(&b.addr)));
        v
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(kind: u8, value: &[u8]) -> std::vec::Vec<u8> {
        let mut v = std::vec![value.len() as u8 + 1, kind];
        v.extend_from_slice(value);
        v
    }

    #[test]
    fn parses_common_fields() {
        let mut data = ad(0x01, &[0x06]);
        data.extend(ad(0x09, b"Kitchen"));
        data.extend(ad(0x03, &[0x0F, 0x18, 0x1A, 0x18]));
        data.extend(ad(0x0A, &[0xF4]));
        data.extend(ad(0xFF, &[0x4C, 0x00, 0x10, 0x05]));
        let info = parse_adv(&data);
        assert_eq!(info.name.as_deref(), Some("Kitchen"));
        assert_eq!(info.services.as_slice(), &[0x180F, 0x181A]);
        assert_eq!(info.tx_power, Some(-12));
        assert_eq!(info.company, Some(0x004C));
        assert_eq!(company_name(0x004C), Some("Apple"));
        assert_eq!(service_name(0xFE95), Some("Xiaomi MiBeacon"));
    }

    #[test]
    fn tolerates_malformed_data() {
        assert_eq!(parse_adv(&[]), AdvInfo::default());
        assert_eq!(parse_adv(&[5, 0x09, b'a']), AdvInfo::default()); // length past end
        let mut data = ad(0x09, b"ok");
        data.extend_from_slice(&[0, 0xFF, 0xFF]);
        assert_eq!(parse_adv(&data).name.as_deref(), Some("ok"));
        // Invalid UTF-8 / control characters do not end up in the name.
        assert_eq!(parse_adv(&ad(0x09, &[0xFF, 0xFE])).name, None);
        assert_eq!(parse_adv(&ad(0x09, b"a\nb")).name.as_deref(), Some("ab"));
    }

    #[test]
    fn decodes_pvvx() {
        // 21.37 °C, 45.67 %, 2950 mV, 87 %
        let mut sd = std::vec![0x1A, 0x18, 1, 2, 3, 4, 5, 6];
        sd.extend_from_slice(&2137i16.to_le_bytes());
        sd.extend_from_slice(&4567u16.to_le_bytes());
        sd.extend_from_slice(&2950u16.to_le_bytes());
        sd.extend_from_slice(&[87, 9, 0]);
        let info = parse_adv(&ad(0x16, &sd));
        assert_eq!(
            info.sensor,
            Some(SensorReading {
                temperature_cc: Some(2137),
                humidity_cp: Some(4567),
                battery_pct: Some(87)
            })
        );
    }

    #[test]
    fn decodes_atc1441() {
        // -4.5 °C, 61 %, 92 %
        let mut sd = std::vec![0x1A, 0x18, 6, 5, 4, 3, 2, 1];
        sd.extend_from_slice(&(-45i16).to_be_bytes());
        sd.extend_from_slice(&[61, 92, 0x0B, 0xB8, 7]);
        let r = parse_adv(&ad(0x16, &sd)).sensor.unwrap();
        assert_eq!(
            (r.temperature_cc, r.humidity_cp, r.battery_pct),
            (Some(-450), Some(6100), Some(92))
        );
    }

    #[test]
    fn decodes_bthome_v2() {
        // info 0x40, battery 0x01=97, temp 0x02=23.45, humidity 0x03=50.55
        let mut sd = std::vec![0xD2, 0xFC, 0x40, 0x01, 97, 0x02];
        sd.extend_from_slice(&2345i16.to_le_bytes());
        sd.push(0x03);
        sd.extend_from_slice(&5055u16.to_le_bytes());
        let r = parse_adv(&ad(0x16, &sd)).sensor.unwrap();
        assert_eq!(
            (r.temperature_cc, r.humidity_cp, r.battery_pct),
            (Some(2345), Some(5055), Some(97))
        );
        // Encrypted payloads are not decoded.
        assert_eq!(decode_service_data(0xFCD2, &[0x41, 0x02, 0, 0]), None);
    }

    #[test]
    fn address_text_roundtrip() {
        let a = [0x01, 0x02, 0x03, 0x04, 0x05, 0xAB];
        assert_eq!(format_addr(&a).as_str(), "AB:05:04:03:02:01");
        assert_eq!(parse_addr("ab:05:04:03:02:01"), Some(a));
        assert_eq!(parse_addr("AB:05:04:03:02"), None);
        assert_eq!(parse_addr("AB:05:04:03:02:01:00"), None);
        assert_eq!(parse_addr("AB:05:04:03:02:1"), None);
        assert_eq!(parse_addr("zz:05:04:03:02:01"), None);
    }

    #[test]
    fn decodes_thermopro() {
        // Test vectors from thermopro-ble / Theengs Decoder.
        let r = decode_thermopro("TP357 (2142)", &[0xC2, 0xF1, 0x00, 0x1D, 0x02, 0x2C]).unwrap();
        assert_eq!(
            (r.temperature_cc, r.humidity_cp, r.battery_pct),
            (Some(2410), Some(2900), Some(100))
        );
        let r = decode_thermopro("TP357", &[0xC2, 0x10, 0x01, 0x47, 0x02, 0x2C]).unwrap();
        assert_eq!((r.temperature_cc, r.humidity_cp), (Some(2720), Some(7100)));
        let r =
            decode_thermopro("TP357S (3C62)", &[0xC2, 0xD6, 0x00, 0x43, 0x22, 0x0B, 0x01]).unwrap();
        assert_eq!(
            (r.temperature_cc, r.humidity_cp, r.battery_pct),
            (Some(2140), Some(6700), Some(100))
        );
        // -3.5 °C (0xFFDD), 50 %, low battery.
        let r = decode_thermopro("TP357", &[0xC2, 0xDD, 0xFF, 50, 0]).unwrap();
        assert_eq!((r.temperature_cc, r.battery_pct), (Some(-350), Some(1)));
        // Invalid marker frame, wrong name, too short.
        assert!(decode_thermopro("TP357S", &[0xC2, 0xFF, 0xFF, 0xFF, 0x0B]).is_none());
        assert!(decode_thermopro("Phone", &[0xC2, 0xF1, 0x00, 0x1D, 0x02]).is_none());
        assert!(decode_thermopro("TP357", &[0xC2, 0xF1, 0x00]).is_none());
    }

    #[test]
    fn thermopro_name_from_scan_response() {
        let mut t = BleTable::new();
        let a = [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA];
        let adv = ad(0xFF, &[0xC2, 0xF1, 0x00, 0x1D, 0x02, 0x2C]);
        // Advertisement before the name is known: not decoded yet.
        t.observe(a, 0, -60, &adv, 0);
        assert_eq!(t.get(&a).unwrap().sensor, None);
        t.observe(a, 0, -60, &ad(0x09, b"TP357 (2142)"), 100); // scan response
        t.observe(a, 0, -60, &adv, 2000);
        let d = t.get(&a).unwrap();
        assert_eq!(d.sensor.unwrap().temperature_cc, Some(2410));
        assert_eq!((d.sensor_ms, d.company), (2000, None));
    }

    #[test]
    fn table_merges_evicts_and_expires() {
        let mut t = BleTable::new();
        let a = [1, 0, 0, 0, 0, 0xAA];
        t.observe(a, 0, -70, &ad(0xFF, &[0x06, 0x00]), 0);
        t.observe(a, 0, -60, &ad(0x09, b"Laptop"), 10); // scan response adds the name
        let d = &t.sorted()[0];
        assert_eq!(
            (d.rssi, d.packets, d.company, d.name.as_deref()),
            (-60, 2, Some(6), Some("Laptop"))
        );
        assert_eq!(d.addr_string().as_str(), "AA:00:00:00:00:01");

        for i in 0..MAX_DEVICES as u8 {
            t.observe([2, 0, 0, 0, 0, i], 1, -90, &[], 100 + i as u64);
        }
        assert_eq!(t.len(), MAX_DEVICES);
        assert!(
            t.sorted().iter().all(|d| d.addr != a),
            "least recently seen evicted"
        );
        assert_eq!(t.sorted()[0].addr_kind, AddrKind::Random);

        t.expire(100 + EXPIRY_MS + 5);
        assert_eq!(t.len(), MAX_DEVICES - 6);
        assert_eq!(t.reports, 2 + MAX_DEVICES as u32);
    }
}
