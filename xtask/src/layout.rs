//! Flash layout of the thermostat (4 MiB part).
//!
//! | offset   | size     | contents                                      |
//! |----------|----------|-----------------------------------------------|
//! | 0x1000   | 0x7000   | second-stage bootloader (embassy-boot-esp)    |
//! | 0x8000   | 0x1000   | partition table                               |
//! | 0x9000   | 0x2000   | embassy-boot swap state                       |
//! | 0x10000  | 0x10000  | settings (wear-levelled record log)           |
//! | 0x20000  | 0x1C0000 | active application (executes in place)       |
//! | 0x1E0000 | 0x1C1000 | DFU: staged update / previous image after swap |
//! | 0x3A1000 | 0x5F000  | spare                                         |

use embassy_boot_esp::partition::{self as p, Partition};
use thermostat_core::settings::{SETTINGS_SUBTYPE, SETTINGS_TYPE};

pub const BOOTLOADER_OFFSET: u32 = 0x1000;
pub const FLASH_SIZE: u32 = 4 * 1024 * 1024;

pub fn partitions() -> Vec<Partition> {
    vec![
        Partition::new("bootstate", p::TYPE_BOOT, p::SUBTYPE_STATE, 0x9000, 0x2000),
        Partition::new(
            "settings",
            SETTINGS_TYPE,
            SETTINGS_SUBTYPE,
            0x10000,
            0x10000,
        ),
        Partition::new("active", p::TYPE_APP, p::SUBTYPE_FACTORY, 0x20000, 0x1C0000),
        Partition::new("dfu", p::TYPE_BOOT, p::SUBTYPE_DFU, 0x1E0000, 0x1C1000),
    ]
}

pub fn active() -> Partition {
    partitions()
        .into_iter()
        .find(|p| p.label() == "active")
        .unwrap()
}

pub fn table_bin() -> Vec<u8> {
    use md5::{Digest, Md5};
    let parts = partitions();
    let mut entries = Vec::new();
    for part in &parts {
        let mut e = [0u8; p::ENTRY_LEN];
        part.write(&mut e);
        entries.extend_from_slice(&e);
    }
    let md5: [u8; 16] = Md5::digest(&entries).into();
    let table = p::encode(&parts, Some(md5));
    p::BootPartitions::from_table(&table).expect("layout satisfies embassy-boot constraints");
    table
}
