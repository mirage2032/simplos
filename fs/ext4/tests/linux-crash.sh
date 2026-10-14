#!/bin/sh
# Makes linux-crash.img.gz: an ext4 file system as Linux's own driver left
# it when the power went, with changes in its journal that aren't yet in
# their places. Needs root, to mount it, and python3. Run once: the image
# is committed, and the tests only read it.
set -eu
out=$(cd "$(dirname "$0")" && pwd)/linux-crash.img.gz
work=$(mktemp -d)
trap 'umount "$work/mnt" 2>/dev/null || true; rm -rf "$work"' EXIT
uuid=5ab1e0e5-0000-4000-8000-00000000e4e4
mke2fs -q -F -t ext4 -U $uuid -E hash_seed=$uuid "$work/fs.img" 16M
mkdir "$work/mnt"
mount -o loop "$work/fs.img" "$work/mnt"
cd "$work/mnt"

# In place on the disk: a directory a few blocks long, and a file.
mkdir old
for i in $(seq 0 299); do echo "$i" > "old/f$i"; done
echo kept > kept.txt
sync

# Only in the journal: old's blocks freed (which the journal revokes), a new
# directory and files, and a move. fsync commits the journal and leaves the
# blocks themselves for later. Of new's files, only those fsynced have their
# data written: Linux hasn't yet found places for the others'.
rm -r old
mkdir new
for i in $(seq 0 299); do echo "new $i" > "new/f$i"; done
echo "written by linux" > linux.txt
mv kept.txt new/kept.txt
sync linux.txt new/f0 new/f150 new/f299 new .
# EXT4_IOC_SHUTDOWN, EXT4_GOING_FLAGS_LOGFLUSH: as if the power went.
python3 -c 'import fcntl, os, struct; fcntl.ioctl(os.open(".", os.O_RDONLY), 0x8004587d, struct.pack("I", 1))'

# Unmounting would write the blocks in place, so the disk is copied first.
cp "$work/fs.img" "$work/crash.img"
cd /
debugfs -R "ls -p /" "$work/crash.img" 2>/dev/null | grep -q /old/ \
    || { echo "the changes reached their places: no crash to recover from" >&2; exit 1; }
gzip -9n < "$work/crash.img" > "$out"
