#!/usr/bin/env bash
# Boots the built images and checks the kernel actually came up.
#
# The integration tests in kernel/tests/ run test kernels through the BIOS path
# only. This runs the real thing, both ways, and is the check that would have
# caught most of what has broken this project: a bootloader stage that no longer
# loads, a firmware path nobody exercised, a framebuffer mode silently falling
# back. The kernel's boot banner is the evidence — it names the firmware and the
# mode it ended up in, so a wrong one fails here instead of shipping.
#
# Usage: scripts/smoke-test.sh [--images DIR] [--expect WIDTHxHEIGHT]
set -euo pipefail

images=dist
expect=1920x1080
while (($#)); do
    case $1 in
    --images)
        images=${2:?--images needs a directory}
        shift 2
        ;;
    --expect)
        expect=${2:?--expect needs WIDTHxHEIGHT}
        shift 2
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
logs=$(mktemp -d)
trap 'rm -rf "$logs"' EXIT

# OVMF for the UEFI path, from the same crate the `cargo run` wrapper uses, so
# this needs no firmware installed on the machine.
firmware=$(find ~/.cargo/registry/src -name 'OVMF-pure-efi.fd' 2>/dev/null | head -1 || true)

status=0
boot() {
    local mode=$1 image=$2 log="$logs/$1.log"
    shift 2
    echo "--- $mode ---"
    # `-display none` because there is nobody to look at it, and TCG because CI
    # has no KVM. A kernel that has reported its mode has nothing left to prove,
    # but it never exits on its own, so the timeout is the normal way out.
    timeout 120 qemu-system-x86_64 \
        -drive "format=raw,file=$image" \
        -accel tcg -cpu qemu64 -m 256M -display none -no-reboot \
        -chardev "file,id=serial0,path=$log" -serial chardev:serial0 \
        "$@" >/dev/null 2>&1 || true

    local banner
    banner=$(grep -a -m1 "^simplos: boot=" "$log" 2>/dev/null || true)
    if [[ -z $banner ]]; then
        echo "::error::$mode: the kernel never reported a boot banner."
        sed 's/^/    /' "$log" 2>/dev/null | tail -20
        status=1
        return
    fi
    echo "    $banner"

    if [[ $banner != *"boot=${mode}"* ]]; then
        echo "::error::$mode: the kernel reports a different firmware: $banner"
        status=1
    fi
    if [[ $banner != *"fb=${expect} "* ]]; then
        echo "::error::$mode: expected fb=$expect, got: $banner"
        status=1
    fi
    # A panic still prints a banner first, so the banner alone isn't enough.
    if grep -qa "KERNEL PANIC\|EXCEPTION: DOUBLE FAULT\|EXCEPTION: PAGE FAULT" "$log"; then
        echo "::error::$mode: the kernel faulted after booting."
        grep -a -A6 "KERNEL PANIC\|EXCEPTION:" "$log" | sed 's/^/    /'
        status=1
    fi
}

boot BIOS "$images/bios.img"
if [[ -n $firmware ]]; then
    boot UEFI "$images/uefi.img" -bios "$firmware"
else
    echo "::error::UEFI: no OVMF firmware found; build fullkernel once so ovmf-prebuilt is unpacked."
    status=1
fi

# The point of the fw_cfg channel: the host asks for a smaller mode and gets it.
expect=1024x768
boot BIOS "$images/bios.img" -fw_cfg name=opt/simplos/fb,string=1024x768

((status == 0)) && echo "All good."
exit "$status"
