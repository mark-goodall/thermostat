use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("bootloader.x", out.join("bootloader.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=bootloader.x");
    println!("cargo:rustc-link-arg-bins=-nostartfiles");
    println!("cargo:rustc-link-arg-bins=-Tbootloader.x");
    println!("cargo:rerun-if-env-changed=THERMOSTAT_FLASH_SIZE");
}
