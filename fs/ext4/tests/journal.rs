//! Crashes, and recovering from them with the journal.

mod common;

use common::*;
use ext4::{Error, Ext4, Kind};
use std::process::Command;

/// Linux's own crash: `linux-crash.sh` says how it was made.
#[test]
fn recovers_what_linux_left_in_its_journal() {
    let _serial = serial();
    let dir = workspace("linux_crash");
    let crash = Command::new("gzip")
        .arg("-dc")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/linux-crash.img.gz"))
        .output()
        .expect("gzip");
    let image = Image::new(crash.stdout);
    // What's in place on the disk is out of date, and replaying the journal
    // would write: refused.
    assert_eq!(Ext4::mount(Box::new(image.clone())).err(), Some(Error::NeedsRecovery));

    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    let entry = |name: &str, kind| (String::from(name), kind);
    assert_eq!(
        names(fs.read_dir("/").unwrap()),
        [
            entry("linux.txt", Kind::File),
            entry("lost+found", Kind::Directory),
            entry("new", Kind::Directory),
        ]
    );
    assert_eq!(read_all(&fs, "linux.txt"), b"written by linux\n");
    assert_eq!(read_all(&fs, "new/kept.txt"), b"kept\n");
    assert_eq!(fs.read_dir("new").unwrap().len(), 301);
    for i in 0..300 {
        let (file, data) = (format!("new/f{i}"), format!("new {i}\n"));
        match i {
            // Fsynced.
            0 | 150 | 299 => assert_eq!(read_all(&fs, &file), data.as_bytes()),
            _ => assert!(read_all(&fs, &file).is_empty(), "{file} wasn't fsynced"),
        }
    }
    drop(fs);

    let saved = dir.join("recovered.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    // Nothing left to recover.
    Ext4::mount(Box::new(image)).unwrap();
}

/// Make changes, then go back over every write they made, and at each one
/// cut the power. Then recover, as Linux would and as we do: each change
/// must be whole or not made at all, and e2fsck must find nothing wrong.
#[test]
fn survives_a_crash_at_any_write() {
    let _serial = serial();
    let dir = workspace("crash");
    let root = dir.join("tree");
    make_tree(&root);
    std::fs::write(root.join("target.txt"), "original\n").unwrap();
    let image = image(&dir, &root, &["-t", "ext4"], "16M");
    let disk = image.bytes();
    // The same changes, mounted again each time: the journal must tell one
    // time's transactions from another's.
    let mut hello = String::from("hello.txt");
    let mut removed = String::from("dir/nested/deep.txt");
    for time in ["a", "b"] {
        let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
        fs.create_dir(time).unwrap();
        let file = fs.create(&format!("{time}/new.txt")).unwrap();
        assert_eq!(file.write_at(0, b"new\n").unwrap(), 4);
        drop(file);
        fs.rename(&hello, &format!("{time}/hello.txt")).unwrap();
        hello = format!("{time}/hello.txt");
        fs.remove_file(&removed).unwrap();
        removed = format!("{time}/new.txt");
        // A file written aside, then renamed over one that's there: the
        // usual way to replace a file whole.
        let file = fs.create(&format!("{time}/swap.txt")).unwrap();
        assert_eq!(file.write_at(0, format!("{time}\n").as_bytes()).unwrap(), 2);
        drop(file);
        fs.rename(&format!("{time}/swap.txt"), "target.txt").unwrap();
        fs.create_dir(&format!("{time}/gone")).unwrap();
        fs.remove_dir(&format!("{time}/gone")).unwrap();
    }

    let writes = image.writes.lock().unwrap().clone();
    // Each cut on its own, four at a time: e2fsck takes most of the time,
    // and four is as many mounts as lwext4 has.
    std::thread::scope(|scope| {
        for worker in 0..ext4::MOUNTS {
            let (dir, disk, writes) = (&dir, &disk, &writes);
            scope.spawn(move || {
                for cut in (worker..=writes.len()).step_by(ext4::MOUNTS) {
                    check_cut(dir, worker, cut, disk, &writes[..cut]);
                }
            });
        }
    });
}

/// Cut the power after `writes` have reached `disk`, and recover.
fn check_cut(dir: &std::path::Path, worker: usize, cut: usize, disk: &[u8], writes: &[Write]) {
    let mut disk = disk.to_vec();
    for (sector, data) in writes {
        let start = *sector as usize * ext4::SECTOR;
        disk[start..start + data.len()].copy_from_slice(data);
    }
    let crashed = dir.join(format!("crashed-{worker}.img"));
    // Mounted, it's marked as Linux marks it: not clean, but not as having
    // errors either, which would outlive a crash.
    let state = u16::from_le_bytes([disk[1024 + 0x3a], disk[1024 + 0x3b]]);
    assert_eq!(state & 2, 0, "after {cut} writes, the file system says it has errors");

    // e2fsck replaying the journal and nothing more, as Linux would.
    std::fs::write(&crashed, &disk).unwrap();
    replay(&crashed)
        .unwrap_or_else(|output| panic!("after {cut} writes, e2fsck can't replay:\n{output}"));
    fsck(&crashed)
        .unwrap_or_else(|problems| panic!("after {cut} writes, replayed by e2fsck:\n{problems}"));

    let image = Image::new(disk.clone());
    let fs = Ext4::mount_writable(Box::new(image.clone()))
        .unwrap_or_else(|error| panic!("after {cut} writes: {error:?}"));
    let hello: Vec<_> = ["hello.txt", "a/hello.txt", "b/hello.txt"]
        .into_iter()
        .filter(|path| fs.metadata(path).is_ok())
        .collect();
    assert_eq!(hello.len(), 1, "after {cut} writes, hello.txt is at {hello:?}");
    assert_eq!(read_all(&fs, hello[0]), b"hello from ext4\n", "after {cut} writes");
    for new in ["a/new.txt", "b/new.txt"] {
        if fs.metadata(new).is_ok() {
            let new = read_all(&fs, new);
            assert!(new.is_empty() || new == b"new\n", "after {cut} writes, new.txt holds {new:?}");
        }
    }
    // Never missing, and never anything but one of the three it's been.
    assert!(fs.metadata("target.txt").is_ok(), "after {cut} writes, target.txt is missing");
    let target = read_all(&fs, "target.txt");
    assert!(
        [&b"original\n"[..], b"a\n", b"b\n"].contains(&&target[..]),
        "after {cut} writes, target.txt holds {target:?}"
    );
    if fs.metadata("dir/nested/deep.txt").is_ok() {
        assert_eq!(read_all(&fs, "dir/nested/deep.txt"), b"deep\n", "after {cut} writes");
    }
    assert!(read_all(&fs, "big.bin") == big(), "after {cut} writes, big.bin changed");
    drop(fs);
    image.save(&crashed);
    fsck(&crashed).unwrap_or_else(|problems| panic!("after {cut} writes, recovered:\n{problems}"));
}
