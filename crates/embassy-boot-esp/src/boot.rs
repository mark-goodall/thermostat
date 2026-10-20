//! Chip-independent half of the bootloader: run embassy-boot and pick the
//! image to start. The ESP32 specific half (ROM flash access, MMU, jump) lives
//! in [`crate::bootloader`].

use core::cell::RefCell;

use embassy_boot::{BootLoader, BootLoaderConfig, State};
use embassy_embedded_hal::flash::partition::BlockingPartition;
use embassy_sync::blocking_mutex::{Mutex, raw::RawMutex};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

use crate::image::{self, ImageError, ImageInfo, MemoryMap};
use crate::partition::BootPartitions;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootError {
    Flash,
}

pub struct Prepared {
    /// What embassy-boot did: plain boot, swapped in an update, or reverted.
    pub state: State,
    /// The active image, or why it cannot be started.
    pub image: Result<ImageInfo, ImageError>,
}

/// Run embassy-boot's swap/revert logic, then validate the active image.
///
/// Only flash errors fail outright. An invalid image is reported in
/// [`Prepared::image`] together with the boot state, so the caller can decide
/// between resetting (a bad trial image reverts on the next boot) and waiting
/// for a reflash.
///
/// The full SHA-256 check runs after a swap or revert (the image has just
/// been copied page by page). On a normal boot only the structure is checked, which keeps
/// boot time short; the image was hashed before it was accepted for update.
pub fn prepare<M: RawMutex, F: NorFlash>(
    flash: &Mutex<M, RefCell<F>>,
    parts: &BootPartitions,
    aligned: &mut [u8],
    chip: u8,
    map: &MemoryMap,
    always_verify: bool,
) -> Result<Prepared, BootError> {
    let mut bl = BootLoader::new(BootLoaderConfig {
        active: BlockingPartition::new(flash, parts.active.offset, parts.active.size),
        dfu: BlockingPartition::new(flash, parts.dfu.offset, parts.dfu.size),
        state: BlockingPartition::new(flash, parts.state.offset, parts.state.size),
    });
    let mut state = bl.prepare_boot(aligned).map_err(|_| BootError::Flash)?;
    // prepare_boot reports the state it found (Swap) even when it went on to
    // revert an unconfirmed image; the state partition tells the two apart.
    if state == State::Swap
        && bl.read_state(aligned).map_err(|_| BootError::Flash)? == State::Revert
    {
        state = State::Revert;
    }

    let mut active = BlockingPartition::new(flash, parts.active.offset, parts.active.size);
    let mut head = [0u8; image::HEADER_LEN + image::MAX_SEGMENTS * image::SEGMENT_ENTRY_LEN];
    active.read(0, &mut head).map_err(|_| BootError::Flash)?;
    // Pages were just copied around: hash the whole image before running it.
    let verify = always_verify || matches!(state, State::Swap | State::Revert);
    let image = image::inspect(&head, parts.active.size, chip, map).and_then(|info| {
        if verify {
            image::verify(&info.header, aligned, |off, b| active.read(off, b))?;
        }
        Ok(info)
    });
    Ok(Prepared { state, image })
}
