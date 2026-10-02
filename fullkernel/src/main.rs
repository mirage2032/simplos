fn main() {
    // read env variables that were set in build script
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");

    // choose whether to start the UEFI or BIOS image; pass `--bios` to use BIOS
    let args: Vec<String> = std::env::args().skip(1).collect();
    let uefi = !args.iter().any(|arg| arg == "--bios");

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        cmd.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
        cmd.arg("-drive").arg(format!("format=raw,file={uefi_path}"));
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    }
    
    // Always enable gdb server
    cmd.arg("-s");
    
    // Serial output to terminal for debugging
    cmd.arg("-serial").arg("stdio");
    
    // Process command line args
    args.iter().for_each(|arg| {
        if arg == "--debug" {
            cmd.arg("-S"); // Wait for debugger
        } else if arg != "--bios" {
            cmd.arg(arg);
        }
    });
    
    cmd.arg("-accel").arg("kvm");
    cmd.arg("-cpu").arg("host");
    
    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}