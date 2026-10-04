//! The framebuffer mode the kernel asks for.
//!
//! `SIMPLOS_FB_WIDTH` / `SIMPLOS_FB_HEIGHT` override it, defaulting to the
//! 1920x1080 that was hardcoded here before. The *same two variables* are read
//! by the vendored bootloader's BIOS stage-2, which is where the BIOS path's
//! VESA cap lives — stage-2 runs in real mode long before any kernel config
//! exists, so it cannot be told at runtime. One switch, both firmware paths,
//! no hand-syncing.
//!
//! The values are written out as a generated source file rather than read with
//! `option_env!` because they are consumed inside the `const`-evaluated
//! `BOOTLOADER_CONFIG` block, and integer parsing is not available in `const`.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-env-changed=SIMPLOS_FB_WIDTH");
    println!("cargo::rerun-if-env-changed=SIMPLOS_FB_HEIGHT");

    let dimension = |name: &str, default: u64| -> u64 {
        match env::var(name) {
            Ok(value) => {
                value.parse().unwrap_or_else(|_| panic!("{name} must be a number, got {value:?}"))
            }
            Err(_) => default,
        }
    };
    let width = dimension("SIMPLOS_FB_WIDTH", 1920);
    let height = dimension("SIMPLOS_FB_HEIGHT", 1080);

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("framebuffer.rs");
    fs::write(
        &out,
        format!(
            "/// Framebuffer width requested at build time (`SIMPLOS_FB_WIDTH`).\n\
             pub const FRAMEBUFFER_WIDTH: u64 = {width};\n\
             /// Framebuffer height requested at build time (`SIMPLOS_FB_HEIGHT`).\n\
             pub const FRAMEBUFFER_HEIGHT: u64 = {height};\n"
        ),
    )
    .expect("write framebuffer.rs");
}
