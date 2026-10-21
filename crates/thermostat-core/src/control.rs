//! Heating control: room temperature from the selected BLE sensors, and an
//! on/off decision with hysteresis.

use crate::ble::BleTable;

/// A sensor reading older than this is ignored.
pub const READING_MAX_AGE_MS: u64 = 10 * 60 * 1000;
/// Minimum time between switching the heating on/off (protects boilers and
/// relays from short-cycling). Losing all readings still switches off at once.
pub const MIN_SWITCH_INTERVAL_MS: u64 = 2 * 60 * 1000;

// The BLE table must keep a device longer than its reading stays valid,
// otherwise the table, not the age rule, decides when a sensor drops out.
const _: () = assert!(crate::ble::EXPIRY_MS > READING_MAX_AGE_MS);

/// Room temperature from the selected sensors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomTemperature {
    /// Average of the fresh readings, in hundredths of a degree Celsius.
    pub temperature_cc: i32,
    /// How many selected sensors contributed.
    pub sensors: u8,
}

impl RoomTemperature {
    /// The temperature in tenths of a degree (rounded).
    pub fn tenths(&self) -> i16 {
        let t = self.temperature_cc;
        ((t + if t >= 0 { 5 } else { -5 }) / 10) as i16
    }
}

/// Average the fresh temperature readings of the `selected` sensors.
pub fn room_temperature(
    table: &BleTable,
    selected: &[[u8; 6]],
    now_ms: u64,
) -> Option<RoomTemperature> {
    let mut sum = 0i32;
    let mut n = 0u8;
    for addr in selected {
        let Some(dev) = table.get(addr) else { continue };
        let Some(t) = dev.sensor.and_then(|r| r.temperature_cc) else {
            continue;
        };
        if now_ms.saturating_sub(dev.sensor_ms) > READING_MAX_AGE_MS {
            continue;
        }
        sum += t as i32;
        n += 1;
    }
    (n > 0).then(|| RoomTemperature {
        temperature_cc: sum / n as i32,
        sensors: n,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No fresh reading from any selected sensor (or none selected): off.
    NoReading,
    BelowBand,
    AboveBand,
    /// Inside the hysteresis band: keep the current state.
    InBand,
    /// A switch was due but the minimum interval has not passed.
    Holding,
}

pub struct Thermostat {
    heating: bool,
    last_switch_ms: Option<u64>,
    pub reason: Reason,
}

impl Default for Thermostat {
    fn default() -> Self {
        Self::new()
    }
}

impl Thermostat {
    pub const fn new() -> Self {
        Self {
            heating: false,
            last_switch_ms: None,
            reason: Reason::NoReading,
        }
    }

    pub fn heating(&self) -> bool {
        self.heating
    }

    /// Decide whether to heat. Heating turns on at or below
    /// `setpoint - hysteresis/2` and off at or above `setpoint + hysteresis/2`
    /// (tenths of °C).
    pub fn update(
        &mut self,
        room: Option<RoomTemperature>,
        setpoint_dc: i16,
        hysteresis_dc: u16,
        now_ms: u64,
    ) -> bool {
        let Some(room) = room else {
            self.reason = Reason::NoReading;
            if self.heating {
                self.switch(false, now_ms);
            }
            return self.heating;
        };
        let t = room.temperature_cc;
        let half = hysteresis_dc as i32 * 10 / 2;
        let setpoint = setpoint_dc as i32 * 10;
        let (want, reason) = if t <= setpoint - half {
            (true, Reason::BelowBand)
        } else if t >= setpoint + half {
            (false, Reason::AboveBand)
        } else {
            (self.heating, Reason::InBand)
        };
        self.reason = reason;
        if want != self.heating {
            let allowed = self
                .last_switch_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= MIN_SWITCH_INTERVAL_MS);
            if allowed {
                self.switch(want, now_ms);
            } else {
                self.reason = Reason::Holding;
            }
        }
        self.heating
    }

    fn switch(&mut self, on: bool, now_ms: u64) {
        self.heating = on;
        self.last_switch_ms = Some(now_ms);
    }
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::NoReading => "no sensor reading",
            Reason::BelowBand => "below setpoint",
            Reason::AboveBand => "above setpoint",
            Reason::InBand => "within hysteresis band",
            Reason::Holding => "waiting for minimum switch interval",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pvvx(temp_cc: i16) -> std::vec::Vec<u8> {
        let mut v = std::vec![18, 0x16, 0x1A, 0x18, 1, 2, 3, 4, 5, 6];
        v.extend_from_slice(&temp_cc.to_le_bytes());
        v.extend_from_slice(&[0x88, 0x13, 0x86, 0x0B, 90, 1, 0]);
        v
    }

    fn room(t_cc: i32) -> Option<RoomTemperature> {
        Some(RoomTemperature {
            temperature_cc: t_cc,
            sensors: 1,
        })
    }

    #[test]
    fn averages_fresh_selected_sensors() {
        let mut t = BleTable::new();
        let (a, b, c) = ([1; 6], [2; 6], [3; 6]);
        t.observe(a, 0, -50, &pvvx(2000), 1_000);
        t.observe(b, 0, -50, &pvvx(2150), 1_000);
        t.observe(c, 0, -50, &pvvx(3000), 1_000); // not selected
        let r = room_temperature(&t, &[a, b], 2_000).unwrap();
        assert_eq!((r.temperature_cc, r.sensors, r.tenths()), (2075, 2, 208));
        // Unknown or stale sensors are skipped.
        t.observe(b, 0, -50, &pvvx(2150), 1_000 + READING_MAX_AGE_MS);
        let r = room_temperature(&t, &[a, b, [9; 6]], 1_001 + READING_MAX_AGE_MS).unwrap();
        assert_eq!((r.temperature_cc, r.sensors), (2150, 1));
        assert_eq!(room_temperature(&t, &[], 0), None);
        assert_eq!(
            RoomTemperature {
                temperature_cc: -255,
                sensors: 1
            }
            .tenths(),
            -26
        );
    }

    #[test]
    fn reading_age_rule_outlives_table_expiry() {
        let mut t = BleTable::new();
        t.observe([1; 6], 0, -50, &pvvx(2000), 0);
        // Nine minutes on (and after the table's periodic expiry), still used.
        let now = 9 * 60_000;
        t.expire(now);
        assert!(room_temperature(&t, &[[1; 6]], now).is_some());
        assert!(room_temperature(&t, &[[1; 6]], READING_MAX_AGE_MS + 1).is_none());
    }

    #[test]
    fn hysteresis_band() {
        let mut th = Thermostat::new();
        // Setpoint 20.0, hysteresis 1.0: on at <= 19.5, off at >= 20.5.
        assert!(!th.update(room(1960), 200, 10, 0));
        assert_eq!(th.reason, Reason::InBand);
        assert!(th.update(room(1950), 200, 10, 0));
        assert!(
            th.update(room(2040), 200, 10, 10 * 60_000),
            "stays on inside the band"
        );
        assert!(!th.update(room(2050), 200, 10, 20 * 60_000));
        assert_eq!(th.reason, Reason::AboveBand);
    }

    #[test]
    fn minimum_switch_interval() {
        let mut th = Thermostat::new();
        assert!(th.update(room(1800), 200, 10, 0));
        // Overshoot right away: holds until the interval passes.
        assert!(th.update(room(2200), 200, 10, 30_000));
        assert_eq!(th.reason, Reason::Holding);
        assert!(!th.update(room(2200), 200, 10, MIN_SWITCH_INTERVAL_MS));
    }

    #[test]
    fn fails_safe_without_readings() {
        let mut th = Thermostat::new();
        assert!(th.update(room(1800), 200, 10, 0));
        // Losing the sensor switches off immediately, despite the interval.
        assert!(!th.update(None, 200, 10, 1_000));
        assert_eq!(th.reason, Reason::NoReading);
        assert!(!th.update(None, 200, 10, 10 * 60_000));
    }
}
