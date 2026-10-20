//! ESP32 second-stage bootloader runtime.
//!
//! Runs from IRAM/DRAM regions that the application never loads into
//! (0x4007_8000 and 0x3FFF_0000, the same ones ESP-IDF's bootloader uses) on
//! the ROM's stack. It uses only mask-ROM routines (plus the ROM patches
//! shipped in `esp-rom-sys`) for flash, cache and UART, so it needs no HAL.

use core::cell::RefCell;
use core::fmt::{self, Write};

use embassy_boot::State;
use embassy_sync::blocking_mutex::{Mutex, raw::NoopRawMutex};
use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};
use esp_rom_sys::rom::spiflash::{
    esp_rom_spiflash_erase_sector, esp_rom_spiflash_read, esp_rom_spiflash_unlock,
    esp_rom_spiflash_write,
};

use crate::boot;
use crate::image::{CHIP_ESP32, ESP32, ImageInfo, MMU_PAGE, SegmentKind};
use crate::partition::{self, BootPartitions};

unsafe extern "C" {
    fn Cache_Read_Disable_rom(cpu: u32);
    fn Cache_Flush_rom(cpu: u32);
    fn Cache_Read_Enable_rom(cpu: u32);
    fn cache_flash_mmu_set_rom(
        cpu: u32,
        pid: u32,
        vaddr: u32,
        paddr: u32,
        psize: u32,
        num: u32,
    ) -> u32;
    fn mmu_init(cpu: u32);
    fn uart_tx_one_char(c: u8) -> i32;
    fn uart_tx_flush(uart: u8);
    fn software_reset() -> !;
    fn ets_delay_us(us: u32);
    static mut g_rom_flashchip: RomFlashChip;
}

#[repr(C)]
struct RomFlashChip {
    device_id: u32,
    chip_size: u32,
    block_size: u32,
    sector_size: u32,
    page_size: u32,
    status_mask: u32,
}

pub struct Config {
    /// Size of the SPI flash chip in bytes.
    pub flash_size: u32,
    /// Hash the active image on every boot, not only after a swap.
    pub always_verify: bool,
}

/// Console output through the ROM UART routine (UART0, as set up by the ROM).
pub struct RomUart;

impl Write for RomUart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            unsafe { uart_tx_one_char(b) };
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! bl_println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::bootloader::RomUart, $($arg)*);
    }};
}
use bl_println as println;

/// Whole-chip flash access through the ROM SPI flash routines.
pub struct RomFlash {
    size: u32,
}

#[derive(Debug)]
pub struct RomFlashError(pub i32);

impl NorFlashError for RomFlashError {
    fn kind(&self) -> NorFlashErrorKind {
        NorFlashErrorKind::Other
    }
}

impl ErrorType for RomFlash {
    type Error = RomFlashError;
}

const BOUNCE_WORDS: usize = 64;

fn check(rc: i32) -> Result<(), RomFlashError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(RomFlashError(rc))
    }
}

impl ReadNorFlash for RomFlash {
    const READ_SIZE: usize = 4;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        // The ROM wants word-aligned buffers; go through a bounce buffer.
        let mut bounce = [0u32; BOUNCE_WORDS];
        for (i, chunk) in bytes.chunks_mut(BOUNCE_WORDS * 4).enumerate() {
            let len = (chunk.len() + 3) & !3;
            let at = offset + (i * BOUNCE_WORDS * 4) as u32;
            check(unsafe { esp_rom_spiflash_read(at, bounce.as_mut_ptr(), len as u32) })?;
            let src =
                unsafe { core::slice::from_raw_parts(bounce.as_ptr() as *const u8, chunk.len()) };
            chunk.copy_from_slice(src);
        }
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.size as usize
    }
}

impl NorFlash for RomFlash {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        for sector in from / 4096..to / 4096 {
            check(unsafe { esp_rom_spiflash_erase_sector(sector) })?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let mut bounce = [0u32; BOUNCE_WORDS];
        for (i, chunk) in bytes.chunks(BOUNCE_WORDS * 4).enumerate() {
            let dst = unsafe {
                core::slice::from_raw_parts_mut(bounce.as_mut_ptr() as *mut u8, chunk.len())
            };
            dst.copy_from_slice(chunk);
            let at = offset + (i * BOUNCE_WORDS * 4) as u32;
            check(unsafe { esp_rom_spiflash_write(at, bounce.as_ptr(), chunk.len() as u32) })?;
        }
        Ok(())
    }
}

fn reg_write(addr: u32, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

fn reg_clear_bits(addr: u32, bits: u32) {
    unsafe {
        core::ptr::write_volatile(
            addr as *mut u32,
            core::ptr::read_volatile(addr as *const u32) & !bits,
        )
    }
}

/// The ROM arms the RTC and TIMG0 watchdogs in "flash boot" mode. A swap can
/// take tens of seconds, so turn them off; the application configures its own.
fn disable_watchdogs() {
    const KEY: u32 = 0x50D8_3AA1;
    for timg in [0x3FF5_F000, 0x3FF6_0000] {
        reg_write(timg + 0x64, KEY); // WDTWPROTECT
        reg_write(timg + 0x48, 0); // WDTCONFIG0
        reg_write(timg + 0x64, 0);
    }
    const RTC_CNTL: u32 = 0x3FF4_8000;
    reg_write(RTC_CNTL + 0xA4, KEY);
    reg_write(RTC_CNTL + 0x8C, 0);
    reg_write(RTC_CNTL + 0xA4, 0);
}

#[repr(align(32))]
struct Aligned([u8; 4096]);
static mut SCRATCH: Aligned = Aligned([0; 4096]);
static mut TABLE: [u8; partition::TABLE_LEN] = [0; partition::TABLE_LEN];

/// Prepare and start the application. Never returns.
pub fn run(config: Config) -> ! {
    disable_watchdogs();
    unsafe {
        let chip = &raw mut g_rom_flashchip;
        (*chip).chip_size = config.flash_size;
        esp_rom_spiflash_unlock();
    }
    println!("\nembassy-boot-esp {}", env!("CARGO_PKG_VERSION"));

    let flash = Mutex::<NoopRawMutex, _>::new(RefCell::new(RomFlash {
        size: config.flash_size,
    }));
    // SAFETY: single-threaded, these statics are only used here.
    let (table_ptr, scratch_ptr) = (&raw mut TABLE, &raw mut SCRATCH);
    let (table, scratch) = unsafe { (&mut *table_ptr, &mut (*scratch_ptr).0) };

    if flash
        .lock(|f| partition::read_table(&mut *f.borrow_mut(), table))
        .is_err()
    {
        halt("cannot read partition table");
    }
    let parts = match BootPartitions::from_table(table) {
        Ok(p) => p,
        Err(e) => {
            println!("partition table: {:?}", e);
            halt("no usable boot partitions");
        }
    };
    println!(
        "active {:#x}+{:#x} dfu {:#x}+{:#x} state {:#x}+{:#x}",
        parts.active.offset,
        parts.active.size,
        parts.dfu.offset,
        parts.dfu.size,
        parts.state.offset,
        parts.state.size
    );

    let prepared = match boot::prepare(
        &flash,
        &parts,
        scratch,
        CHIP_ESP32,
        &ESP32,
        config.always_verify,
    ) {
        Ok(p) => p,
        Err(_) => halt("flash error while preparing boot"),
    };
    let state = match prepared.state {
        State::Boot => "boot",
        State::Swap => "swapped (trial)",
        State::Revert => "reverted",
        State::DfuDetach => "dfu-detach",
    };
    match prepared.image {
        Ok(info) => {
            println!(
                "state: {}, image {} entry {:#x}",
                state,
                info.header.version_str(),
                info.header.entry
            );
            unsafe { start(&info, parts.active.offset) }
        }
        Err(e) => {
            println!("state: {}, active image invalid: {:?}", state, e);
            if prepared.state == State::Swap {
                // The trial image is broken; the next boot reverts it.
                println!("resetting to revert");
                unsafe {
                    uart_tx_flush(0);
                    software_reset()
                }
            }
            halt("no bootable image; flash one over serial")
        }
    }
}

/// Map flash segments, copy RAM segments and jump to the entry point.
///
/// # Safety
/// `info` must describe a validated image located at flash `base`.
unsafe fn start(info: &ImageInfo, base: u32) -> ! {
    unsafe {
        for cpu in 0..2 {
            Cache_Read_Disable_rom(cpu);
            Cache_Flush_rom(cpu);
            mmu_init(cpu);
        }
        for seg in info.segments() {
            let paddr = base + seg.offset;
            match seg.kind {
                SegmentKind::Flash => {
                    let vstart = seg.vaddr & !(MMU_PAGE - 1);
                    let pstart = paddr & !(MMU_PAGE - 1);
                    let pages = ((seg.vaddr % MMU_PAGE) + seg.len).div_ceil(MMU_PAGE);
                    for cpu in 0..2 {
                        let rc = cache_flash_mmu_set_rom(cpu, 0, vstart, pstart, 64, pages);
                        if rc != 0 {
                            println!("mmu map {:#x} failed: {}", vstart, rc);
                            halt("mmu");
                        }
                    }
                }
                SegmentKind::Ram => {
                    // inspect() guarantees word alignment and an allowed range.
                    let rc = esp_rom_spiflash_read(paddr, seg.vaddr as *mut u32, seg.len);
                    if rc != 0 {
                        halt("flash read failed while loading");
                    }
                }
            }
        }
        // Unmask the cache regions the app uses: IRAM0 (bit 0), DRAM1 (bit 3), DROM0 (bit 4).
        const MASK: u32 = (1 << 0) | (1 << 3) | (1 << 4);
        reg_clear_bits(0x3FF0_0044, MASK); // DPORT_PRO_CACHE_CTRL1
        reg_clear_bits(0x3FF0_005C, MASK); // DPORT_APP_CACHE_CTRL1
        // Only the PRO CPU cache: the APP CPU cache memory overlaps our own IRAM.
        Cache_Read_Enable_rom(0);
        uart_tx_flush(0);

        // Hand over with the FPU (coprocessor 0) enabled, as ESP-IDF's
        // bootloader does. esp-hal's interrupt entry saves FPU state and
        // takes a Coprocessor0Disabled exception otherwise.
        core::arch::asm!("wsr.cpenable {0}", "rsync", in(reg) 1u32);

        let entry: extern "C" fn() -> ! = core::mem::transmute(info.header.entry as usize);
        entry()
    }
}

/// Report an unrecoverable error periodically and reset after a while, so a
/// device that lost its image keeps telling anyone listening on the serial port.
pub fn halt(msg: &str) -> ! {
    for _ in 0..12 {
        println!("bootloader halted: {}", msg);
        unsafe { ets_delay_us(5_000_000) };
    }
    unsafe {
        uart_tx_flush(0);
        software_reset()
    }
}
