//! Hardware-independent parts of the thermostat firmware.
//!
//! Everything here builds for the host as well as the ESP32, so it is unit
//! tested with `cargo test`. The firmware crate provides the [`platform`]
//! traits on real hardware (or QEMU).
#![cfg_attr(not(any(test, feature = "std")), no_std)]

pub mod auth;
pub mod ble;
pub mod clock;
pub mod codec;
pub mod control;
pub mod dhcp;
pub mod http;
pub mod platform;
pub mod proto;
pub mod settings;
pub mod sntp;
pub mod store;
pub mod web;
pub mod zones;

pub use codec::hex;
