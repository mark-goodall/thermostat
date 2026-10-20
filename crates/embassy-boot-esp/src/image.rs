//! Application image format understood by the bootloader.
//!
//! The ESP32 ROM only loads the second-stage bootloader; everything after that
//! is ours, so the application uses a small purpose-built format instead of the
//! ESP-IDF one:
//!
//! ```text
//! 0x00  header (64 bytes)
//! 0x40  segment table (16 bytes per segment)
//! ....  segment data
//! N     SHA-256 over bytes [0, N), where N = header.image_len
//! ```
//!
//! Segments flagged [`SegmentKind::Flash`] are executed in place through the
//! flash cache. The MMU maps 64 KiB pages, so such a segment is stored at an
//! image offset congruent to its virtual address modulo 64 KiB. Everything
//! else is copied into RAM by the bootloader before it jumps to `entry`.

use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 4] = *b"EBES";
pub const FORMAT_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 64;
pub const SEGMENT_ENTRY_LEN: usize = 16;
pub const MAX_SEGMENTS: usize = 16;
pub const HASH_LEN: usize = 32;
pub const VERSION_LEN: usize = 32;
/// Flash MMU page size on the ESP32.
pub const MMU_PAGE: u32 = 0x1_0000;

/// Chip identifiers (matches the ESP-IDF image `chip_id` numbering).
pub const CHIP_ESP32: u8 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    BadMagic,
    UnsupportedVersion,
    WrongChip,
    TooManySegments,
    /// A segment lies outside the image or overlaps the header.
    SegmentOutOfBounds,
    /// A flash-mapped segment is not congruent with its address modulo [`MMU_PAGE`].
    Misaligned,
    /// A RAM segment targets memory the bootloader is not allowed to overwrite.
    BadLoadAddress,
    TooLarge,
    HashMismatch,
    Flash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Copied into RAM at boot.
    Ram = 0,
    /// Mapped from flash through the cache MMU.
    Flash = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u32,
    /// Offset of the data from the start of the image.
    pub offset: u32,
    pub len: u32,
    pub kind: SegmentKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub chip: u8,
    pub segment_count: u16,
    pub entry: u32,
    /// Length of the hashed region. The SHA-256 trailer starts here.
    pub image_len: u32,
    pub version: [u8; VERSION_LEN],
}

impl Header {
    pub fn parse(buf: &[u8]) -> Result<Self, ImageError> {
        if buf.len() < HEADER_LEN || buf[0..4] != MAGIC {
            return Err(ImageError::BadMagic);
        }
        if buf[4] != FORMAT_VERSION {
            return Err(ImageError::UnsupportedVersion);
        }
        let segment_count = u16::from_le_bytes([buf[6], buf[7]]);
        if segment_count as usize > MAX_SEGMENTS {
            return Err(ImageError::TooManySegments);
        }
        let mut version = [0; VERSION_LEN];
        version.copy_from_slice(&buf[16..16 + VERSION_LEN]);
        Ok(Self {
            chip: buf[5],
            segment_count,
            entry: le32(&buf[8..]),
            image_len: le32(&buf[12..]),
            version,
        })
    }

    pub fn write(&self, buf: &mut [u8]) {
        buf[..HEADER_LEN].fill(0);
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = FORMAT_VERSION;
        buf[5] = self.chip;
        buf[6..8].copy_from_slice(&self.segment_count.to_le_bytes());
        buf[8..12].copy_from_slice(&self.entry.to_le_bytes());
        buf[12..16].copy_from_slice(&self.image_len.to_le_bytes());
        buf[16..16 + VERSION_LEN].copy_from_slice(&self.version);
    }

    /// The version string with NUL padding removed.
    pub fn version_str(&self) -> &str {
        let end = self
            .version
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(VERSION_LEN);
        core::str::from_utf8(&self.version[..end]).unwrap_or("?")
    }

    /// Total bytes occupied by the image including the hash trailer.
    pub fn total_len(&self) -> u32 {
        self.image_len + HASH_LEN as u32
    }

    fn table_end(&self) -> u32 {
        (HEADER_LEN + self.segment_count as usize * SEGMENT_ENTRY_LEN) as u32
    }
}

impl Segment {
    pub fn parse(buf: &[u8]) -> Self {
        Self {
            vaddr: le32(&buf[0..]),
            offset: le32(&buf[4..]),
            len: le32(&buf[8..]),
            kind: if le32(&buf[12..]) & 1 == 1 {
                SegmentKind::Flash
            } else {
                SegmentKind::Ram
            },
        }
    }

    pub fn write(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&self.vaddr.to_le_bytes());
        buf[4..8].copy_from_slice(&self.offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.len.to_le_bytes());
        buf[12..16].copy_from_slice(&(self.kind as u32).to_le_bytes());
    }
}

/// Memory map rules for a chip: which addresses are flash-mapped and which RAM
/// ranges the bootloader may write into.
pub struct MemoryMap {
    pub flash: &'static [(u32, u32)],
    pub ram: &'static [(u32, u32)],
}

/// ESP32 memory map. RAM ranges exclude the bootloader's own IRAM/DRAM, the
/// ROM data and the ROM stack, which are all live while the app is loaded.
pub const ESP32: MemoryMap = MemoryMap {
    flash: &[
        (0x3F40_0000, 0x3F80_0000), // DROM0
        (0x400D_0000, 0x4040_0000), // IROM (via IRAM0 cache region)
    ],
    ram: &[
        (0x4008_0000, 0x400A_0000), // SRAM0 IRAM (above the cache-reserved 64K)
        (0x3FFB_0000, 0x3FFE_0000), // SRAM2 + start of SRAM1 DRAM
        (0x400C_0000, 0x400C_2000), // RTC fast memory (instruction bus)
        (0x3FF8_0000, 0x3FF8_2000), // RTC fast memory (data bus)
        (0x5000_0000, 0x5000_2000), // RTC slow memory
    ],
};

impl MemoryMap {
    pub fn is_flash(&self, vaddr: u32, len: u32) -> bool {
        within(self.flash, vaddr, len)
    }
    pub fn is_ram(&self, vaddr: u32, len: u32) -> bool {
        within(self.ram, vaddr, len)
    }
}

fn within(ranges: &[(u32, u32)], start: u32, len: u32) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    ranges.iter().any(|&(lo, hi)| start >= lo && end <= hi)
}

/// Validated view of an image header and its segment table.
pub struct ImageInfo {
    pub header: Header,
    pub segments: [Segment; MAX_SEGMENTS],
}

impl ImageInfo {
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.header.segment_count as usize]
    }
}

/// Parse and sanity check an image header plus segment table.
///
/// `head` must hold at least the first `HEADER_LEN + MAX_SEGMENTS * SEGMENT_ENTRY_LEN`
/// bytes of the image (or the whole image if shorter). `capacity` is the size of
/// the partition holding the image.
pub fn inspect(
    head: &[u8],
    capacity: u32,
    chip: u8,
    map: &MemoryMap,
) -> Result<ImageInfo, ImageError> {
    let header = Header::parse(head)?;
    if header.chip != chip {
        return Err(ImageError::WrongChip);
    }
    let table_end = header.table_end();
    if head.len() < table_end as usize {
        return Err(ImageError::SegmentOutOfBounds);
    }
    if header.total_len() > capacity || header.image_len < table_end {
        return Err(ImageError::TooLarge);
    }
    let mut segments = [Segment {
        vaddr: 0,
        offset: 0,
        len: 0,
        kind: SegmentKind::Ram,
    }; MAX_SEGMENTS];
    for (i, seg) in segments
        .iter_mut()
        .take(header.segment_count as usize)
        .enumerate()
    {
        let at = HEADER_LEN + i * SEGMENT_ENTRY_LEN;
        *seg = Segment::parse(&head[at..at + SEGMENT_ENTRY_LEN]);
        let end = seg
            .offset
            .checked_add(seg.len)
            .ok_or(ImageError::SegmentOutOfBounds)?;
        if seg.offset < table_end || end > header.image_len {
            return Err(ImageError::SegmentOutOfBounds);
        }
        match seg.kind {
            SegmentKind::Flash => {
                if !map.is_flash(seg.vaddr, seg.len) {
                    return Err(ImageError::BadLoadAddress);
                }
                if seg.vaddr % MMU_PAGE != seg.offset % MMU_PAGE {
                    return Err(ImageError::Misaligned);
                }
            }
            SegmentKind::Ram => {
                if !map.is_ram(seg.vaddr, seg.len) || seg.vaddr % 4 != 0 || seg.len % 4 != 0 {
                    return Err(ImageError::BadLoadAddress);
                }
            }
        }
    }
    Ok(ImageInfo { header, segments })
}

/// Hash the image through `read(offset, buf)` and compare against its trailer.
///
/// `buf` is scratch space; its length must be a multiple of 4.
pub fn verify<E>(
    header: &Header,
    buf: &mut [u8],
    mut read: impl FnMut(u32, &mut [u8]) -> Result<(), E>,
) -> Result<(), ImageError> {
    let mut hasher = Sha256::new();
    let mut offset = 0u32;
    while offset < header.image_len {
        let n = buf.len().min((header.image_len - offset) as usize);
        // Reads stay word sized; the hasher only consumes the requested bytes.
        let padded = (n + 3) & !3;
        read(offset, &mut buf[..padded]).map_err(|_| ImageError::Flash)?;
        hasher.update(&buf[..n]);
        offset += n as u32;
    }
    let mut trailer = [0u8; HASH_LEN];
    read(header.image_len, &mut trailer).map_err(|_| ImageError::Flash)?;
    if hasher.finalize().as_slice() == trailer {
        Ok(())
    } else {
        Err(ImageError::HashMismatch)
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(feature = "std")]
pub use builder::build;

#[cfg(feature = "std")]
mod builder {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    /// Build an image from loadable ELF segments `(vaddr, data)`.
    pub fn build(
        chip: u8,
        map: &MemoryMap,
        entry: u32,
        version: &str,
        segments: &[(u32, Vec<u8>)],
    ) -> Result<Vec<u8>, ImageError> {
        if segments.len() > MAX_SEGMENTS {
            return Err(ImageError::TooManySegments);
        }
        let mut flash: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut ram: Vec<(u32, Vec<u8>)> = Vec::new();
        for (vaddr, data) in segments {
            let mut data = data.clone();
            if map.is_flash(*vaddr, data.len() as u32) {
                flash.push((*vaddr, data));
            } else if map.is_ram(*vaddr, data.len() as u32) {
                while data.len() % 4 != 0 {
                    data.push(0);
                }
                ram.push((*vaddr, data));
            } else {
                return Err(ImageError::BadLoadAddress);
            }
        }
        flash.sort_by_key(|s| s.0);
        // Largest RAM segments first gives the gap filler a better chance.
        ram.sort_by_key(|s| core::cmp::Reverse(s.1.len()));

        let table_end = (HEADER_LEN + segments.len() * SEGMENT_ENTRY_LEN) as u32;
        let mut placed: Vec<Segment> = Vec::new();
        let mut cursor = table_end;
        let mut ram_left: Vec<Option<(u32, Vec<u8>)>> = ram.into_iter().map(Some).collect();
        let mut out = Vec::new();

        let mut place_ram = |cursor: &mut u32,
                             limit: Option<u32>,
                             placed: &mut Vec<Segment>,
                             out: &mut Vec<(u32, Vec<u8>)>| {
            for slot in ram_left.iter_mut() {
                let fits = match (slot.as_ref(), limit) {
                    (Some((_, d)), Some(limit)) => *cursor + d.len() as u32 <= limit,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if fits {
                    let (vaddr, data) = slot.take().unwrap();
                    placed.push(Segment {
                        vaddr,
                        offset: *cursor,
                        len: data.len() as u32,
                        kind: SegmentKind::Ram,
                    });
                    *cursor += data.len() as u32;
                    out.push((placed.last().unwrap().offset, data));
                }
            }
        };

        for (vaddr, data) in flash {
            let mut offset = (cursor & !(MMU_PAGE - 1)) | (vaddr % MMU_PAGE);
            if offset < cursor {
                offset += MMU_PAGE;
            }
            place_ram(&mut cursor, Some(offset), &mut placed, &mut out);
            placed.push(Segment {
                vaddr,
                offset,
                len: data.len() as u32,
                kind: SegmentKind::Flash,
            });
            cursor = offset + data.len() as u32;
            out.push((offset, data));
        }
        place_ram(&mut cursor, None, &mut placed, &mut out);

        let image_len = (cursor + 3) & !3;
        let mut image = std::vec![0xFFu8; image_len as usize + HASH_LEN];
        for (offset, data) in out {
            image[offset as usize..offset as usize + data.len()].copy_from_slice(&data);
        }
        let mut v = [0u8; VERSION_LEN];
        let vb = version.as_bytes();
        let n = vb.len().min(VERSION_LEN);
        v[..n].copy_from_slice(&vb[..n]);
        Header {
            chip,
            segment_count: placed.len() as u16,
            entry,
            image_len,
            version: v,
        }
        .write(&mut image);
        for (i, seg) in placed.iter().enumerate() {
            let at = HEADER_LEN + i * SEGMENT_ENTRY_LEN;
            seg.write(&mut image[at..at + SEGMENT_ENTRY_LEN]);
        }
        let hash = Sha256::digest(&image[..image_len as usize]);
        image[image_len as usize..].copy_from_slice(&hash);
        Ok(image)
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    extern crate std;
    use std::vec;

    use super::*;

    fn sample() -> std::vec::Vec<u8> {
        build(
            CHIP_ESP32,
            &ESP32,
            0x4008_0400,
            "1.2.3",
            &[
                (0x3F40_0020, vec![1; 1000]),
                (0x400D_0020, vec![2; 3000]),
                (0x4008_0000, vec![3; 4001]),
                (0x3FFB_0000, vec![4; 200]),
            ],
        )
        .unwrap()
    }

    #[test]
    fn roundtrip() {
        let img = sample();
        let info = inspect(&img, img.len() as u32, CHIP_ESP32, &ESP32).unwrap();
        assert_eq!(info.header.version_str(), "1.2.3");
        assert_eq!(info.header.entry, 0x4008_0400);
        assert_eq!(info.segments().len(), 4);
        for seg in info.segments() {
            let data = &img[seg.offset as usize..(seg.offset + seg.len) as usize];
            let fill = match seg.vaddr {
                0x3F40_0020 => 1,
                0x400D_0020 => 2,
                0x4008_0000 => 3,
                _ => 4,
            };
            assert!(data[..seg.len.min(200) as usize].iter().all(|&b| b == fill));
            if seg.kind == SegmentKind::Flash {
                assert_eq!(seg.offset % MMU_PAGE, seg.vaddr % MMU_PAGE);
            }
        }
        // RAM segments were packed into the gap before the first flash segment.
        assert!(info.header.image_len < 2 * MMU_PAGE + 4000);
        let mut buf = [0u8; 256];
        verify(&info.header, &mut buf, |off, b: &mut [u8]| {
            b.copy_from_slice(&img[off as usize..off as usize + b.len()]);
            Ok::<_, ()>(())
        })
        .unwrap();
    }

    #[test]
    fn detects_corruption() {
        let mut img = sample();
        let info = inspect(&img, img.len() as u32, CHIP_ESP32, &ESP32).unwrap();
        img[info.segments()[0].offset as usize] ^= 0xFF;
        let mut buf = [0u8; 256];
        let res = verify(&info.header, &mut buf, |off, b: &mut [u8]| {
            b.copy_from_slice(&img[off as usize..off as usize + b.len()]);
            Ok::<_, ()>(())
        });
        assert_eq!(res, Err(ImageError::HashMismatch));
    }

    #[test]
    fn rejects_bad_images() {
        let img = sample();
        assert_eq!(
            inspect(&img, 100, CHIP_ESP32, &ESP32).err(),
            Some(ImageError::TooLarge)
        );
        assert_eq!(
            inspect(&img, img.len() as u32, 9, &ESP32).err(),
            Some(ImageError::WrongChip)
        );
        assert_eq!(
            inspect(&[0; 64], 1 << 20, CHIP_ESP32, &ESP32).err(),
            Some(ImageError::BadMagic)
        );

        let mut bad = img.clone();
        // Point the first segment at the bootloader's own DRAM.
        let seg = Segment {
            vaddr: 0x3FFF_0000,
            offset: 0x100,
            len: 4,
            kind: SegmentKind::Ram,
        };
        seg.write(&mut bad[HEADER_LEN..HEADER_LEN + SEGMENT_ENTRY_LEN]);
        assert_eq!(
            inspect(&bad, bad.len() as u32, CHIP_ESP32, &ESP32).err(),
            Some(ImageError::BadLoadAddress)
        );
    }
}
