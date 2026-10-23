//! End-to-end update cycle on a simulated NOR flash: stage an image with the
//! app-side updater, let the bootloader logic swap it in, then confirm or revert.

use core::cell::RefCell;

use embassy_boot::State;
use embassy_boot_esp::boot;
use embassy_boot_esp::image::{self, CHIP_ESP32, ESP32, Header, ImageError};
use embassy_boot_esp::partition::{self, BootPartitions, Partition};
use embassy_boot_esp::{BootStatus, UpdateError, Updater};
use embassy_sync::blocking_mutex::{Mutex, raw::NoopRawMutex};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

const SIZE: usize = 0x80000;

/// NOR semantics: erase sets 0xFF, writes can only clear bits.
struct MemFlash(Vec<u8>);

#[derive(Debug)]
struct FlashErr;
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
        assert_eq!(offset % 4, 0);
        bytes.copy_from_slice(&self.0[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }
    fn capacity(&self) -> usize {
        self.0.len()
    }
}
impl NorFlash for MemFlash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;
    fn erase(&mut self, from: u32, to: u32) -> Result<(), FlashErr> {
        assert_eq!(from % 4096, 0);
        assert_eq!(to % 4096, 0);
        self.0[from as usize..to as usize].fill(0xFF);
        Ok(())
    }
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), FlashErr> {
        assert_eq!(offset % 4, 0);
        assert_eq!(bytes.len() % 4, 0);
        for (d, s) in self.0[offset as usize..].iter_mut().zip(bytes) {
            *d &= *s;
        }
        Ok(())
    }
}

fn table() -> Vec<u8> {
    partition::encode(
        &[
            Partition::new(
                "bootstate",
                partition::TYPE_BOOT,
                partition::SUBTYPE_STATE,
                0x9000,
                0x2000,
            ),
            Partition::new(
                "active",
                partition::TYPE_APP,
                partition::SUBTYPE_FACTORY,
                0x10000,
                0x30000,
            ),
            Partition::new(
                "dfu",
                partition::TYPE_BOOT,
                partition::SUBTYPE_DFU,
                0x40000,
                0x31000,
            ),
        ],
        None,
    )
}

fn make_image(version: &str, fill: u8) -> Vec<u8> {
    image::build(
        CHIP_ESP32,
        &ESP32,
        0x4008_0400,
        version,
        &[
            (0x3F40_0020, vec![fill; 5000]),
            (0x400D_0020, vec![fill; 9000]),
            (0x4008_0000, vec![fill; 2000]),
        ],
    )
    .unwrap()
}

struct Device {
    flash: Mutex<NoopRawMutex, RefCell<MemFlash>>,
    parts: BootPartitions,
}

#[repr(align(32))]
struct Aligned([u8; 4096]);

impl Device {
    fn new(initial: &[u8]) -> Self {
        let mut mem = vec![0xFF; SIZE];
        let t = table();
        mem[0x8000..0x8000 + t.len()].copy_from_slice(&t);
        let parts = BootPartitions::from_table(&t).unwrap();
        mem[parts.active.offset as usize..][..initial.len()].copy_from_slice(initial);
        Self {
            flash: Mutex::new(RefCell::new(MemFlash(mem))),
            parts,
        }
    }

    fn boot(&self) -> Result<(State, String), ImageError> {
        let mut buf = Aligned([0; 4096]);
        let p = boot::prepare(
            &self.flash,
            &self.parts,
            &mut buf.0,
            CHIP_ESP32,
            &ESP32,
            false,
        )
        .unwrap();
        Ok((p.state, p.image?.header.version_str().to_string()))
    }

    fn updater<'a>(&'a self, buf: &'a mut [u8]) -> Updater<'a, NoopRawMutex, MemFlash> {
        Updater::new(&self.flash, &self.parts, buf, CHIP_ESP32, &ESP32)
    }

    fn upload(&self, img: &[u8], chunk: usize) -> Result<(), UpdateError> {
        let mut buf = Aligned([0; 4096]);
        let mut up = self.updater(&mut buf.0[..4]);
        up.begin(img.len() as u32)?;
        for (i, c) in img.chunks(chunk).enumerate() {
            up.write((i * chunk) as u32, c)?;
        }
        up.finish().map(|_| ())
    }

    fn upload_checked(
        &self,
        img: &[u8],
        accept: impl FnOnce(&Header) -> bool,
    ) -> Result<(), UpdateError> {
        let mut buf = Aligned([0; 4096]);
        let mut up = self.updater(&mut buf.0[..4]);
        up.begin(img.len() as u32)?;
        up.write(0, img)?;
        up.finish_checked(accept).map(|_| ())
    }

    fn status(&self) -> BootStatus {
        let mut buf = [0u8; 4];
        self.updater(&mut buf).status().unwrap()
    }

    fn confirm(&self) {
        let mut buf = [0u8; 4];
        self.updater(&mut buf).mark_booted().unwrap();
    }
}

#[test]
fn update_confirm_cycle() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    assert_eq!(dev.boot().unwrap(), (State::Boot, "1.0.0".into()));

    dev.upload(&make_image("2.0.0", 0x22), 1000).unwrap();
    assert_eq!(dev.boot().unwrap(), (State::Swap, "2.0.0".into()));
    assert_eq!(dev.status(), BootStatus::Trial);
    dev.confirm();
    assert_eq!(dev.boot().unwrap(), (State::Boot, "2.0.0".into()));
    assert_eq!(dev.status(), BootStatus::Booted);
}

#[test]
fn unconfirmed_update_reverts() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    dev.upload(&make_image("2.0.0", 0x22), 4096).unwrap();
    assert_eq!(dev.boot().unwrap().1, "2.0.0");
    // No mark_booted: the next reset goes back to the old image.
    assert_eq!(dev.boot().unwrap(), (State::Revert, "1.0.0".into()));
    assert_eq!(dev.status(), BootStatus::Reverted);
    // A reverted device accepts a new update.
    dev.upload(&make_image("3.0.0", 0x33), 777).unwrap();
    assert_eq!(dev.boot().unwrap().1, "3.0.0");
}

#[test]
fn corrupt_upload_is_rejected() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    let mut img = make_image("2.0.0", 0x22);
    let n = img.len();
    img[n / 2] ^= 1;
    assert_eq!(
        dev.upload(&img, 1000),
        Err(UpdateError::Image(ImageError::HashMismatch))
    );
    assert_eq!(dev.boot().unwrap(), (State::Boot, "1.0.0".into()));
}

#[test]
fn refused_image_is_not_staged() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    assert_eq!(
        dev.upload_checked(&make_image("2.0.0@dual", 0x22), |h| !h
            .version_str()
            .contains('@')),
        Err(UpdateError::Rejected)
    );
    assert_eq!(dev.boot().unwrap(), (State::Boot, "1.0.0".into()));
    dev.upload_checked(&make_image("2.0.0", 0x22), |_| true)
        .unwrap();
    assert_eq!(dev.boot().unwrap().1, "2.0.0");
}

#[test]
fn trial_image_cannot_be_updated() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    dev.upload(&make_image("2.0.0", 0x22), 1000).unwrap();
    dev.boot().unwrap();
    assert_eq!(
        dev.upload(&make_image("3.0.0", 0x33), 1000),
        Err(UpdateError::NotBooted)
    );
}

#[test]
fn out_of_order_chunks_rejected() {
    let dev = Device::new(&make_image("1.0.0", 0x11));
    let mut buf = [0u8; 4];
    let mut up = dev.updater(&mut buf);
    up.begin(5000).unwrap();
    up.write(0, &[0; 100]).unwrap();
    assert_eq!(up.write(200, &[0; 100]), Err(UpdateError::Sequence));
}

#[test]
fn blank_flash_reports_bad_image() {
    let dev = Device::new(&[]);
    assert_eq!(dev.boot().err(), Some(ImageError::BadMagic));
}
