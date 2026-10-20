//! ESP-IDF compatible partition table (at flash offset 0x8000).
//!
//! The table keeps the standard binary format so `espflash`/`esptool` can read
//! it. The partitions embassy-boot needs are identified by type/subtype:
//!
//! | role       | type          | subtype |
//! |------------|---------------|---------|
//! | active     | app (0x00)    | factory (0x00) |
//! | dfu        | boot (0x40)   | 0x00    |
//! | boot state | boot (0x40)   | 0x01    |
//!
//! Further application data partitions (for example settings) can use any
//! other type; see [`find`].

use embedded_storage::nor_flash::ReadNorFlash;

pub const TABLE_OFFSET: u32 = 0x8000;
pub const TABLE_LEN: usize = 0xC00;
pub const ENTRY_LEN: usize = 32;
pub const MAX_ENTRIES: usize = 95;

const ENTRY_MAGIC: [u8; 2] = [0xAA, 0x50];
const MD5_MAGIC: [u8; 2] = [0xEB, 0xEB];

pub const TYPE_APP: u8 = 0x00;
pub const TYPE_DATA: u8 = 0x01;
pub const SUBTYPE_FACTORY: u8 = 0x00;
/// Custom partition type used for embassy-boot bookkeeping.
pub const TYPE_BOOT: u8 = 0x40;
pub const SUBTYPE_DFU: u8 = 0x00;
pub const SUBTYPE_STATE: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    pub kind: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub label: [u8; 16],
    pub flags: u32,
}

impl Partition {
    pub fn new(label: &str, kind: u8, subtype: u8, offset: u32, size: u32) -> Self {
        let mut l = [0; 16];
        let n = label.len().min(16);
        l[..n].copy_from_slice(&label.as_bytes()[..n]);
        Self {
            kind,
            subtype,
            offset,
            size,
            label: l,
            flags: 0,
        }
    }

    pub fn label(&self) -> &str {
        let end = self.label.iter().position(|&b| b == 0).unwrap_or(16);
        core::str::from_utf8(&self.label[..end]).unwrap_or("")
    }

    pub fn end(&self) -> u32 {
        self.offset + self.size
    }

    fn parse(b: &[u8]) -> Option<Self> {
        if b[0..2] != ENTRY_MAGIC {
            return None;
        }
        let mut label = [0; 16];
        label.copy_from_slice(&b[12..28]);
        Some(Self {
            kind: b[2],
            subtype: b[3],
            offset: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            size: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            label,
            flags: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
        })
    }

    pub fn write(&self, b: &mut [u8]) {
        b[0..2].copy_from_slice(&ENTRY_MAGIC);
        b[2] = self.kind;
        b[3] = self.subtype;
        b[4..8].copy_from_slice(&self.offset.to_le_bytes());
        b[8..12].copy_from_slice(&self.size.to_le_bytes());
        b[12..28].copy_from_slice(&self.label);
        b[28..32].copy_from_slice(&self.flags.to_le_bytes());
    }
}

/// Iterate the entries of a raw partition table.
pub fn entries(table: &[u8]) -> impl Iterator<Item = Partition> + '_ {
    table
        .as_chunks::<ENTRY_LEN>()
        .0
        .iter()
        .take(MAX_ENTRIES)
        .take_while(|e| e[0..2] != MD5_MAGIC && e[0..2] != [0xFF, 0xFF])
        .filter_map(|e| Partition::parse(e))
}

pub fn find(table: &[u8], kind: u8, subtype: u8) -> Option<Partition> {
    entries(table).find(|p| p.kind == kind && p.subtype == subtype)
}

pub fn find_by_label(table: &[u8], label: &str) -> Option<Partition> {
    entries(table).find(|p| p.label() == label)
}

/// The three partitions embassy-boot operates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootPartitions {
    pub active: Partition,
    pub dfu: Partition,
    pub state: Partition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    Missing,
    /// The active partition must start on a 64 KiB MMU page.
    ActiveNotPageAligned,
    /// embassy-boot needs DFU to be at least one erase page larger than active.
    DfuTooSmall,
    /// The state partition cannot hold swap progress for the active partition.
    StateTooSmall,
    Unaligned,
}

impl BootPartitions {
    pub fn from_table(table: &[u8]) -> Result<Self, LayoutError> {
        let parts = Self {
            active: find(table, TYPE_APP, SUBTYPE_FACTORY).ok_or(LayoutError::Missing)?,
            dfu: find(table, TYPE_BOOT, SUBTYPE_DFU).ok_or(LayoutError::Missing)?,
            state: find(table, TYPE_BOOT, SUBTYPE_STATE).ok_or(LayoutError::Missing)?,
        };
        parts.validate(4096, 4)?;
        Ok(parts)
    }

    pub fn validate(&self, erase_size: u32, write_size: u32) -> Result<(), LayoutError> {
        for p in [&self.active, &self.dfu, &self.state] {
            if p.offset % erase_size != 0 || p.size % erase_size != 0 {
                return Err(LayoutError::Unaligned);
            }
        }
        if !self.active.offset.is_multiple_of(crate::image::MMU_PAGE) {
            return Err(LayoutError::ActiveNotPageAligned);
        }
        if self.dfu.size < self.active.size + erase_size {
            return Err(LayoutError::DfuTooSmall);
        }
        // Mirrors embassy-boot's own assertion: magic + progress words for swap and revert.
        let pages = self.active.size / erase_size;
        if (2 + 4 * pages) * write_size > self.state.size {
            return Err(LayoutError::StateTooSmall);
        }
        Ok(())
    }
}

/// Read the raw partition table from flash.
pub fn read_table<F: ReadNorFlash>(
    flash: &mut F,
    buf: &mut [u8; TABLE_LEN],
) -> Result<(), F::Error> {
    flash.read(TABLE_OFFSET, buf)
}

#[cfg(feature = "std")]
pub use encode::encode;

#[cfg(feature = "std")]
mod encode {
    extern crate std;
    use super::*;

    /// Serialise partitions into a table image (without the optional MD5 entry,
    /// which callers may append with [`md5_entry`]).
    pub fn encode(parts: &[Partition], md5: Option<[u8; 16]>) -> std::vec::Vec<u8> {
        let mut out = std::vec![0xFFu8; TABLE_LEN];
        for (i, p) in parts.iter().enumerate() {
            p.write(&mut out[i * ENTRY_LEN..(i + 1) * ENTRY_LEN]);
        }
        if let Some(md5) = md5 {
            let at = parts.len() * ENTRY_LEN;
            out[at..at + 2].copy_from_slice(&MD5_MAGIC);
            out[at + 2..at + 16].fill(0xFF);
            out[at + 16..at + 32].copy_from_slice(&md5);
        }
        out
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn layout() -> [Partition; 4] {
        [
            Partition::new("bootstate", TYPE_BOOT, SUBTYPE_STATE, 0x9000, 0x2000),
            Partition::new("settings", TYPE_DATA, 0x99, 0x10000, 0x10000),
            Partition::new("active", TYPE_APP, SUBTYPE_FACTORY, 0x20000, 0x1C0000),
            Partition::new("dfu", TYPE_BOOT, SUBTYPE_DFU, 0x1E0000, 0x1C1000),
        ]
    }

    #[test]
    fn roundtrip_and_lookup() {
        let table = encode(&layout(), Some([0; 16]));
        assert_eq!(entries(&table).count(), 4);
        let boot = BootPartitions::from_table(&table).unwrap();
        assert_eq!(boot.active.offset, 0x20000);
        assert_eq!(boot.dfu.label(), "dfu");
        assert_eq!(find_by_label(&table, "settings").unwrap().subtype, 0x99);
    }

    #[test]
    fn layout_checks() {
        let mut l = layout();
        l[3].size = 0x1C0000;
        assert_eq!(
            BootPartitions::from_table(&encode(&l, None)),
            Err(LayoutError::DfuTooSmall)
        );
        let mut l = layout();
        l[0].size = 0x1000;
        assert_eq!(
            BootPartitions::from_table(&encode(&l, None)),
            Err(LayoutError::StateTooSmall)
        );
        let mut l = layout();
        l[2].offset = 0x21000;
        assert_eq!(
            BootPartitions::from_table(&encode(&l, None)),
            Err(LayoutError::ActiveNotPageAligned)
        );
    }
}
