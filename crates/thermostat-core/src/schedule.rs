//! Per-sensor setpoint schedules: one setpoint per hour, one row for weekdays
//! (Monday-Friday) and one for weekends, in 0.5 °C steps.
//!
//! Each hour is one byte: `v` means `5.0 + v × 0.5` °C, so 0..=55 covers
//! 5.0-32.5 °C. Schedules are keyed by sensor address and stored apart from the
//! settings (see `store`), and the device keeps the only copy in RAM; the web
//! pages read and write one sensor's schedule at a time.

use heapless::Vec;

use crate::clock::DateTime;
use crate::settings::MAX_TEMP_SENSORS;

pub const HOURS: usize = 24;
/// Lowest and highest schedulable setpoints, tenths of °C.
pub const MIN_DC: i16 = 50;
pub const MAX_DC: i16 = 325;
const STEP_DC: i16 = 5;
const MAX_STEP: u8 = ((MAX_DC - MIN_DC) / STEP_DC) as u8;

/// Bytes per stored entry: address and both rows.
const ENTRY_LEN: usize = 6 + 2 * HOURS;
/// Largest encoded size of [`Schedules`].
pub const MAX_ENCODED: usize = ENTRY_LEN * MAX_TEMP_SENSORS;

/// A setpoint (tenths of °C) as a schedule step, if it is on the 0.5 °C grid
/// within range.
pub fn step_from_dc(dc: i16) -> Option<u8> {
    ((MIN_DC..=MAX_DC).contains(&dc) && dc % STEP_DC == 0).then(|| ((dc - MIN_DC) / STEP_DC) as u8)
}

pub fn dc_from_step(step: u8) -> i16 {
    MIN_DC + step.min(MAX_STEP) as i16 * STEP_DC
}

/// The nearest schedule step to a setpoint (for filling a new schedule).
pub fn nearest_step(dc: i16) -> u8 {
    let dc = dc.clamp(MIN_DC, MAX_DC);
    ((dc - MIN_DC + STEP_DC / 2) / STEP_DC) as u8
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Monday-Friday, hour 0..23 (local time).
    pub weekday: [u8; HOURS],
    /// Saturday and Sunday.
    pub weekend: [u8; HOURS],
}

impl Schedule {
    /// Every hour at `setpoint_dc` (rounded to the 0.5 °C grid).
    pub fn flat(setpoint_dc: i16) -> Self {
        let s = nearest_step(setpoint_dc);
        Self {
            weekday: [s; HOURS],
            weekend: [s; HOURS],
        }
    }

    /// The setpoint (tenths of °C) at a local time.
    pub fn setpoint_dc(&self, at: &DateTime) -> i16 {
        let weekend = at.weekday == 0 || at.weekday == 6;
        let row = if weekend {
            &self.weekend
        } else {
            &self.weekday
        };
        dc_from_step(row[at.hour as usize % HOURS])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleError {
    /// Every slot holds a schedule for a selected sensor.
    Full,
}

/// The schedules of the selected sensors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedules {
    entries: Vec<([u8; 6], Schedule), MAX_TEMP_SENSORS>,
}

impl Schedules {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn get(&self, addr: &[u8; 6]) -> Option<&Schedule> {
        self.entries.iter().find(|(a, _)| a == addr).map(|(_, s)| s)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Set (or with `None` remove) a sensor's schedule. Schedules of sensors
    /// for which `keep` is false (no longer selected) are dropped first.
    pub fn set(
        &mut self,
        addr: [u8; 6],
        schedule: Option<Schedule>,
        keep: impl Fn(&[u8; 6]) -> bool,
    ) -> Result<(), ScheduleError> {
        self.entries.retain(|(a, _)| *a == addr || keep(a));
        match (self.entries.iter().position(|(a, _)| *a == addr), schedule) {
            (Some(i), Some(s)) => self.entries[i].1 = s,
            (Some(i), None) => {
                self.entries.swap_remove(i);
            }
            (None, Some(s)) => self
                .entries
                .push((addr, s))
                .map_err(|_| ScheduleError::Full)?,
            (None, None) => {}
        }
        Ok(())
    }

    /// Encode into `out` (at least [`MAX_ENCODED`] bytes); returns the length.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let mut n = 0;
        for (addr, s) in &self.entries {
            out[n..n + 6].copy_from_slice(addr);
            out[n + 6..n + 6 + HOURS].copy_from_slice(&s.weekday);
            out[n + 6 + HOURS..n + ENTRY_LEN].copy_from_slice(&s.weekend);
            n += ENTRY_LEN;
        }
        n
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if !data.len().is_multiple_of(ENTRY_LEN) {
            return None;
        }
        let mut out = Self::new();
        for e in data.as_chunks::<ENTRY_LEN>().0 {
            if e[6..].iter().any(|&v| v > MAX_STEP) {
                return None;
            }
            let s = Schedule {
                weekday: e[6..6 + HOURS].try_into().unwrap(),
                weekend: e[6 + HOURS..].try_into().unwrap(),
            };
            out.entries.push((e[..6].try_into().unwrap(), s)).ok()?;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(weekday: u32, hour: u32) -> DateTime {
        DateTime {
            year: 2026,
            month: 10,
            day: 1,
            hour,
            minute: 30,
            second: 0,
            weekday,
            offset_s: 0,
            dst: false,
        }
    }

    #[test]
    fn steps() {
        assert_eq!(step_from_dc(50), Some(0));
        assert_eq!(step_from_dc(205), Some(31));
        assert_eq!(step_from_dc(325), Some(55));
        assert_eq!(step_from_dc(207), None, "off the 0.5 grid");
        assert_eq!(step_from_dc(45), None);
        assert_eq!(step_from_dc(330), None);
        assert_eq!(dc_from_step(31), 205);
        assert_eq!(dc_from_step(200), 325, "clamped");
        assert_eq!(nearest_step(212), 32); // 21.2 -> 21.0
        assert_eq!(nearest_step(213), 33); // 21.3 -> 21.5
        assert_eq!(nearest_step(0), 0);
    }

    #[test]
    fn setpoint_by_day_and_hour() {
        let mut s = Schedule::flat(180);
        s.weekday[7] = step_from_dc(210).unwrap();
        s.weekend[9] = step_from_dc(220).unwrap();
        assert_eq!(s.setpoint_dc(&at(1, 7)), 210, "Monday 07:30");
        assert_eq!(s.setpoint_dc(&at(5, 7)), 210, "Friday");
        assert_eq!(
            s.setpoint_dc(&at(6, 7)),
            180,
            "Saturday uses the weekend row"
        );
        assert_eq!(s.setpoint_dc(&at(0, 9)), 220, "Sunday 09:30");
        assert_eq!(s.setpoint_dc(&at(3, 9)), 180);
    }

    #[test]
    fn set_replace_remove_and_prune() {
        let mut all = Schedules::new();
        let yes = |_: &[u8; 6]| true;
        for i in 0..MAX_TEMP_SENSORS as u8 {
            all.set([i; 6], Some(Schedule::flat(200)), yes).unwrap();
        }
        assert_eq!(
            all.set([99; 6], Some(Schedule::flat(200)), yes),
            Err(ScheduleError::Full)
        );
        // Replacing in place works when full.
        all.set([3; 6], Some(Schedule::flat(190)), yes).unwrap();
        assert_eq!(all.get(&[3; 6]).unwrap().weekday[0], nearest_step(190));
        // Sensors no longer selected make room.
        all.set([99; 6], Some(Schedule::flat(200)), |a| a[0] < 4)
            .unwrap();
        assert_eq!(all.len(), 5);
        all.set([99; 6], None, yes).unwrap();
        assert!(all.get(&[99; 6]).is_none());
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn encode_decode() {
        let mut all = Schedules::new();
        let mut s = Schedule::flat(200);
        s.weekend[23] = 55;
        all.set([1, 2, 3, 4, 5, 6], Some(s), |_| true).unwrap();
        all.set([7; 6], Some(Schedule::flat(50)), |_| true).unwrap();
        let mut buf = [0u8; MAX_ENCODED];
        let n = all.encode(&mut buf);
        assert_eq!(n, 2 * ENTRY_LEN);
        assert_eq!(Schedules::decode(&buf[..n]), Some(all));
        assert_eq!(Schedules::decode(&buf[..n - 1]), None);
        buf[10] = 56; // out of range
        assert_eq!(Schedules::decode(&buf[..n]), None);
        assert_eq!(Schedules::decode(&[]), Some(Schedules::new()));
    }
}
