#!/usr/bin/env bash
# Builds the bootable disk images and copies them somewhere predictable, with
# the ext4 disk that goes with them: dist/bios.img, dist/uefi.img and
# dist/disk.qcow2.
#
# `fullkernel`'s build script creates both images inside its OUT_DIR and tells
# the crate where they are with UEFI_PATH / BIOS_PATH. OUT_DIR contains a cargo
# metadata hash that changes whenever the build graph does, and stale hash
# directories from earlier builds are left behind — this repository has a dozen
# — so the paths cannot be guessed or globbed. Cargo will say where they are if
# asked in JSON, which is what this does.
#
# Usage: scripts/build-images.sh [--profile dev|release] [--out DIR]
#        SIMPLOS_FB_WIDTH / SIMPLOS_FB_HEIGHT change the built-in mode.
set -euo pipefail

profile=release
out=dist
while (($#)); do
    case $1 in
    --profile)
        profile=${2:?--profile needs dev or release}
        shift 2
        ;;
    --out)
        out=${2:?--out needs a directory}
        shift 2
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root/fullkernel"

# `json-render-diagnostics` keeps the machine-readable stream on stdout while
# still printing warnings and errors as text, so a failing build reads normally.
build=(cargo build --message-format=json-render-diagnostics)
[[ $profile == dev ]] || build+=("--profile" "$profile")

echo "Building the disk images (profile: $profile)..." >&2
messages=$("${build[@]}")

# The build script of this crate, not of its dependencies: the bootloader's own
# build script exports paths with similar names for the four BIOS stages.
paths=$(
    PACKAGE="simplos-os" python3 -c '
import json, os, sys

wanted = os.environ["PACKAGE"]
found = {}
for line in sys.stdin:
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("reason") != "build-script-executed":
        continue
    if f"#{wanted}@" not in message.get("package_id", ""):
        continue
    found.update(dict(message.get("env") or []))

for key in ("BIOS_PATH", "UEFI_PATH", "PROBE_PATH"):
    if key not in found:
        sys.exit(f"{key} was not reported by {wanted}’s build script.")
    print(found[key])
' <<<"$messages"
)
read -r bios uefi probe <<<"$(tr '\n' ' ' <<<"$paths")"

mkdir -p "$root/$out"
install -m 644 "$bios" "$root/$out/bios.img"
install -m 644 "$uefi" "$root/$out/uefi.img"

# The ext4 disk, made as `cargo run` makes its own (`disk` in
# fullkernel/src/main.rs; keep the two in step): hello.txt, the user program
# as /bin/probe, the userland and GCC. Unlike `cargo run`, the userland and
# the toolchain have to build: a release without busybox, zsh, vim and gcc
# isn't one. 1 GiB to the guest, but compressed qcow2 holds only what mke2fs
# wrote, so the file is a fraction of that, and QEMU attaches it as it is:
# -drive format=qcow2,file=disk.qcow2. The toolchain over the userland
# replaces what's there rather than writing through it, as binutils replace
# busybox's links.
"$root/scripts/userland.sh"
"$root/scripts/toolchain.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/tree/bin"
echo "hello from the ext4 disk" >"$work/tree/hello.txt"
cp "$probe" "$work/tree/bin/probe"
cp -a "$root/target/userland/root/." "$work/tree/"
cp -a --remove-destination "$root/target/toolchain/root/." "$work/tree/"
PATH=$PATH:/usr/sbin:/sbin mke2fs -q -t ext4 -d "$work/tree" "$work/disk.img" 1G
qemu-img convert -c -f raw -O qcow2 "$work/disk.img" "$root/$out/disk.qcow2"

echo "Images in $out/:" >&2
ls -l "$root/$out/bios.img" "$root/$out/uefi.img" "$root/$out/disk.qcow2" >&2
