//! [embassy-boot](https://crates.io/crates/embassy-boot) for Espressif chips.
//!
//! Like `embassy-boot-stm32`/`-nrf`/`-rp`, this crate glues embassy-boot's
//! power-fail-safe swap algorithm to a chip family. On the ESP32 that means:
//!
//! * a **second-stage bootloader** ([`bootloader`], feature `bootloader`) that the
//!   mask ROM loads from flash offset 0x1000 in place of the ESP-IDF one. It
//!   finds the partitions in the standard partition table, runs embassy-boot's
//!   swap/revert, validates the active image, sets up the flash MMU and jumps
//!   into the application.
//! * an **application-side updater** ([`updater`]) that streams a new image into
//!   the DFU partition, verifies it and marks it for swapping.
//! * the **image format** ([`image`]) and **partition table** ([`partition`])
//!   shared by both, plus host-side builders (feature `std`) for tooling.
#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![cfg_attr(feature = "bootloader", feature(asm_experimental_arch))]

pub mod boot;
pub mod image;
pub mod partition;
pub mod updater;

#[cfg(feature = "bootloader")]
pub mod bootloader;

pub use embassy_boot::State;
pub use updater::{BootStatus, UpdateError, Updater};
