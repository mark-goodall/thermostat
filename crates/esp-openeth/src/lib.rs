//! `embassy-net` driver for the OpenCores 10/100 Ethernet MAC that Espressif's
//! QEMU attaches to the emulated ESP32 (`-nic user,model=open_eth`).
//!
//! The MAC does not exist on real silicon; this driver lets the networking
//! code (web server, HTTP OTA) run unchanged in the emulator and in CI.
//!
//! Register map and behaviour follow QEMU's `hw/net/opencores_eth.c`. The
//! MAC DMAs frames to and from buffers in internal DRAM through descriptors
//! that live in the MAC itself (at `base + 0x400`).
//!
//! Wake-ups come from the `ETH_MAC` interrupt: bind [`on_interrupt`] to it.
#![no_std]

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Context;

use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embassy_sync::waitqueue::AtomicWaker;

/// `DR_REG_EMAC_BASE` on the ESP32.
pub const BASE: usize = 0x3FF6_9000;

mod reg {
    pub const MODER: usize = 0x00;
    pub const INT_SOURCE: usize = 0x04;
    pub const INT_MASK: usize = 0x08;
    pub const PACKETLEN: usize = 0x18;
    pub const TX_BD_NUM: usize = 0x20;
    pub const MAC_ADDR0: usize = 0x40;
    pub const MAC_ADDR1: usize = 0x44;
    pub const DESC: usize = 0x400;
}

const MODER_RXEN: u32 = 1 << 0;
const MODER_TXEN: u32 = 1 << 1;
const MODER_PRO: u32 = 1 << 5;
const MODER_FULLD: u32 = 1 << 10;
const MODER_RST: u32 = 1 << 11;
const MODER_CRCEN: u32 = 1 << 13;
const MODER_PAD: u32 = 1 << 15;

const INT_TXB: u32 = 1 << 0;
const INT_RXB: u32 = 1 << 2;
const INT_BUSY: u32 = 1 << 4;

const TXD_RD: u32 = 1 << 15;
const TXD_IRQ: u32 = 1 << 14;
const TXD_WR: u32 = 1 << 13;
const TXD_PAD: u32 = 1 << 12;
const TXD_CRC: u32 = 1 << 11;

const RXD_E: u32 = 1 << 15;
const RXD_IRQ: u32 = 1 << 14;
const RXD_WR: u32 = 1 << 13;
/// Error bits: overrun, invalid symbol, dribble, too long, short, CRC, late collision.
const RXD_ERR: u32 = 0x7F;

const TX_COUNT: usize = 2;
const RX_COUNT: usize = 4;
pub const MTU: usize = 1514;
const BUF_LEN: usize = 1600;

/// Frame buffers in internal DRAM, where the emulated MAC can DMA.
#[repr(C, align(4))]
struct Buffers {
    tx: [[u8; BUF_LEN]; TX_COUNT],
    rx: [[u8; BUF_LEN]; RX_COUNT],
}

static mut BUFFERS: Buffers = Buffers {
    tx: [[0; BUF_LEN]; TX_COUNT],
    rx: [[0; BUF_LEN]; RX_COUNT],
};
static WAKER: AtomicWaker = AtomicWaker::new();
static TAKEN: AtomicBool = AtomicBool::new(false);

fn read(off: usize) -> u32 {
    unsafe { read_volatile((BASE + off) as *const u32) }
}

fn write(off: usize, v: u32) {
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}

fn desc_flags(i: usize) -> u32 {
    read(reg::DESC + i * 8)
}

fn set_desc(i: usize, flags: u32, buf: *const u8) {
    // Pointer first: writing the flags word may start a transmission.
    write(reg::DESC + i * 8 + 4, buf as u32);
    write(reg::DESC + i * 8, flags);
}

/// Returns true if an emulated open_eth MAC answers at [`BASE`] (reset values
/// of TX_BD_NUM / PACKETLEN). On real hardware this reads unrelated registers
/// and returns false.
pub fn is_present() -> bool {
    read(reg::PACKETLEN) == 0x0040_0600
}

/// Interrupt handler body: acknowledge and wake the network stack.
pub fn on_interrupt() {
    let src = read(reg::INT_SOURCE);
    write(reg::INT_SOURCE, src);
    WAKER.wake();
}

pub struct OpenEth {
    mac: [u8; 6],
    tx_next: usize,
    rx_next: usize,
}

impl OpenEth {
    /// Reset and configure the MAC. Can only be called once.
    pub fn new(mac: [u8; 6]) -> Option<Self> {
        if TAKEN.swap(true, Ordering::AcqRel) || !is_present() {
            return None;
        }
        write(reg::MODER, MODER_RST);
        write(reg::MODER, 0);
        write(reg::TX_BD_NUM, TX_COUNT as u32);
        write(
            reg::MAC_ADDR0,
            u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]),
        );
        write(reg::MAC_ADDR1, u32::from_be_bytes([0, 0, mac[0], mac[1]]));
        let bufs = unsafe { &mut *addr_of_mut!(BUFFERS) };
        for i in 0..TX_COUNT {
            let wrap = if i == TX_COUNT - 1 { TXD_WR } else { 0 };
            set_desc(i, wrap, bufs.tx[i].as_ptr());
        }
        for i in 0..RX_COUNT {
            let wrap = if i == RX_COUNT - 1 { RXD_WR } else { 0 };
            set_desc(TX_COUNT + i, RXD_E | RXD_IRQ | wrap, bufs.rx[i].as_ptr());
        }
        write(reg::INT_SOURCE, 0xFFFF_FFFF);
        write(reg::INT_MASK, INT_RXB | INT_TXB | INT_BUSY);
        // Promiscuous: the emulated link only carries traffic for us anyway,
        // and it saves programming the multicast hash for IPv6/mDNS.
        write(
            reg::MODER,
            MODER_RXEN | MODER_TXEN | MODER_PRO | MODER_FULLD | MODER_CRCEN | MODER_PAD,
        );
        Some(Self {
            mac,
            tx_next: 0,
            rx_next: 0,
        })
    }

    fn rx_ready(&self) -> bool {
        desc_flags(TX_COUNT + self.rx_next) & RXD_E == 0
    }

    fn tx_free(&self) -> bool {
        desc_flags(self.tx_next) & TXD_RD == 0
    }
}

impl embassy_net_driver::Driver for OpenEth {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        WAKER.register(cx.waker());
        if self.rx_ready() && self.tx_free() {
            Some((
                RxToken {
                    next: &mut self.rx_next,
                },
                TxToken {
                    next: &mut self.tx_next,
                },
            ))
        } else {
            None
        }
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        WAKER.register(cx.waker());
        self.tx_free().then_some(TxToken {
            next: &mut self.tx_next,
        })
    }

    fn link_state(&mut self, _cx: &mut Context) -> LinkState {
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(1);
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

pub struct RxToken<'a> {
    next: &'a mut usize,
}

impl embassy_net_driver::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, f: F) -> R {
        let i = *self.next;
        let flags = desc_flags(TX_COUNT + i);
        let bufs = unsafe { &mut *addr_of_mut!(BUFFERS) };
        // The emulated MAC appends a 4 byte (zero) FCS to the reported length.
        let len = ((flags >> 16) as usize).saturating_sub(4).min(BUF_LEN);
        let len = if flags & RXD_ERR != 0 { 0 } else { len };
        let r = f(&mut bufs.rx[i][..len]);
        let wrap = if i == RX_COUNT - 1 { RXD_WR } else { 0 };
        set_desc(TX_COUNT + i, RXD_E | RXD_IRQ | wrap, bufs.rx[i].as_ptr());
        *self.next = (i + 1) % RX_COUNT;
        r
    }
}

pub struct TxToken<'a> {
    next: &'a mut usize,
}

impl embassy_net_driver::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let i = *self.next;
        let bufs = unsafe { &mut *addr_of_mut!(BUFFERS) };
        let len = len.min(BUF_LEN);
        let r = f(&mut bufs.tx[i][..len]);
        let wrap = if i == TX_COUNT - 1 { TXD_WR } else { 0 };
        set_desc(
            i,
            ((len as u32) << 16) | TXD_RD | TXD_IRQ | TXD_PAD | TXD_CRC | wrap,
            bufs.tx[i].as_ptr(),
        );
        *self.next = (i + 1) % TX_COUNT;
        r
    }
}
