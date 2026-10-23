//! Application-side firmware updates.
//!
//! Wraps embassy-boot's [`BlockingFirmwareUpdater`] with the partition table
//! lookup and image validation, so transports (HTTP, serial, ...) only need to
//! stream bytes in order.

use core::cell::RefCell;

use embassy_boot::{BlockingFirmwareUpdater, FirmwareUpdaterConfig, FirmwareUpdaterError, State};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_sync::blocking_mutex::{Mutex, raw::RawMutex};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

use crate::image::{self, Header, ImageError, MemoryMap};
use crate::partition::BootPartitions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateError {
    /// No update in progress, or data arrived out of order.
    Sequence,
    /// The update is larger than the DFU partition.
    TooLarge,
    Image(ImageError),
    Flash,
    /// The running firmware has not been marked booted; updating now would
    /// make a revert impossible.
    NotBooted,
    /// A valid image the caller refused (see [`Updater::finish_checked`]).
    Rejected,
}

impl From<FirmwareUpdaterError> for UpdateError {
    fn from(e: FirmwareUpdaterError) -> Self {
        match e {
            FirmwareUpdaterError::BadState => UpdateError::NotBooted,
            _ => UpdateError::Flash,
        }
    }
}

impl From<ImageError> for UpdateError {
    fn from(e: ImageError) -> Self {
        UpdateError::Image(e)
    }
}

/// Boot status as seen by the running application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStatus {
    /// Running a confirmed image.
    Booted,
    /// Running a freshly swapped image that has not been confirmed yet.
    /// Resetting without calling [`Updater::mark_booted`] reverts it.
    Trial,
    /// The previous update failed to confirm and was reverted.
    Reverted,
    /// An update is staged and will be swapped in on the next reset.
    UpdatePending,
}

type Part<'a, M, F> = BlockingPartition<'a, M, F>;

/// Firmware updater bound to the boot partitions of a flash device.
///
/// `ALIGN` is a scratch buffer for embassy-boot; it must be at least the flash
/// write size and divide the erase size.
pub struct Updater<'a, M: RawMutex, F: NorFlash> {
    inner: BlockingFirmwareUpdater<'a, Part<'a, M, F>, Part<'a, M, F>>,
    dfu_size: u32,
    chip: u8,
    map: &'static MemoryMap,
    /// Next expected byte offset of the update being received.
    next: Option<u32>,
    carry: [u8; 4],
    carry_len: usize,
}

impl<'a, M: RawMutex, F: NorFlash> Updater<'a, M, F> {
    pub fn new(
        flash: &'a Mutex<M, RefCell<F>>,
        parts: &BootPartitions,
        aligned: &'a mut [u8],
        chip: u8,
        map: &'static MemoryMap,
    ) -> Self {
        let config = FirmwareUpdaterConfig {
            dfu: BlockingPartition::new(flash, parts.dfu.offset, parts.dfu.size),
            state: BlockingPartition::new(flash, parts.state.offset, parts.state.size),
        };
        Self {
            inner: BlockingFirmwareUpdater::new(config, aligned),
            dfu_size: parts.dfu.size,
            chip,
            map,
            next: None,
            carry: [0; 4],
            carry_len: 0,
        }
    }

    pub fn status(&mut self) -> Result<BootStatus, UpdateError> {
        Ok(match self.inner.get_state()? {
            State::Boot => BootStatus::Booted,
            State::Swap => BootStatus::Trial,
            State::Revert => BootStatus::Reverted,
            State::DfuDetach => BootStatus::UpdatePending,
        })
    }

    /// Confirm the running image. Call once the firmware is known to work.
    pub fn mark_booted(&mut self) -> Result<(), UpdateError> {
        self.inner.mark_booted()?;
        Ok(())
    }

    /// Start receiving an update of `len` bytes (header to end of hash trailer).
    pub fn begin(&mut self, len: u32) -> Result<(), UpdateError> {
        if len > self.dfu_size {
            return Err(UpdateError::TooLarge);
        }
        if !matches!(self.status()?, BootStatus::Booted | BootStatus::Reverted) {
            return Err(UpdateError::NotBooted);
        }
        self.next = Some(0);
        self.carry_len = 0;
        Ok(())
    }

    /// Write the next chunk. Chunks must arrive in order; any length is accepted.
    pub fn write(&mut self, offset: u32, mut data: &[u8]) -> Result<(), UpdateError> {
        if self.next != Some(offset) {
            return Err(UpdateError::Sequence);
        }
        let end = offset as usize + data.len();
        if end > self.dfu_size as usize {
            return Err(UpdateError::TooLarge);
        }
        self.next = Some(end as u32);
        // The flash takes word-aligned writes: complete a carried partial word
        // first, write whole words, and carry the remainder.
        let mut flash_at = offset as usize - self.carry_len;
        if self.carry_len > 0 {
            let n = (4 - self.carry_len).min(data.len());
            self.carry[self.carry_len..self.carry_len + n].copy_from_slice(&data[..n]);
            self.carry_len += n;
            data = &data[n..];
            if self.carry_len < 4 {
                return Ok(());
            }
            let word = self.carry;
            self.inner.write_firmware(flash_at, &word)?;
            flash_at += 4;
            self.carry_len = 0;
        }
        let whole = data.len() & !3;
        if whole > 0 {
            self.inner.write_firmware(flash_at, &data[..whole])?;
        }
        let rest = &data[whole..];
        self.carry[..rest.len()].copy_from_slice(rest);
        self.carry_len = rest.len();
        Ok(())
    }

    /// Validate the staged image and schedule it for the next boot.
    pub fn finish(&mut self) -> Result<Header, UpdateError> {
        self.finish_checked(|_| true)
    }

    /// Like [`finish`](Self::finish), but the verified image is only staged if
    /// `accept` approves its header (e.g. it was built for this board).
    pub fn finish_checked(
        &mut self,
        accept: impl FnOnce(&Header) -> bool,
    ) -> Result<Header, UpdateError> {
        let received = self.next.take().ok_or(UpdateError::Sequence)?;
        if self.carry_len > 0 {
            let mut word = [0xFFu8; 4];
            word[..self.carry_len].copy_from_slice(&self.carry[..self.carry_len]);
            self.inner
                .write_firmware(received as usize - self.carry_len, &word)?;
            self.carry_len = 0;
        }
        let header = self.verify_dfu()?;
        if header.total_len() != received {
            return Err(UpdateError::Image(ImageError::TooLarge));
        }
        if !accept(&header) {
            return Err(UpdateError::Rejected);
        }
        self.inner.mark_updated()?;
        Ok(header)
    }

    /// Abandon a partially received update. The DFU contents are left as-is
    /// and are never booted unless a later update completes.
    pub fn abort(&mut self) {
        self.next = None;
        self.carry_len = 0;
    }

    /// Check the image in the DFU partition (structure and SHA-256).
    pub fn verify_dfu(&mut self) -> Result<Header, UpdateError> {
        let mut head = [0u8; image::HEADER_LEN + image::MAX_SEGMENTS * image::SEGMENT_ENTRY_LEN];
        self.inner.read_dfu(0, &mut head)?;
        let info = image::inspect(&head, self.dfu_size, self.chip, self.map)?;
        let mut buf = [0u8; 256];
        let inner = &mut self.inner;
        image::verify(&info.header, &mut buf, |off, b| inner.read_dfu(off, b))?;
        Ok(info.header)
    }
}

/// Read the header of the image in a partition (e.g. the running one).
pub fn read_header<F: ReadNorFlash>(flash: &mut F, offset: u32) -> Option<Header> {
    let mut head = [0u8; image::HEADER_LEN];
    flash.read(offset, &mut head).ok()?;
    Header::parse(&head).ok()
}
