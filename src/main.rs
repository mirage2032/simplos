fn main() {
    // read env variables that were set in build script
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");

    // choose whether to start the UEFI or BIOS image
    let uefi = true;

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        cmd.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
        cmd.arg("-drive").arg(format!("format=raw,file={uefi_path}")).arg("-accel").arg("kvm");
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={bios_path}")).arg("-accel").arg("kvm");
    }
    //append env args to cmd
    std::env::args().skip(1).for_each(|arg| {
        cmd.arg(arg);
    });
    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}