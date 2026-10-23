//! Heating control: readings from the selected BLE sensors, each with its own
//! setpoint, and an on/off decision with hysteresis.

use heapless::Vec;

use crate::ble::BleTable;
use crate::settings::{MAX_TEMP_SENSORS, MAX_ZONES, TempSensor};

/// A sensor reading older than this is ignored.
pub const READING_MAX_AGE_MS: u64 = 10 * 60 * 1000;
/// Default minimum heater on time in minutes (short-cycle protection;
/// configurable in the settings).
pub const DEFAULT_MIN_ON_MIN: u16 = 4 * 60;
/// Upper limit for the configurable minimum on time (one day).
pub const MAX_MIN_ON_MIN: u16 = 24 * 60;

// The BLE table must keep a device longer than its reading stays valid,
// otherwise the table, not the age rule, decides when a sensor drops out.
const _: () = assert!(crate::ble::EXPIRY_MS > READING_MAX_AGE_MS);

/// Hundredths of a degree to tenths, rounded.
pub fn cc_to_dc(t: i32) -> i16 {
    ((t + if t >= 0 { 5 } else { -5 }) / 10) as i16
}

/// Where one sensor stands against its setpoint and the hysteresis band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    /// At or below `setpoint - hysteresis/2`: wants heat.
    Below,
    Inside,
    /// At or above `setpoint + hysteresis/2`: warm enough.
    Above,
}

impl Band {
    pub fn as_str(&self) -> &'static str {
        match self {
            Band::Below => "below",
            Band::Inside => "in band",
            Band::Above => "above",
        }
    }
}

/// A fresh reading from a selected sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorReading {
    pub addr: [u8; 6],
    /// Hundredths of a degree Celsius.
    pub temperature_cc: i32,
    /// Tenths of a degree Celsius.
    pub setpoint_dc: i16,
    /// Heating zone the sensor belongs to.
    pub zone: u8,
    /// `setpoint_dc` comes from the sensor's schedule (see [`apply_schedules`]).
    pub scheduled: bool,
}

impl SensorReading {
    pub fn band(&self, hysteresis_dc: u16) -> Band {
        let half = hysteresis_dc as i32 * 10 / 2;
        let setpoint = self.setpoint_dc as i32 * 10;
        if self.temperature_cc <= setpoint - half {
            Band::Below
        } else if self.temperature_cc >= setpoint + half {
            Band::Above
        } else {
            Band::Inside
        }
    }
}

pub type Readings = Vec<SensorReading, MAX_TEMP_SENSORS>;

/// The fresh temperature readings of the selected sensors.
pub fn readings(table: &BleTable, selected: &[TempSensor], now_ms: u64) -> Readings {
    let mut out = Readings::new();
    for sel in selected {
        let Some(dev) = table.get(&sel.addr) else {
            continue;
        };
        let Some(t) = dev.sensor.and_then(|r| r.temperature_cc) else {
            continue;
        };
        if now_ms.saturating_sub(dev.sensor_ms) > READING_MAX_AGE_MS {
            continue;
        }
        let _ = out.push(SensorReading {
            addr: sel.addr,
            temperature_cc: t as i32,
            setpoint_dc: sel.setpoint_dc,
            zone: sel.zone,
            scheduled: false,
        });
    }
    out
}

/// Give each reading with a schedule its scheduled setpoint for local time
/// `local`. Without the time (no NTP yet) the fixed setpoints stay.
pub fn apply_schedules(
    readings: &mut [SensorReading],
    schedules: &crate::schedule::Schedules,
    local: Option<&crate::clock::DateTime>,
) {
    let Some(local) = local else { return };
    for r in readings {
        if let Some(s) = schedules.get(&r.addr) {
            r.setpoint_dc = s.setpoint_dc(local);
            r.scheduled = true;
        }
    }
}

/// Average of the readings in tenths of a degree (for display).
pub fn average_dc(readings: &[SensorReading]) -> Option<i16> {
    let n = readings.len() as i32;
    (n > 0).then(|| cc_to_dc(readings.iter().map(|r| r.temperature_cc).sum::<i32>() / n))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// No fresh reading from any selected sensor (or none selected): off.
    NoReading,
    /// At least one sensor is at or below its setpoint minus half the hysteresis.
    BelowBand,
    /// Every sensor is at or above its setpoint plus half the hysteresis.
    AboveBand,
    /// Neither: keep the current state.
    InBand,
    /// Warm enough, but the heater has not yet run for its minimum on time.
    MinOnTime,
    /// Warm enough, and another zone seems to be heating: asking the group to
    /// confirm before switching off (see [`Thermostat::released`]).
    Releasing,
    /// Boosted: heating on for a while whatever the sensors say.
    Boost,
}

/// The rest of the heater group, as seen by this zone (see `zones`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupState {
    /// Another zone currently requests heat, so the heater runs regardless of us.
    pub others_heating: bool,
    /// When the heater last switched on, as reported by the group (our monotonic ms).
    pub heater_on_since_ms: Option<u64>,
}

impl GroupState {
    /// A thermostat with its own heater.
    pub const STANDALONE: GroupState = GroupState {
        others_heating: false,
        heater_on_since_ms: None,
    };
}

pub struct Thermostat {
    heating: bool,
    /// When this zone switched the heater on (monotonic ms).
    started_heater_ms: Option<u64>,
    /// When this zone last switched itself on, whether or not that started the heater.
    on_since_ms: Option<u64>,
    started_heater: bool,
    /// While held on: when switching off becomes allowed.
    hold_until_ms: Option<u64>,
    /// Waiting for the group to confirm that another zone keeps the heater on;
    /// the number identifies the request.
    releasing: Option<u32>,
    release_seq: u32,
    /// While boosted: when the boost ends (monotonic ms).
    boost_until_ms: Option<u64>,
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
            started_heater_ms: None,
            on_since_ms: None,
            started_heater: false,
            hold_until_ms: None,
            releasing: None,
            release_seq: 0,
            boost_until_ms: None,
            reason: Reason::NoReading,
        }
    }

    /// Heat for `duration_ms` from now whatever the sensors say (applied at the
    /// next update). Afterwards normal control resumes, including the minimum
    /// on time, so the heater may run on past the boost.
    pub fn boost(&mut self, now_ms: u64, duration_ms: u64) {
        self.boost_until_ms = Some(now_ms + duration_ms);
    }

    pub fn cancel_boost(&mut self) {
        self.boost_until_ms = None;
    }

    /// Milliseconds of boost left, while boosted.
    pub fn boost_remaining_ms(&self, now_ms: u64) -> Option<u64> {
        self.boost_until_ms
            .filter(|&t| t > now_ms)
            .map(|t| t - now_ms)
    }

    /// Whether the heat output is on.
    pub fn heating(&self) -> bool {
        self.heating
    }

    /// Whether this zone tells the group it keeps the heater on: true while
    /// heating or held on, false once it has asked to leave (releasing).
    pub fn requesting(&self) -> bool {
        self.heating && self.releasing.is_none()
    }

    /// The release waiting for confirmation, if any (pass it to [`released`](Self::released)).
    pub fn release_pending(&self) -> Option<u32> {
        self.releasing
    }

    /// The group's answer to a release. `confirmed` means another zone replied,
    /// after hearing that we no longer request heat, that it keeps the heater on,
    /// so we can switch off. Otherwise we may be the last zone: the output stays
    /// on and the next [`update`](Self::update) applies the minimum on time.
    /// Answers to an older or cancelled release are ignored.
    pub fn released(&mut self, release: u32, confirmed: bool) {
        if self.releasing != Some(release) {
            return;
        }
        self.releasing = None;
        if confirmed {
            self.heating = false;
            self.reason = Reason::AboveBand;
        }
    }

    /// True once after an update in which this zone switched the heater on
    /// (the group should learn the new start time).
    pub fn take_heater_started(&mut self) -> bool {
        core::mem::take(&mut self.started_heater)
    }

    /// Milliseconds the heater must still stay on (minimum on time).
    pub fn hold_remaining_ms(&self, now_ms: u64) -> Option<u64> {
        match self.reason {
            Reason::MinOnTime => self.hold_until_ms.map(|t| t.saturating_sub(now_ms)),
            _ => None,
        }
    }

    /// Decide whether to heat from the fresh `readings`, each against its own
    /// setpoint. Heating turns on as soon as any sensor is at or below
    /// `setpoint - hysteresis/2`, and off once every sensor is at or above
    /// `setpoint + hysteresis/2` (tenths of °C).
    ///
    /// Short-cycle protection is a minimum heater **on** time: a request for
    /// heat is always acted on at once. The last zone switching off keeps the
    /// heater on until it has run for `min_on_ms` since it started. Losing all
    /// readings switches off at once.
    ///
    /// A zone that wants to switch off early while `group.others_heating`
    /// releases instead: it stops requesting heat but keeps its output on until
    /// the group answers through [`released`](Self::released). Two zones leaving
    /// at the same moment then both see the other no longer requesting, and
    /// both hold the heater on rather than both switching it off.
    pub fn update(
        &mut self,
        readings: &[SensorReading],
        hysteresis_dc: u16,
        min_on_ms: u64,
        group: GroupState,
        now_ms: u64,
    ) -> bool {
        self.hold_until_ms = None;
        let boosting = self.boost_remaining_ms(now_ms).is_some();
        if !boosting {
            self.boost_until_ms = None;
        }
        // Without readings heating stops at once, unless boosted (a boost is
        // short and explicitly asked for).
        if readings.is_empty() && !boosting {
            self.reason = Reason::NoReading;
            self.heating = false;
            self.releasing = None;
            return false;
        }
        let band = |b| readings.iter().filter(move |r| r.band(hysteresis_dc) == b);
        let (want, reason) = if boosting {
            (true, Reason::Boost)
        } else if band(Band::Below).next().is_some() {
            (true, Reason::BelowBand)
        } else if band(Band::Above).count() == readings.len() {
            (false, Reason::AboveBand)
        } else {
            (self.requesting(), Reason::InBand)
        };
        self.reason = reason;
        if want {
            self.releasing = None;
        }
        if want && !self.heating {
            self.heating = true;
            self.on_since_ms = Some(now_ms);
            if !group.others_heating {
                self.started_heater_ms = Some(now_ms);
                self.started_heater = true;
            }
        } else if !want && self.heating {
            // Count the heater's on time from the latest start we know of; if none
            // is known (we joined a running heater and never heard when it
            // started), from when we switched on, which errs on the long side.
            let since = self
                .started_heater_ms
                .max(group.heater_on_since_ms)
                .or(self.on_since_ms);
            match since {
                Some(since) if now_ms.saturating_sub(since) < min_on_ms => {
                    if group.others_heating {
                        // Someone else may keep the heater on: ask before leaving.
                        if self.releasing.is_none() {
                            self.release_seq = self.release_seq.wrapping_add(1);
                            self.releasing = Some(self.release_seq);
                        }
                        self.reason = Reason::Releasing;
                    } else {
                        // We are the last zone: switching off would stop the heater.
                        self.releasing = None;
                        self.reason = Reason::MinOnTime;
                        self.hold_until_ms = Some(since + min_on_ms);
                    }
                }
                // The heater has run long enough: off, whoever else is heating.
                _ => {
                    self.heating = false;
                    self.releasing = None;
                }
            }
        }
        self.heating
    }
}

/// The heating zones of one board, each with its own relay, sharing one heater.
///
/// Each zone decides on its own from its sensors (see [`Thermostat::update`]),
/// and the minimum on time applies to the heater as a whole, exactly as for a
/// heater group: a zone requesting heat gets it at once; a zone leaving while
/// another zone keeps the heater on switches off at once; the last zone keeps
/// its relay on until the heater has run for the minimum time since it started,
/// whichever zone started it. The zones are decided together, so a zone always
/// knows whether its neighbour is heating and never needs to ask (no release).
pub struct Controller {
    zones: [Thermostat; MAX_ZONES],
    count: usize,
    /// When this board last started the heater (monotonic ms).
    heater_on_since_ms: Option<u64>,
    started_heater: bool,
}

impl Controller {
    pub const fn new(count: usize) -> Self {
        assert!(count >= 1 && count <= MAX_ZONES);
        Self {
            zones: [const { Thermostat::new() }; MAX_ZONES],
            count,
            heater_on_since_ms: None,
            started_heater: false,
        }
    }

    pub fn zones(&self) -> &[Thermostat] {
        &self.zones[..self.count]
    }

    pub fn zones_mut(&mut self) -> &mut [Thermostat] {
        &mut self.zones[..self.count]
    }

    /// Whether any relay is on.
    pub fn heating(&self) -> bool {
        self.zones().iter().any(|z| z.heating())
    }

    /// Whether this board tells a heater group that it keeps the heater on.
    pub fn requesting(&self) -> bool {
        self.zones().iter().any(|z| z.requesting())
    }

    /// True once after an update in which this board started the heater.
    pub fn take_heater_started(&mut self) -> bool {
        core::mem::take(&mut self.started_heater)
    }

    /// Decide every zone from `readings` (each sensor counts for its own zone).
    /// `remote` is the rest of a heater group (thermostats on other boards);
    /// `GroupState::STANDALONE` when there is none. Returns the relay states.
    pub fn update(
        &mut self,
        readings: &[SensorReading],
        hysteresis_dc: u16,
        min_on_ms: u64,
        remote: GroupState,
        now_ms: u64,
    ) -> [bool; MAX_ZONES] {
        let mut out = [false; MAX_ZONES];
        for (i, relay) in out.iter_mut().enumerate().take(self.count) {
            let mut mine = Readings::new();
            for r in readings.iter().filter(|r| r.zone as usize == i) {
                let _ = mine.push(*r);
            }
            let local_others = (0..self.count)
                .filter(|&j| j != i)
                .any(|j| self.zones[j].requesting());
            let group = GroupState {
                others_heating: remote.others_heating || local_others,
                heater_on_since_ms: remote.heater_on_since_ms.max(self.heater_on_since_ms),
            };
            let zone = &mut self.zones[i];
            zone.update(&mine, hysteresis_dc, min_on_ms, group, now_ms);
            // Leaving while a neighbour on this board heats needs no confirmation:
            // we know it keeps the heater on. Releases that depend only on other
            // boards are left to the group messaging.
            if let Some(r) = zone.release_pending()
                && local_others
            {
                zone.released(r, true);
            }
            if zone.take_heater_started() {
                self.heater_on_since_ms = Some(now_ms);
                self.started_heater = true;
            }
            *relay = zone.heating();
        }
        out
    }
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::NoReading => "no sensor reading",
            Reason::BelowBand => "a sensor is below its setpoint",
            Reason::AboveBand => "all sensors above setpoint",
            Reason::InBand => "within hysteresis band",
            Reason::MinOnTime => "keeping the heater on for its minimum on time",
            Reason::Releasing => "checking that another zone keeps the heater on",
            Reason::Boost => "boost",
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

    /// One sensor with setpoint 20.0 °C.
    fn room(t_cc: i32) -> [SensorReading; 1] {
        [reading(1, t_cc, 200)]
    }

    fn reading(id: u8, t_cc: i32, setpoint_dc: i16) -> SensorReading {
        zreading(0, id, t_cc, setpoint_dc)
    }

    fn zreading(zone: u8, id: u8, t_cc: i32, setpoint_dc: i16) -> SensorReading {
        SensorReading {
            addr: [id; 6],
            temperature_cc: t_cc,
            setpoint_dc,
            zone,
            scheduled: false,
        }
    }

    fn sel(id: u8, setpoint_dc: i16) -> TempSensor {
        TempSensor {
            addr: [id; 6],
            setpoint_dc,
            name: Default::default(),
            zone: 0,
        }
    }

    #[test]
    fn fresh_readings_of_selected_sensors() {
        let mut t = BleTable::new();
        t.observe([1; 6], 0, -50, &pvvx(2000), 1_000);
        t.observe([2; 6], 0, -50, &pvvx(2150), 1_000);
        t.observe([3; 6], 0, -50, &pvvx(3000), 1_000); // not selected
        let r = readings(&t, &[sel(1, 200), sel(2, 180)], 2_000);
        assert_eq!(
            r.as_slice(),
            &[reading(1, 2000, 200), reading(2, 2150, 180)]
        );
        assert_eq!(average_dc(&r), Some(208));
        // Unknown or stale sensors are skipped.
        t.observe([2; 6], 0, -50, &pvvx(2150), 1_000 + READING_MAX_AGE_MS);
        let r = readings(
            &t,
            &[sel(1, 200), sel(2, 180), sel(9, 200)],
            1_001 + READING_MAX_AGE_MS,
        );
        assert_eq!(r.as_slice(), &[reading(2, 2150, 180)]);
        assert!(readings(&t, &[], 0).is_empty());
        assert_eq!(average_dc(&[]), None);
        assert_eq!(cc_to_dc(-255), -26);
    }

    #[test]
    fn reading_age_rule_outlives_table_expiry() {
        let mut t = BleTable::new();
        t.observe([1; 6], 0, -50, &pvvx(2000), 0);
        // Nine minutes on (and after the table's periodic expiry), still used.
        let now = 9 * 60_000;
        t.expire(now);
        assert_eq!(readings(&t, &[sel(1, 200)], now).len(), 1);
        assert!(readings(&t, &[sel(1, 200)], READING_MAX_AGE_MS + 1).is_empty());
    }

    #[test]
    fn any_sensor_low_turns_on_all_high_turns_off() {
        // Hysteresis 1.0: each sensor is low at <= setpoint - 0.5, high at >= setpoint + 0.5.
        let s = GroupState::STANDALONE;
        let mut th = Thermostat::new();
        // Living room 21.0 (setpoint 21.0: in band), bedroom 17.6 (setpoint 18.0: in band).
        assert!(!th.update(&[reading(1, 2100, 210), reading(2, 1760, 180)], 10, 0, s, 0));
        assert_eq!(th.reason, Reason::InBand);
        // Bedroom drops to 17.5: one sensor low is enough.
        assert!(th.update(&[reading(1, 2200, 210), reading(2, 1750, 180)], 10, 0, s, 1));
        assert_eq!(th.reason, Reason::BelowBand);
        // Bedroom warm (18.5), living room only in band: keeps heating.
        assert!(th.update(&[reading(1, 2140, 210), reading(2, 1850, 180)], 10, 0, s, 2));
        assert_eq!(th.reason, Reason::InBand);
        // Living room low while the bedroom is well above: heats.
        assert!(th.update(&[reading(1, 2050, 210), reading(2, 2500, 180)], 10, 0, s, 3));
        assert_eq!(th.reason, Reason::BelowBand);
        // Both at or above their setpoint + 0.5: off.
        assert!(!th.update(&[reading(1, 2150, 210), reading(2, 1850, 180)], 10, 0, s, 4));
        assert_eq!(th.reason, Reason::AboveBand);
        // A sensor going stale (missing) leaves the decision to the others.
        assert!(th.update(&[reading(2, 1700, 180)], 10, 0, s, 5));
    }

    #[test]
    fn all_high_still_respects_minimum_on_time() {
        let min = 60_000;
        let s = GroupState::STANDALONE;
        let mut th = Thermostat::new();
        assert!(th.update(
            &[reading(1, 1800, 200), reading(2, 2500, 180)],
            10,
            min,
            s,
            0
        ));
        // Everyone warm after 30 s: held for the rest of the minute.
        assert!(th.update(
            &[reading(1, 2100, 200), reading(2, 2500, 180)],
            10,
            min,
            s,
            30_000
        ));
        assert_eq!(th.reason, Reason::MinOnTime);
        assert!(!th.update(
            &[reading(1, 2100, 200), reading(2, 2500, 180)],
            10,
            min,
            s,
            min
        ));
    }

    #[test]
    fn schedules_replace_fixed_setpoints_when_the_time_is_known() {
        use crate::schedule::{Schedule, Schedules, step_from_dc};
        let mut all = Schedules::new();
        let mut s = Schedule::flat(170);
        s.weekday[7] = step_from_dc(215).unwrap();
        all.set([1; 6], Some(s), |_| true).unwrap();
        let mut r = [reading(1, 2000, 200), reading(2, 2000, 200)];
        apply_schedules(&mut r, &all, None);
        assert_eq!(
            (r[0].setpoint_dc, r[0].scheduled),
            (200, false),
            "no clock yet"
        );
        let friday_7 = crate::clock::DateTime::from_unix(1_792_738_800, 0, false); // 2026-10-23 07:00Z
        assert_eq!((friday_7.weekday, friday_7.hour), (5, 7));
        apply_schedules(&mut r, &all, Some(&friday_7));
        assert_eq!((r[0].setpoint_dc, r[0].scheduled), (215, true));
        assert_eq!(
            (r[1].setpoint_dc, r[1].scheduled),
            (200, false),
            "no schedule"
        );
        // 20.0 against a scheduled 21.5 (hysteresis 1.0): heat.
        let mut th = Thermostat::new();
        assert!(th.update(&r[..1], 10, 0, GroupState::STANDALONE, 0));
    }

    #[test]
    fn boost_heats_for_its_time_then_normal_control_resumes() {
        let hour = 3_600_000;
        let min = 2 * hour;
        let s = GroupState::STANDALONE;
        let mut th = Thermostat::new();
        // Warm room, no heat; a boost turns it on regardless.
        assert!(!th.update(&room(2200), 10, min, s, 0));
        th.boost(1_000, hour);
        assert!(th.update(&room(2200), 10, min, s, 2_000));
        assert_eq!(th.reason, Reason::Boost);
        assert!(th.take_heater_started());
        assert_eq!(th.boost_remaining_ms(2_000), Some(hour - 1_000));
        // Even without sensor readings while it lasts.
        assert!(th.update(&[], 10, min, s, 30 * 60_000));
        // After the hour the minimum on time (2 h, counted from the boost) holds it.
        assert!(th.update(&room(2200), 10, min, s, hour + 1_000));
        assert_eq!(th.reason, Reason::MinOnTime);
        assert_eq!(th.boost_remaining_ms(hour + 1_000), None);
        assert!(
            !th.update(&room(2200), 10, min, s, 2_000 + min),
            "2 h from the heater start"
        );
        // Cancelling ends it at the next update (here the minimum on time is off).
        th.boost(10 * hour, hour);
        assert!(th.update(&room(2200), 10, 0, s, 10 * hour));
        th.cancel_boost();
        assert!(!th.update(&room(2200), 10, 0, s, 10 * hour + 1));
    }

    #[test]
    fn band_per_sensor() {
        let r = |t| reading(1, t, 200);
        assert_eq!(r(1950).band(10), Band::Below);
        assert_eq!(r(1951).band(10), Band::Inside);
        assert_eq!(r(2049).band(10), Band::Inside);
        assert_eq!(r(2050).band(10), Band::Above);
        // Zero hysteresis: at the setpoint counts as low (heat wins).
        assert_eq!(r(2000).band(0), Band::Below);
        assert_eq!(r(2001).band(0), Band::Above);
    }

    #[test]
    fn hysteresis_band() {
        let mut th = Thermostat::new();
        // Setpoint 20.0, hysteresis 1.0: on at <= 19.5, off at >= 20.5.
        assert!(!th.update(&room(1960), 10, 0, GroupState::STANDALONE, 0));
        assert_eq!(th.reason, Reason::InBand);
        assert!(th.update(&room(1950), 10, 0, GroupState::STANDALONE, 0));
        assert!(
            th.update(&room(2040), 10, 0, GroupState::STANDALONE, 1),
            "stays on inside the band"
        );
        assert!(!th.update(&room(2050), 10, 0, GroupState::STANDALONE, 2));
        assert_eq!(th.reason, Reason::AboveBand);
    }

    #[test]
    fn minimum_on_time() {
        let four_h = DEFAULT_MIN_ON_MIN as u64 * 60_000;
        let mut th = Thermostat::new();
        assert!(th.update(&room(1800), 10, four_h, GroupState::STANDALONE, 0));
        assert!(th.take_heater_started());
        // Warm after 3 h: kept on for the rest of the 4 h.
        assert!(th.update(
            &room(2200),
            10,
            four_h,
            GroupState::STANDALONE,
            3 * 3_600_000
        ));
        assert_eq!(th.reason, Reason::MinOnTime);
        assert_eq!(th.hold_remaining_ms(3 * 3_600_000), Some(3_600_000));
        assert!(!th.update(&room(2200), 10, four_h, GroupState::STANDALONE, four_h));
        assert_eq!(th.hold_remaining_ms(four_h), None);
    }

    #[test]
    fn heat_is_never_delayed() {
        let four_h = DEFAULT_MIN_ON_MIN as u64 * 60_000;
        let mut th = Thermostat::new();
        assert!(th.update(&room(1800), 10, four_h, GroupState::STANDALONE, 0));
        assert!(!th.update(&room(2200), 10, four_h, GroupState::STANDALONE, four_h));
        // Cold again a minute after switching off: heats at once.
        assert!(th.update(
            &room(1800),
            10,
            four_h,
            GroupState::STANDALONE,
            four_h + 60_000
        ));
        // Even right after the group's heater just started or stopped.
        let mut other = Thermostat::new();
        let busy = GroupState {
            others_heating: true,
            heater_on_since_ms: Some(five_s()),
        };
        assert!(other.update(&room(1800), 10, four_h, busy, five_s()));
        assert!(!other.take_heater_started(), "joined a running heater");
    }

    fn five_s() -> u64 {
        5_000
    }

    #[test]
    fn group_minimum_on_time() {
        let min = 2 * 60_000;
        let busy = |since| GroupState {
            others_heating: true,
            heater_on_since_ms: Some(since),
        };
        let alone = |since| GroupState {
            others_heating: false,
            heater_on_since_ms: Some(since),
        };
        let mut th = Thermostat::new();
        // Another zone started the heater at t=0; we join at 10 s.
        assert!(th.update(&room(1800), 10, min, busy(0), 10_000));
        // Leaving while the other zone still heats: released once it confirms.
        assert!(th.update(&room(2200), 10, min, busy(0), 20_000));
        let r = th.release_pending().unwrap();
        th.released(r, true);
        assert!(!th.heating());
        // Join again, then the other zone leaves: we are the last zone, so the
        // heater stays on until 2 min after it started (t=0), not after we joined.
        assert!(th.update(&room(1800), 10, min, busy(0), 30_000));
        assert!(th.update(&room(2200), 10, min, alone(0), 60_000));
        assert_eq!(th.hold_remaining_ms(60_000), Some(60_000));
        assert!(!th.update(&room(2200), 10, min, alone(0), min));
        // If the start time is unknown, our own switch-on time is used.
        let mut late = Thermostat::new();
        let unknown = GroupState {
            others_heating: true,
            heater_on_since_ms: None,
        };
        assert!(late.update(&room(1800), 10, min, unknown, 50_000));
        assert!(late.update(&room(2200), 10, min, GroupState::STANDALONE, 60_000));
        assert_eq!(late.hold_remaining_ms(60_000), Some(110_000));
    }

    #[test]
    fn release_waits_for_confirmation() {
        let min = 2 * 60_000;
        let busy = GroupState {
            others_heating: true,
            heater_on_since_ms: Some(0),
        };
        let mut th = Thermostat::new();
        assert!(th.update(&room(1800), 10, min, busy, 0));
        assert!(th.requesting());
        // Warm: stops requesting but keeps the output on while it asks.
        assert!(th.update(&room(2200), 10, min, busy, 10_000));
        assert_eq!(th.reason, Reason::Releasing);
        assert!(th.heating() && !th.requesting());
        let r = th.release_pending().unwrap();
        // Later updates keep the same release (no new request per loop),
        // also when the temperature dips back into the band.
        assert!(th.update(&room(2000), 10, min, busy, 15_000));
        assert_eq!(th.release_pending(), Some(r));
        // An answer to an older release does nothing.
        th.released(r.wrapping_sub(1), true);
        assert!(th.heating());
        th.released(r, true);
        assert!(!th.heating() && th.release_pending().is_none());
        // Off, inside the band: stays off.
        assert!(!th.update(&room(2000), 10, min, busy, 20_000));
    }

    #[test]
    fn simultaneous_release_holds_the_heater() {
        // Zones A and B both heat; both get warm before hearing from each other.
        let min = 2 * 60_000;
        let both = GroupState {
            others_heating: true,
            heater_on_since_ms: Some(0),
        };
        let (mut a, mut b) = (Thermostat::new(), Thermostat::new());
        assert!(a.update(&room(1800), 10, min, both, 0));
        assert!(b.update(&room(1800), 10, min, both, 0));
        assert!(a.update(&room(2200), 10, min, both, 10_000));
        assert!(b.update(&room(2200), 10, min, both, 10_000));
        // Each asks the other, which answers heat=0: neither is confirmed.
        assert!(!a.requesting() && !b.requesting());
        a.released(a.release_pending().unwrap(), false);
        b.released(b.release_pending().unwrap(), false);
        // Each now knows the other is not requesting: both hold the heater.
        let alone = GroupState {
            others_heating: false,
            heater_on_since_ms: Some(0),
        };
        assert!(a.update(&room(2200), 10, min, alone, 11_000));
        assert!(b.update(&room(2200), 10, min, alone, 11_000));
        assert_eq!((a.reason, b.reason), (Reason::MinOnTime, Reason::MinOnTime));
        assert!(a.requesting() && b.requesting());
        // The minimum on time over, both switch off without asking.
        assert!(!a.update(&room(2200), 10, min, both, min));
        assert!(!b.update(&room(2200), 10, min, both, min));
    }

    #[test]
    fn release_cancelled_by_demand_or_group() {
        let min = 2 * 60_000;
        let busy = GroupState {
            others_heating: true,
            heater_on_since_ms: Some(0),
        };
        let mut th = Thermostat::new();
        assert!(th.update(&room(1800), 10, min, busy, 0));
        assert!(th.update(&room(2200), 10, min, busy, 1_000));
        let r = th.release_pending().unwrap();
        // Cold again before the answer: keeps heating, the late answer is ignored.
        assert!(th.update(&room(1800), 10, min, busy, 2_000));
        assert!(th.requesting());
        th.released(r, true);
        assert!(th.heating());
        // Release again; meanwhile the other zone's own release arrives, so we
        // become the last zone and hold instead of waiting.
        assert!(th.update(&room(2200), 10, min, busy, 3_000));
        assert!(th.release_pending().is_some());
        assert!(th.update(&room(2200), 10, min, GroupState::STANDALONE, 4_000));
        assert_eq!(th.reason, Reason::MinOnTime);
        assert!(th.release_pending().is_none() && th.requesting());
    }

    /// Zone 0 at `t0`, zone 1 at `t1` (hundredths); setpoints 20.0, hysteresis 1.0.
    fn two(t0: i32, t1: i32) -> [SensorReading; 2] {
        [zreading(0, 1, t0, 200), zreading(1, 2, t1, 200)]
    }

    const ALONE: GroupState = GroupState::STANDALONE;

    #[test]
    fn zones_switch_independently() {
        let mut c = Controller::new(2);
        assert_eq!(c.update(&two(2000, 2000), 10, 0, ALONE, 0), [false, false]);
        assert_eq!(c.update(&two(1900, 2000), 10, 0, ALONE, 1), [true, false]);
        assert!(c.take_heater_started());
        assert_eq!(c.update(&two(1900, 1900), 10, 0, ALONE, 2), [true, true]);
        assert!(!c.take_heater_started(), "zone 1 joined a running heater");
        assert_eq!(c.update(&two(2100, 1900), 10, 0, ALONE, 3), [false, true]);
        assert_eq!(c.update(&two(2100, 2100), 10, 0, ALONE, 4), [false, false]);
        // A zone with no fresh reading is off; the other carries on.
        assert_eq!(
            c.update(&[zreading(1, 2, 1900, 200)], 10, 0, ALONE, 5),
            [false, true]
        );
        assert_eq!(c.zones()[0].reason, Reason::NoReading);
    }

    #[test]
    fn minimum_on_time_covers_the_heater_across_zones() {
        let min = 120_000;
        let mut c = Controller::new(2);
        // Zone 0 starts the heater at t=0; zone 1 joins at 10 s.
        assert_eq!(c.update(&two(1900, 2000), 10, min, ALONE, 0), [true, false]);
        assert_eq!(
            c.update(&two(1900, 1900), 10, min, ALONE, 10_000),
            [true, true]
        );
        // Zone 0 warm at 20 s: zone 1 keeps the heater on, so zone 0 leaves at once.
        assert_eq!(
            c.update(&two(2100, 1900), 10, min, ALONE, 20_000),
            [false, true]
        );
        assert_eq!(c.zones()[0].reason, Reason::AboveBand);
        // Zone 1 warm at 30 s: it is last, held until 2 min after the heater started.
        assert_eq!(
            c.update(&two(2100, 2100), 10, min, ALONE, 30_000),
            [false, true]
        );
        assert_eq!(c.zones()[1].reason, Reason::MinOnTime);
        assert_eq!(c.zones()[1].hold_remaining_ms(30_000), Some(90_000));
        assert_eq!(
            c.update(&two(2100, 2100), 10, min, ALONE, min),
            [false, false]
        );
        // Heat is never delayed: zone 0 restarts the heater straight away.
        assert_eq!(
            c.update(&two(1900, 2100), 10, min, ALONE, min + 1),
            [true, false]
        );
        assert!(c.take_heater_started());
    }

    #[test]
    fn both_zones_warm_together_hold_one_relay() {
        let min = 120_000;
        let mut c = Controller::new(2);
        assert_eq!(c.update(&two(1900, 1900), 10, min, ALONE, 0), [true, true]);
        // Both warm in the same step: one zone leaves, the other holds the heater.
        assert_eq!(
            c.update(&two(2100, 2100), 10, min, ALONE, 10_000),
            [false, true]
        );
        assert_eq!(c.zones()[1].reason, Reason::MinOnTime);
        assert!(c.zones().iter().all(|z| z.release_pending().is_none()));
        // The held zone wanting heat again simply carries on.
        assert_eq!(
            c.update(&two(2100, 1900), 10, min, ALONE, 20_000),
            [false, true]
        );
        assert_eq!(c.zones()[1].reason, Reason::BelowBand);
    }

    #[test]
    fn hold_counts_from_the_heater_start_not_the_last_zone() {
        let min = 120_000;
        let mut c = Controller::new(2);
        assert_eq!(c.update(&two(1900, 2000), 10, min, ALONE, 0), [true, false]);
        // Zone 1 joins late (100 s) and is the last to leave (110 s): held only
        // until 120 s, the heater having started at 0.
        assert_eq!(
            c.update(&two(1900, 1900), 10, min, ALONE, 100_000),
            [true, true]
        );
        assert_eq!(
            c.update(&two(2100, 1900), 10, min, ALONE, 105_000),
            [false, true]
        );
        assert_eq!(
            c.update(&two(2100, 2100), 10, min, ALONE, 110_000),
            [false, true]
        );
        assert_eq!(c.zones()[1].hold_remaining_ms(110_000), Some(10_000));
    }

    #[test]
    fn one_zone_controller_matches_a_thermostat() {
        let min = 60_000;
        let mut c = Controller::new(1);
        assert_eq!(c.zones().len(), 1);
        assert_eq!(c.update(&room(1800), 10, min, ALONE, 0), [true, false]);
        assert_eq!(c.update(&room(2200), 10, min, ALONE, 30_000), [true, false]);
        assert_eq!(c.zones()[0].reason, Reason::MinOnTime);
        // In a heater group, leaving early still waits for the group to confirm.
        let busy = GroupState {
            others_heating: true,
            heater_on_since_ms: Some(0),
        };
        assert_eq!(c.update(&room(2200), 10, min, busy, 40_000), [true, false]);
        let r = c.zones()[0].release_pending().unwrap();
        c.zones_mut()[0].released(r, true);
        assert!(!c.heating());
    }

    #[test]
    fn fails_safe_without_readings() {
        let mut th = Thermostat::new();
        assert!(th.update(&room(1800), 10, 3_600_000, GroupState::STANDALONE, 0));
        // Losing the sensor switches off immediately, despite the minimum on time.
        assert!(!th.update(&[], 10, 3_600_000, GroupState::STANDALONE, 1_000));
        assert_eq!(th.reason, Reason::NoReading);
    }
}
