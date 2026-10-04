use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    let local_path = Path::new(env!("CARGO_MANIFEST_DIR"));
    println!(
        "cargo:rustc-link-arg-bins=--script={}",
        local_path.join("stage-2-link.ld").display()
    );

    // LOCAL PATCH: the VESA mode cap, from the same SIMPLOS_FB_WIDTH/HEIGHT the
    // kernel's own build script reads. Stage-2 runs in real mode before any
    // kernel config exists, so this is the only way the BIOS path can be told
    // what resolution to pick. Unset, it keeps the previous hardcoded 1920x1080.
    println!("cargo::rerun-if-env-changed=SIMPLOS_FB_WIDTH");
    println!("cargo::rerun-if-env-changed=SIMPLOS_FB_HEIGHT");
    let dimension = |name: &str, default: u16| -> u16 {
        match env::var(name) {
            Ok(value) => value
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be a number, got {value:?}")),
            Err(_) => default,
        }
    };
    let width = dimension("SIMPLOS_FB_WIDTH", 1920);
    let height = dimension("SIMPLOS_FB_HEIGHT", 1080);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("vesa_cap.rs");
    fs::write(
        &out,
        format!(
            "/// Widest VESA mode stage-2 will select (`SIMPLOS_FB_WIDTH`).\n\
             const MAX_WIDTH: u16 = {width};\n\
             /// Tallest VESA mode stage-2 will select (`SIMPLOS_FB_HEIGHT`).\n\
             const MAX_HEIGHT: u16 = {height};\n"
        ),
    )
    .expect("write vesa_cap.rs");
}
