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
    let image = image(&dir, &root, &["-t", "ext4"], "16M");
    let mut disk = image.bytes();
    // The same changes, mounted again each time: the journal must tell one
    // time's transactions from another's.
    let mut hello = String::from("hello.txt");
    let mut removed = String::from("dir/nested/deep.txt");
    for time in ["a", "b"] {
        let mut fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
        fs.create_dir(time).unwrap();
        let mut file = fs.create(&format!("{time}/new.txt")).unwrap();
        assert_eq!(file.write(b"new\n").unwrap(), 4);
        drop(file);
        fs.rename(&hello, &format!("{time}/hello.txt")).unwrap();
        hello = format!("{time}/hello.txt");
        fs.remove_file(&removed).unwrap();
        removed = format!("{time}/new.txt");
    }

    let writes = image.writes.lock().unwrap().clone();
    let crashed = dir.join("crashed.img");
    for cut in 0..=writes.len() {
        if let Some((sector, data)) = cut.checked_sub(1).map(|last| &writes[last]) {
            let start = *sector as usize * ext4::SECTOR;
            disk[start..start + data.len()].copy_from_slice(data);
        }

        // e2fsck replaying the journal and nothing more, as Linux would.
        std::fs::write(&crashed, &disk).unwrap();
        let replay = e2fsprogs("e2fsck").args(["-y", "-E", "journal_only"]).arg(&crashed).output();
        let replay = replay.expect("e2fsck, from e2fsprogs");
        assert!(
            replay.status.success(),
            "after {cut} writes, e2fsck can't replay the journal:\n{}{}",
            String::from_utf8_lossy(&replay.stdout),
            String::from_utf8_lossy(&replay.stderr)
        );
        fsck(&crashed).unwrap_or_else(|problems| {
            panic!("after {cut} writes, replayed by e2fsck:\n{problems}")
        });

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
                assert!(
                    new.is_empty() || new == b"new\n",
                    "after {cut} writes, new.txt holds {new:?}"
                );
            }
        }
        if fs.metadata("dir/nested/deep.txt").is_ok() {
            assert_eq!(read_all(&fs, "dir/nested/deep.txt"), b"deep\n", "after {cut} writes");
        }
        assert!(read_all(&fs, "big.bin") == big(), "after {cut} writes, big.bin changed");
        drop(fs);
        image.save(&crashed);
        fsck(&crashed)
            .unwrap_or_else(|problems| panic!("after {cut} writes, recovered:\n{problems}"));
    }
}
