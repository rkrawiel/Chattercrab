//! Warns when OpenH264 will be built without its assembly.
//!
//! `openh264-sys2` silently drops the SIMD assembly when `nasm` is not on `PATH`. The
//! decoder then runs about 50% slower. On Linux that decoder shows every screen share
//! (there is no hardware decode there), so a build without it drops frames.

use std::process::{Command, Stdio};

fn main() {
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    println!("cargo:rerun-if-changed=build.rs");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !matches!(arch.as_str(), "x86_64" | "x86") || std::env::var_os("OPENH264_NO_ASM").is_some() {
        return;
    }
    let found = Command::new("nasm")
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !found {
        println!(
            "cargo:warning=nasm not found: OpenH264 is built without assembly and decodes \
             screen shares about 50% slower (choppy on Linux). Install nasm and rebuild: \
             Debian/Ubuntu: sudo apt-get install nasm | Arch/Omarchy: sudo pacman -S nasm. \
             Then run: cargo clean -p openh264-sys2"
        );
    }
}
