//! Thermostat second-stage bootloader.
//!
//! All the work happens in [`embassy_boot_esp::bootloader`]; this crate only
//! provides the entry point, panic handler and memory layout.
#![no_std]
#![no_main]

use embassy_boot_esp::bootloader::{self, Config};

/// Flash size in bytes, overridable at build time (`THERMOSTAT_FLASH_SIZE`, in MiB).
const FLASH_SIZE: u32 = match option_env!("THERMOSTAT_FLASH_SIZE") {
    Some(s) => parse_mib(s.as_bytes()) * 1024 * 1024,
    None => 4 * 1024 * 1024,
};

const fn parse_mib(s: &[u8]) -> u32 {
    let mut v = 0;
    let mut i = 0;
    while i < s.len() {
        v = v * 10 + (s[i] - b'0') as u32;
        i += 1;
    }
    v
}

unsafe extern "C" {
    static mut _bss_start: u32;
    static mut _bss_end: u32;
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // The ROM loads .data with the image but knows nothing about .bss.
    unsafe {
        let mut p = &raw mut _bss_start;
        while p < &raw mut _bss_end {
            p.write_volatile(0);
            p = p.add(1);
        }
    }
    bootloader::run(Config {
        flash_size: FLASH_SIZE,
        always_verify: false,
    })
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    embassy_boot_esp::bl_println!("bootloader panic: {}", info);
    bootloader::halt("panic")
}
