use std::path::PathBuf;

fn main() {
    // set by cargo, build scripts should use this directory for output files
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    // set by cargo's artifact dependency feature, see0x8000008490
    // https://doc.rust-lang.org/nightly/cargo/reference/unstable.html#artifact-dependencies
    //panic with all env vars
    // panic!("env vars: {:?}", std::env::vars());
    // "/home/alx/simplos/target/x86_64-unknown-none/debug/deps/artifact/simplos-b1fd25d037f61c13/bin/simplos-b1fd25d037f61c13"
    let kernel = PathBuf::from(std::env::var_os("CARGO_BIN_FILE_SIMPLOS_simplos").unwrap());
    //manifest dir
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let gdbinit_path = manifest_dir.join("../.gdbinit");
    let gdbinit_content = format!(
        "file {}\n\
        set architecture i386:x86-64\n\
        add-symbol-file {} -o 0x8000000000\n\
        ",
         kernel.to_string_lossy(),
         kernel.to_string_lossy()
    );
    println!("cargo:rerun-if-changed={}", gdbinit_path.display());
    std::fs::write(&gdbinit_path, gdbinit_content).unwrap();
    let lldbinit_path = manifest_dir.join("../.lldbinit");
    let lldbinit_content = format!(
        "target create {}\n\
        target modules load --file {} --slide 0x8000000000\n\
        process handle SIGTRAP -s false\n\
        ",
        kernel.to_string_lossy(),
        kernel.to_string_lossy()
    );
    std::fs::write(&lldbinit_path, lldbinit_content).unwrap();
    println!("cargo:rerun-if-changed={}", lldbinit_path.display());
    // create an UEFI disk image (optional)
    let uefi_path = out_dir.join("uefi.img");
    bootloader::UefiBoot::new(&kernel).create_disk_image(&uefi_path).unwrap();

    // create a BIOS disk image
    let bios_path = out_dir.join("bios.img");
    bootloader::BiosBoot::new(&kernel).create_disk_image(&bios_path).unwrap();

    // pass the disk image paths as env variables to the `main.rs`
    println!("cargo:rustc-env=UEFI_PATH={}", uefi_path.display());
    println!("cargo:rustc-env=BIOS_PATH={}", bios_path.display());
}