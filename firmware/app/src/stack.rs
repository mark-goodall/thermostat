//! Main stack high-water mark. RAM beside the radio is tight and the main
//! stack is whatever the linker leaves, so measure what is actually used:
//! the free part is painted at boot and later scanned for the deepest write.

use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    static _stack_start_cpu0: u32;
    static _stack_end_cpu0: u32;
}

const PATTERN: u32 = 0xA5A5_5A5A;
/// Leave esp-hal's stack guard word (just above the end) alone.
const GUARD_SKIP: usize = 128;

static REPORTED: AtomicUsize = AtomicUsize::new(0);

fn bounds() -> (usize, usize) {
    // The stack grows down from `start` to `end`.
    (
        &raw const _stack_end_cpu0 as usize,
        &raw const _stack_start_cpu0 as usize,
    )
}

/// Stack size in bytes.
pub fn size() -> usize {
    let (end, start) = bounds();
    start - end
}

/// Fill the unused stack below the caller with the pattern. Call early.
#[inline(never)]
pub fn paint() {
    let marker = 0u32;
    let sp = core::hint::black_box(&marker) as *const u32 as usize;
    let (end, _) = bounds();
    let mut p = (end + GUARD_SKIP) & !3;
    // Stay clear of this function's own frame.
    while p + 512 < sp {
        unsafe { (p as *mut u32).write_volatile(PATTERN) };
        p += 4;
    }
}

/// Deepest stack use since boot, in bytes.
pub fn peak_used() -> usize {
    let (end, start) = bounds();
    let mut p = (end + GUARD_SKIP) & !3;
    while p < start && unsafe { (p as *const u32).read_volatile() } == PATTERN {
        p += 4;
    }
    start - p
}

/// Log the high-water mark when it has grown by 256 bytes or more.
pub fn report() {
    let used = peak_used();
    if used >= REPORTED.load(Ordering::Relaxed) + 256 {
        REPORTED.store(used, Ordering::Relaxed);
        log::info!("stack: peak {used} of {} bytes", size());
    }
}
