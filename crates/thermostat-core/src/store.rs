//! Power-fail-safe, wear-levelled record storage on NOR flash.
//!
//! Each save writes one record into the next sector of the region
//! (round-robin), so erases are spread over all sectors and the previous
//! record stays intact until the new one is complete. Loading picks the valid
//! record with the highest sequence number.
//!
//! Record layout (word aligned):
//! `magic u32 | seq u32 | len u32 | payload (padded to 4) | crc32 u32`
//!
//! The settings partition is split in two halves: settings records (magic
//! `STG1`) in the first, schedule records (`SCH1`, see `schedule`) in the
//! second. Firmware before the split spread settings over the whole partition;
//! [`load_split`] moves the newest such record into the first half.

use embedded_storage::nor_flash::NorFlash;

use crate::schedule::{self, Schedules};
use crate::settings::{self, Settings};

const SETTINGS_MAGIC: u32 = u32::from_le_bytes(*b"STG1");
const SCHEDULE_MAGIC: u32 = u32::from_le_bytes(*b"SCH1");
const HEADER: usize = 12;
const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// Scratch space for one record of either kind.
const MAX_PAYLOAD: usize = if settings::MAX_ENCODED > schedule::MAX_ENCODED {
    settings::MAX_ENCODED
} else {
    schedule::MAX_ENCODED
};
const BUF: usize = HEADER + MAX_PAYLOAD + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Flash,
    /// The region is smaller than two sectors.
    TooSmall,
}

/// A record log in one flash region.
struct Log {
    magic: u32,
    sectors: u32,
    sector_size: u32,
    /// Sector holding the newest record, if any.
    current: Option<u32>,
    seq: u32,
}

impl Log {
    /// Scan the region; `accept` sees each valid record (sequence number and
    /// payload) and returns whether it decodes. The newest accepted record wins.
    fn scan<F: NorFlash>(
        flash: &mut F,
        magic: u32,
        max_len: usize,
        mut accept: impl FnMut(u32, &[u8]) -> bool,
    ) -> Result<Self, StoreError> {
        let sector_size = F::ERASE_SIZE as u32;
        let sectors = flash.capacity() as u32 / sector_size;
        if sectors < 2 {
            return Err(StoreError::TooSmall);
        }
        let mut log = Self {
            magic,
            sectors,
            sector_size,
            current: None,
            seq: 0,
        };
        let mut buf = [0u8; BUF];
        for sector in 0..sectors {
            if let Some((seq, len)) =
                read_record(flash, magic, max_len, sector * sector_size, &mut buf)?
                && (log.current.is_none() || seq > log.seq)
                && accept(seq, &buf[HEADER..HEADER + len])
            {
                log.current = Some(sector);
                log.seq = seq;
            }
        }
        Ok(log)
    }

    /// Write `buf[HEADER..HEADER + len]` (the payload, already in place) as the
    /// next record.
    fn write<F: NorFlash>(
        &mut self,
        flash: &mut F,
        buf: &mut [u8; BUF],
        len: usize,
    ) -> Result<(), StoreError> {
        let padded = (len + 3) & !3;
        let seq = self.seq.wrapping_add(1);
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..8].copy_from_slice(&seq.to_le_bytes());
        buf[8..12].copy_from_slice(&(len as u32).to_le_bytes());
        buf[HEADER + len..HEADER + padded].fill(0xFF);
        let crc = CRC.checksum(&buf[..HEADER + padded]);
        buf[HEADER + padded..HEADER + padded + 4].copy_from_slice(&crc.to_le_bytes());

        let sector = self.current.map_or(0, |s| (s + 1) % self.sectors);
        let at = sector * self.sector_size;
        flash
            .erase(at, at + self.sector_size)
            .map_err(|_| StoreError::Flash)?;
        flash
            .write(at, &buf[..HEADER + padded + 4])
            .map_err(|_| StoreError::Flash)?;
        self.current = Some(sector);
        self.seq = seq;
        Ok(())
    }

    fn wipe<F: NorFlash>(&mut self, flash: &mut F) -> Result<(), StoreError> {
        flash
            .erase(0, self.sectors * self.sector_size)
            .map_err(|_| StoreError::Flash)?;
        self.current = None;
        self.seq = 0;
        Ok(())
    }
}

pub struct SettingsStore(Log);

impl SettingsStore {
    /// Scan the region and return the store plus the newest valid settings.
    pub fn load<F: NorFlash>(flash: &mut F) -> Result<(Self, Option<Settings>), StoreError> {
        let mut best = None;
        let log =
            Log::scan(
                flash,
                SETTINGS_MAGIC,
                settings::MAX_ENCODED,
                |_, p| match Settings::decode(p) {
                    Ok(s) => {
                        best = Some(s);
                        true
                    }
                    Err(_) => false,
                },
            )?;
        Ok((Self(log), best))
    }

    pub fn save<F: NorFlash>(
        &mut self,
        flash: &mut F,
        settings: &Settings,
    ) -> Result<(), StoreError> {
        let mut buf = [0u8; BUF];
        let len = settings.encode(&mut buf[HEADER..HEADER + settings::MAX_ENCODED]);
        self.0.write(flash, &mut buf, len)
    }

    /// Erase every record (factory reset).
    pub fn wipe<F: NorFlash>(&mut self, flash: &mut F) -> Result<(), StoreError> {
        self.0.wipe(flash)
    }
}

/// Load the settings from the first half of the partition (`own`), moving the
/// newest record there if it is in the second half (`other`, written by
/// firmware from before the split). The moved copy gets a higher sequence
/// number than anything left behind.
pub fn load_split<F: NorFlash>(
    own: &mut F,
    other: &mut F,
) -> Result<(SettingsStore, Option<Settings>), StoreError> {
    let (mut store, mine) = SettingsStore::load(own)?;
    let (theirs, newer) = SettingsStore::load(other)?;
    let newer_seq = theirs.0.current.map(|_| theirs.0.seq);
    match (newer, newer_seq) {
        (Some(s), Some(seq)) if store.0.current.is_none() || seq > store.0.seq => {
            store.0.seq = seq;
            store.save(own, &s)?;
            Ok((store, Some(s)))
        }
        _ => Ok((store, mine)),
    }
}

pub struct ScheduleStore(Log);

impl ScheduleStore {
    /// Scan the region and return the store plus the newest schedules (empty
    /// if none were saved yet).
    pub fn load<F: NorFlash>(flash: &mut F) -> Result<(Self, Schedules), StoreError> {
        let mut best = Schedules::new();
        let log =
            Log::scan(
                flash,
                SCHEDULE_MAGIC,
                schedule::MAX_ENCODED,
                |_, p| match Schedules::decode(p) {
                    Some(s) => {
                        best = s;
                        true
                    }
                    None => false,
                },
            )?;
        Ok((Self(log), best))
    }

    pub fn save<F: NorFlash>(
        &mut self,
        flash: &mut F,
        schedules: &Schedules,
    ) -> Result<(), StoreError> {
        let mut buf = [0u8; BUF];
        let len = schedules.encode(&mut buf[HEADER..HEADER + schedule::MAX_ENCODED]);
        self.0.write(flash, &mut buf, len)
    }
}

/// Read and check the record at `at`; returns its sequence number and payload
/// length (the payload is left in `buf[HEADER..]`).
fn read_record<F: NorFlash>(
    flash: &mut F,
    magic: u32,
    max_len: usize,
    at: u32,
    buf: &mut [u8; BUF],
) -> Result<Option<(u32, usize)>, StoreError> {
    flash
        .read(at, &mut buf[..HEADER])
        .map_err(|_| StoreError::Flash)?;
    let word = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if word(buf, 0) != magic {
        return Ok(None);
    }
    let seq = word(buf, 4);
    let len = word(buf, 8) as usize;
    if len > max_len {
        return Ok(None);
    }
    let padded = (len + 3) & !3;
    flash
        .read(at + HEADER as u32, &mut buf[HEADER..HEADER + padded + 4])
        .map_err(|_| StoreError::Flash)?;
    let crc = word(buf, HEADER + padded);
    if CRC.checksum(&buf[..HEADER + padded]) != crc {
        return Ok(None);
    }
    Ok(Some((seq, len)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::settings::str32;
    use embedded_storage::nor_flash::{ErrorType, NorFlashError, NorFlashErrorKind, ReadNorFlash};

    pub struct MemFlash {
        pub data: std::vec::Vec<u8>,
        pub erases: std::vec::Vec<u32>,
        /// Fail writes after this many more bytes (simulated power loss).
        pub write_budget: Option<usize>,
    }

    impl MemFlash {
        pub fn new(size: usize) -> Self {
            Self {
                data: std::vec![0xFF; size],
                erases: std::vec![0; size / 4096],
                write_budget: None,
            }
        }
    }

    #[derive(Debug)]
    pub struct FlashErr;
    impl NorFlashError for FlashErr {
        fn kind(&self) -> NorFlashErrorKind {
            NorFlashErrorKind::Other
        }
    }
    impl ErrorType for MemFlash {
        type Error = FlashErr;
    }
    impl ReadNorFlash for MemFlash {
        const READ_SIZE: usize = 4;
        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), FlashErr> {
            assert!(offset.is_multiple_of(4) && bytes.len().is_multiple_of(4));
            bytes.copy_from_slice(&self.data[offset as usize..offset as usize + bytes.len()]);
            Ok(())
        }
        fn capacity(&self) -> usize {
            self.data.len()
        }
    }
    impl NorFlash for MemFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 4096;
        fn erase(&mut self, from: u32, to: u32) -> Result<(), FlashErr> {
            for s in from / 4096..to / 4096 {
                self.erases[s as usize] += 1;
            }
            self.data[from as usize..to as usize].fill(0xFF);
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), FlashErr> {
            assert!(offset.is_multiple_of(4) && bytes.len().is_multiple_of(4));
            for (i, b) in bytes.iter().enumerate() {
                if let Some(budget) = self.write_budget.as_mut() {
                    if *budget == 0 {
                        return Err(FlashErr);
                    }
                    *budget -= 1;
                }
                self.data[offset as usize + i] &= b;
            }
            Ok(())
        }
    }

    fn named(n: &str) -> Settings {
        Settings {
            hostname: str32(n),
            ..Settings::default()
        }
    }

    #[test]
    fn empty_then_save_and_reload() {
        let mut f = MemFlash::new(4 * 4096);
        let (mut store, s) = SettingsStore::load(&mut f).unwrap();
        assert!(s.is_none());
        store.save(&mut f, &named("a")).unwrap();
        store.save(&mut f, &named("b")).unwrap();
        let (_, s) = SettingsStore::load(&mut f).unwrap();
        assert_eq!(s.unwrap().hostname, "b");
    }

    #[test]
    fn wear_is_spread() {
        let mut f = MemFlash::new(4 * 4096);
        let (mut store, _) = SettingsStore::load(&mut f).unwrap();
        for i in 0..40 {
            store.save(&mut f, &named(&std::format!("n{i}"))).unwrap();
        }
        assert_eq!(f.erases, std::vec![10, 10, 10, 10]);
        let (mut store, s) = SettingsStore::load(&mut f).unwrap();
        assert_eq!(s.unwrap().hostname, "n39");
        // Continues the rotation after a reload.
        store.save(&mut f, &named("next")).unwrap();
        assert_eq!(f.erases, std::vec![11, 10, 10, 10]);
    }

    #[test]
    fn interrupted_save_keeps_previous() {
        let mut f = MemFlash::new(2 * 4096);
        let (mut store, _) = SettingsStore::load(&mut f).unwrap();
        store.save(&mut f, &named("good")).unwrap();
        f.write_budget = Some(20);
        assert!(store.save(&mut f, &named("torn")).is_err());
        f.write_budget = None;
        let (_, s) = SettingsStore::load(&mut f).unwrap();
        assert_eq!(s.unwrap().hostname, "good");
    }

    #[test]
    fn wipe_resets() {
        let mut f = MemFlash::new(2 * 4096);
        let (mut store, _) = SettingsStore::load(&mut f).unwrap();
        store.save(&mut f, &named("x")).unwrap();
        store.wipe(&mut f).unwrap();
        assert!(SettingsStore::load(&mut f).unwrap().1.is_none());
    }

    #[test]
    fn settings_move_out_of_the_schedule_half() {
        // Old firmware: one store over the whole (here 8-sector) partition.
        let mut whole = MemFlash::new(8 * 4096);
        let (mut old, _) = SettingsStore::load(&mut whole).unwrap();
        for i in 0..6 {
            old.save(&mut whole, &named(&std::format!("v{i}"))).unwrap();
        }
        // The newest ("v5") sits in sector 5: the second half.
        let split = |w: &MemFlash| {
            let mut lo = MemFlash::new(4 * 4096);
            let mut hi = MemFlash::new(4 * 4096);
            lo.data.copy_from_slice(&w.data[..4 * 4096]);
            hi.data.copy_from_slice(&w.data[4 * 4096..]);
            (lo, hi)
        };
        let (mut lo, mut hi) = split(&whole);
        let (mut store, s) = load_split(&mut lo, &mut hi).unwrap();
        assert_eq!(s.unwrap().hostname, "v5");
        // It now lives in the first half, newer than anything in the second.
        let (_, again) = SettingsStore::load(&mut lo).unwrap();
        assert_eq!(again.unwrap().hostname, "v5");
        store.save(&mut lo, &named("after")).unwrap();
        let (_, s) = load_split(&mut lo, &mut hi).unwrap();
        assert_eq!(s.unwrap().hostname, "after", "old records stay superseded");
        // Schedules in the second half ignore the stale settings records.
        let (mut sched, none) = ScheduleStore::load(&mut hi).unwrap();
        assert!(none.is_empty());
        let mut all = Schedules::new();
        all.set([1; 6], Some(crate::schedule::Schedule::flat(195)), |_| true)
            .unwrap();
        sched.save(&mut hi, &all).unwrap();
        assert_eq!(ScheduleStore::load(&mut hi).unwrap().1, all);
        let (_, s) = load_split(&mut lo, &mut hi).unwrap();
        assert_eq!(s.unwrap().hostname, "after");
    }

    #[test]
    fn newest_settings_already_in_first_half_stay() {
        let mut lo = MemFlash::new(4 * 4096);
        let mut hi = MemFlash::new(4 * 4096);
        let (mut store, _) = SettingsStore::load(&mut lo).unwrap();
        store.save(&mut lo, &named("here")).unwrap();
        let erases = lo.erases.clone();
        let (_, s) = load_split(&mut lo, &mut hi).unwrap();
        assert_eq!(s.unwrap().hostname, "here");
        assert_eq!(lo.erases, erases, "nothing rewritten");
    }

    #[test]
    fn schedules_survive_reload_and_torn_writes() {
        let mut f = MemFlash::new(2 * 4096);
        let (mut store, s) = ScheduleStore::load(&mut f).unwrap();
        assert!(s.is_empty());
        let mut all = Schedules::new();
        for i in 0..crate::settings::MAX_TEMP_SENSORS as u8 {
            all.set([i; 6], Some(crate::schedule::Schedule::flat(200)), |_| true)
                .unwrap();
        }
        store.save(&mut f, &all).unwrap();
        let mut fewer = all.clone();
        fewer.set([0; 6], None, |_| true).unwrap();
        f.write_budget = Some(40);
        assert!(store.save(&mut f, &fewer).is_err());
        f.write_budget = None;
        assert_eq!(ScheduleStore::load(&mut f).unwrap().1, all);
    }
}
