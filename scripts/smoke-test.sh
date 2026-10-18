#!/usr/bin/env bash
# Boots the built images and checks the kernel actually came up.
#
# The integration tests in kernel/tests/ boot test kernels, not the one that
# ships. This boots the real images, both ways, and is the check that would have
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

# How long to give a boot before calling it hung. A 1080p framebuffer under an
# interpreted CPU is not fast, but it is nowhere near this slow.
readonly DEADLINE=120
# What the kernel's panic and fault handlers print. They halt rather than exit,
# so without looking for these a crash would wait out the whole deadline.
readonly FAULTS='KERNEL PANIC\|\*\*\* EXCEPTION:'
# A line a boot must also print after its banner, when set.
also=""

boot() {
    local mode=$1 image=$2
    shift 2
    # Separate statement: a single `local` expands every right-hand side before
    # it assigns any of them, so `$mode` would still be unset here.
    local log="$logs/$mode-$expect.log"
    echo "--- $mode, expecting $expect ---"
    : >"$log"

    # Started in the background and killed as soon as it has said what we need
    # to know. The kernel has no shutdown: it reports its mode and then runs its
    # render loop forever, so waiting for QEMU to exit means waiting out the
    # whole deadline on every boot — three of those is six minutes of nothing.
    #
    # `-display none` because there is nobody to look at it, and TCG because CI
    # has no KVM.
    qemu-system-x86_64 \
        -drive "format=raw,file=$image" \
        -accel tcg -cpu qemu64 -m 256M -display none -no-reboot \
        -chardev "file,id=serial0,path=$log" -serial chardev:serial0 \
        "$@" >/dev/null 2>&1 &
    local qemu=$!

    local banner="" waited=0
    while ((waited < DEADLINE)); do
        banner=$(grep -a -m1 "^simplos: boot=" "$log" 2>/dev/null || true)
        [[ -n $banner ]] && break
        grep -qa "$FAULTS" "$log" 2>/dev/null && break
        # A guest that died on its own (a triple fault, a QEMU error) will never
        # print anything, so stop waiting for it.
        kill -0 "$qemu" 2>/dev/null || break
        sleep 1
        waited=$((waited + 1))
    done
    while [[ -n $banner && -n $also ]] && ((waited < DEADLINE)); do
        grep -qa "$also" "$log" 2>/dev/null && break
        grep -qa "$FAULTS" "$log" 2>/dev/null && break
        kill -0 "$qemu" 2>/dev/null || break
        sleep 1
        waited=$((waited + 1))
    done
    # Give the kernel a moment past the banner, so a fault immediately after it
    # still lands in the log before we pull the plug.
    [[ -n $banner ]] && sleep 2
    kill "$qemu" 2>/dev/null || true
    wait "$qemu" 2>/dev/null || true

    banner=$(grep -a -m1 "^simplos: boot=" "$log" 2>/dev/null || true)
    if [[ -z $banner ]]; then
        echo "::error::$mode: the kernel never reported a boot banner (waited ${waited}s)."
        tail -20 "$log" 2>/dev/null | sed 's/^/    /'
        status=1
        return
    fi
    echo "    $banner  (after ${waited}s)"

    if [[ $banner != *"boot=${mode}"* ]]; then
        echo "::error::$mode: the kernel reports a different firmware: $banner"
        status=1
    fi
    if [[ $banner != *"fb=${expect} "* ]]; then
        echo "::error::$mode: expected fb=$expect, got: $banner"
        status=1
    fi
    if [[ -n $also ]] && grep -qa "$also" "$log"; then
        echo "    $also"
    elif [[ -n $also ]]; then
        echo "::error::$mode: the kernel never printed \"$also\" (waited ${waited}s)."
        tail -20 "$log" | sed 's/^/    /'
        status=1
    fi
    # A panic prints a banner first, so the banner alone isn't enough.
    if grep -qa "$FAULTS" "$log"; then
        echo "::error::$mode: the kernel faulted after booting."
        grep -a -A6 "$FAULTS" "$log" | sed 's/^/    /'
        status=1
    fi
}

boot BIOS "$images/bios.img"
if [[ -n $firmware ]]; then
    boot UEFI "$images/uefi.img" -bios "$firmware"
else
    echo "::error::UEFI: no OVMF firmware found; build tools/boot once so ovmf-prebuilt is unpacked."
    status=1
fi

# No PS/2 controller, as on many modern PCs: no keyboard, but it must still boot.
boot BIOS "$images/bios.img" -machine pc,i8042=off

# The ext4 disk that ships beside the images: the kernel has to find it and
# mount it. snapshot=on, so the boot's writes (its journal, at least) don't
# change the file about to be released.
if [[ -f $images/disk.qcow2 ]]; then
    also="simplos: ext4 on disk 1"
    boot BIOS "$images/bios.img" -drive "format=qcow2,file=$images/disk.qcow2,snapshot=on"
    also=""
fi

# The point of the fw_cfg channel: the host asks for a smaller mode and gets it.
expect=1024x768
boot BIOS "$images/bios.img" -fw_cfg name=opt/simplos/fb,string=1024x768

((status == 0)) && echo "All good."
exit "$status"
