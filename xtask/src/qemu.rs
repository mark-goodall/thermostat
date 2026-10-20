//! Espressif QEMU integration: launching and end-to-end tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

pub enum Console {
    Stdio,
    /// TCP server on localhost; QEMU waits for a client before starting.
    Tcp(u16),
}

/// Locate the patched `qemu-system-xtensa` (see tools/qemu/README.md).
pub fn binary() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("QEMU_XTENSA") {
        return Ok(p.into());
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".local/share/esp-qemu/patched/bin/qemu-system-xtensa");
        if p.exists() {
            return Ok(p);
        }
    }
    bail!(
        "patched qemu-system-xtensa not found. Build it with tools/qemu/build.sh \
         (stock Espressif QEMU cannot run esp-hal firmware), or set QEMU_XTENSA"
    )
}

/// Raw Ethernet tap for tests: QEMU sends every frame on the emulated link to
/// `127.0.0.1:remote` and injects frames received on `127.0.0.1:local`.
/// (slirp, which carries the HTTP port forward, drops multicast.)
pub struct FrameTap {
    pub local: u16,
    pub remote: u16,
}

pub fn command(
    flash: &Path,
    http_port: u16,
    console: &Console,
    tap: Option<&FrameTap>,
) -> Result<Command> {
    let mut cmd = Command::new(binary()?);
    cmd.args(["-nographic", "-machine", "esp32"])
        .arg("-drive")
        .arg(format!("file={},if=mtd,format=raw", flash.display()));
    let slirp = format!("hostfwd=tcp:127.0.0.1:{http_port}-:80");
    match tap {
        None => {
            cmd.arg("-nic").arg(format!("user,model=open_eth,{slirp}"));
        }
        Some(tap) => {
            // A hub joins the NIC, slirp and the UDP frame tap.
            cmd.args(["-netdev", &format!("user,id=u0,{slirp}")])
                .args(["-netdev", "hubport,id=hp0,hubid=0,netdev=u0"])
                .args([
                    "-netdev",
                    &format!(
                        "dgram,id=d0,local.type=inet,local.host=127.0.0.1,local.port={},\
                         remote.type=inet,remote.host=127.0.0.1,remote.port={}",
                        tap.local, tap.remote
                    ),
                ])
                .args(["-netdev", "hubport,id=hp1,hubid=0,netdev=d0"])
                .args(["-nic", "hubport,hubid=0,model=open_eth"]);
        }
    }
    match console {
        Console::Stdio => {
            cmd.args(["-serial", "mon:stdio"]);
        }
        Console::Tcp(port) => {
            cmd.args([
                "-serial",
                &format!("tcp:127.0.0.1:{port},server=on,wait=on"),
                "-monitor",
                "none",
            ]);
        }
    }
    Ok(cmd)
}

pub fn run_tests(release: bool) -> Result<()> {
    crate::e2e::run(release)
}
