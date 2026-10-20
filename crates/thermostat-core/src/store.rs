//! Power-fail-safe, wear-levelled settings storage on NOR flash.
//!
//! Each save writes one record into the next sector of the partition
//! (round-robin), so erases are spread over all sectors and the previous
//! record stays intact until the new one is complete. Loading picks the valid
//! record with the highest sequence number.
//!
//! Record layout (word aligned):
//! `magic u32 | seq u32 | len u32 | payload (padded to 4) | crc32 u32`

use embedded_storage::nor_flash::NorFlash;

use crate::settings::{MAX_ENCODED, Settings};

const MAGIC: u32 = u32::from_le_bytes(*b"STG1");
const HEADER: usize = 12;
const CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Flash,
    /// The partition is smaller than two sectors.
    TooSmall,
}

pub struct SettingsStore {
    sectors: u32,
    sector_size: u32,
    /// Sector holding the newest record, if any.
    current: Option<u32>,
    seq: u32,
}

impl SettingsStore {
    /// Scan the partition and return the store plus the newest valid settings.
    pub fn load<F: NorFlash>(flash: &mut F) -> Result<(Self, Option<Settings>), StoreError> {
        let sector_size = F::ERASE_SIZE as u32;
        let sectors = flash.capacity() as u32 / sector_size;
        if sectors < 2 {
            return Err(StoreError::TooSmall);
        }
        let mut store = Self {
            sectors,
            sector_size,
            current: None,
            seq: 0,
        };
        let mut best: Option<Settings> = None;
        let mut buf = [0u8; HEADER + MAX_ENCODED + 4];
        for sector in 0..sectors {
            if let Some((seq, settings)) = read_record(flash, sector * sector_size, &mut buf)?
                && (store.current.is_none() || seq > store.seq)
            {
                store.current = Some(sector);
                store.seq = seq;
                best = Some(settings);
            }
        }
        Ok((store, best))
    }

    pub fn save<F: NorFlash>(
        &mut self,
        flash: &mut F,
        settings: &Settings,
    ) -> Result<(), StoreError> {
        let mut buf = [0u8; HEADER + MAX_ENCODED + 4];
        let len = settings.encode(&mut buf[HEADER..HEADER + MAX_ENCODED]);
        let padded = (len + 3) & !3;
        let seq = self.seq.wrapping_add(1);
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
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

    /// Erase every record (factory reset).
    pub fn wipe<F: NorFlash>(&mut self, flash: &mut F) -> Result<(), StoreError> {
        flash
            .erase(0, self.sectors * self.sector_size)
            .map_err(|_| StoreError::Flash)?;
        self.current = None;
        self.seq = 0;
        Ok(())
    }
}

fn read_record<F: NorFlash>(
    flash: &mut F,
    at: u32,
    buf: &mut [u8],
) -> Result<Option<(u32, Settings)>, StoreError> {
    flash
        .read(at, &mut buf[..HEADER])
        .map_err(|_| StoreError::Flash)?;
    let word = |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    if word(0) != MAGIC {
        return Ok(None);
    }
    let seq = word(4);
    let len = word(8) as usize;
    if len > MAX_ENCODED {
        return Ok(None);
    }
    let padded = (len + 3) & !3;
    flash
        .read(at + HEADER as u32, &mut buf[HEADER..HEADER + padded + 4])
        .map_err(|_| StoreError::Flash)?;
    let crc = u32::from_le_bytes(
        buf[HEADER + padded..HEADER + padded + 4]
            .try_into()
            .unwrap(),
    );
    if CRC.checksum(&buf[..HEADER + padded]) != crc {
        return Ok(None);
    }
    Ok(Settings::decode(&buf[HEADER..HEADER + len])
        .ok()
        .map(|s| (seq, s)))
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
}
