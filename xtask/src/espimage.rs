//! ELF handling: the ROM-loadable bootloader image and the application image.

use anyhow::{Context, Result, bail};
use object::{Object, ObjectSegment};

pub struct Elf {
    pub entry: u32,
    /// Loadable segments with file contents: (address, bytes).
    pub segments: Vec<(u32, Vec<u8>)>,
}

pub fn read_elf(path: &std::path::Path) -> Result<Elf> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file = object::File::parse(&*data)?;
    let mut segments = Vec::new();
    for seg in file.segments() {
        let bytes = seg.data()?;
        if bytes.is_empty() {
            continue;
        }
        segments.push((seg.address() as u32, bytes.to_vec()));
    }
    Ok(Elf {
        entry: file.entry() as u32,
        segments,
    })
}

/// Build an ESP32 ROM bootloader image (esp_image_header_t format).
pub fn rom_image(elf: &Elf, flash_size: u32) -> Result<Vec<u8>> {
    let size_code = match flash_size >> 20 {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        16 => 4,
        _ => bail!("unsupported flash size"),
    };
    let mut out = vec![
        0xE9,
        elf.segments.len() as u8,
        2,                // SPI mode: DIO
        (size_code << 4), // 40 MHz
    ];
    out.extend_from_slice(&elf.entry.to_le_bytes());
    out.extend_from_slice(&[0xEE, 0, 0, 0]); // WP pin unused, default pin drive
    out.extend_from_slice(&0u16.to_le_bytes()); // chip id: ESP32
    out.push(0); // min chip revision (legacy)
    out.extend_from_slice(&0u16.to_le_bytes()); // min chip revision
    out.extend_from_slice(&399u16.to_le_bytes()); // max chip revision v3.99
    out.extend_from_slice(&[0; 4]);
    out.push(0); // no SHA-256 appended
    let mut checksum = 0xEFu8;
    for (addr, data) in &elf.segments {
        let mut data = data.clone();
        data.resize(data.len().div_ceil(4) * 4, 0);
        out.extend_from_slice(&addr.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        checksum = data.iter().fold(checksum, |c, b| c ^ b);
        out.extend_from_slice(&data);
    }
    while out.len() % 16 != 15 {
        out.push(0);
    }
    out.push(checksum);
    Ok(out)
}

pub fn app_image(elf: &Elf, version: &str) -> Result<Vec<u8>> {
    use embassy_boot_esp::image;
    image::build(
        image::CHIP_ESP32,
        &image::ESP32,
        elf.entry,
        version,
        &elf.segments,
    )
    .map_err(|e| anyhow::anyhow!("cannot build app image: {e:?}"))
}
