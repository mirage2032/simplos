fn main() {
    // read env variables that were set in build script
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");

    // choose whether to start the UEFI or BIOS image
    let uefi = true;

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        cmd.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
        cmd.arg("-drive").arg(format!("format=raw,file={uefi_path}"));
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    }
    //append env args to cmd
    let mut use_accel_kvm = true;
    std::env::args().skip(1).for_each(|arg| {
        if arg == "--debug" {
            cmd.arg("-s").arg("-S");
            use_accel_kvm = false;
        }
        else {
            cmd.arg(&arg);
        }
    });
    if use_accel_kvm {
        cmd.arg("-accel").arg("kvm");
    }
    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}