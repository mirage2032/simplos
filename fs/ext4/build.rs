//! Builds the vendored lwext4 (`vendor/lwext4`) and this crate's C glue,
//! configured for simplos.

use std::path::Path;

fn main() {
    let lwext4 = Path::new("../../vendor/lwext4");
    let mut build = cc::Build::new();
    build
        .include(lwext4.join("include"))
        .file("src/glue.c")
        // Prototypes for the malloc hooks, which lwext4 calls undeclared.
        .flag("-include")
        .flag("src/memory.h");
    for entry in std::fs::read_dir(lwext4.join("src")).expect("vendor/lwext4/src") {
        build.file(entry.expect("a source file").path());
    }
    build
        // Settings come from these, not from a header CMake would generate.
        .define("CONFIG_USE_DEFAULT_CFG", "1")
        .define("CONFIG_DEBUG_PRINTF", "0")
        // Assertions stay on, through the platform's `assert`.
        .define("CONFIG_HAVE_OWN_ASSERT", "0")
        // lwext4's own errno values (Linux's), so no <errno.h> is needed.
        .define("CONFIG_HAVE_OWN_ERRNO", "1")
        // malloc and friends come from `src/memory.rs`, on Rust's allocator.
        .define("CONFIG_USE_USER_MALLOC", "1")
        // `MOUNTS` in src/lib.rs: file systems mounted at once.
        .define("CONFIG_EXT4_MOUNTPOINTS_COUNT", "4")
        .define("CONFIG_EXT4_BLOCKDEVS_COUNT", "4")
        // Upstream code with its own style: its warnings aren't ours to fix.
        .warnings(false)
        .compile("lwext4");
    println!("cargo:rerun-if-changed=src/glue.c");
    println!("cargo:rerun-if-changed=src/memory.h");
    println!("cargo:rerun-if-changed=../../vendor/lwext4");
}
