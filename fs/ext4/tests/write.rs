//! Writing, then letting e2fsprogs judge: e2fsck must find nothing wrong,
//! and debugfs must read back what was written.

mod common;

use common::*;
use ext4::{Changes, Error, Ext4, Kind};
use std::os::unix::fs::MetadataExt;

/// Unlike `big`'s, and a few bytes into its last block.
fn bigger() -> Vec<u8> {
    (0..(3 << 20) + 1234).map(|i: u32| (i % 253) as u8).collect()
}

/// Make an image with `options` and change it in every way there is.
fn writes_the_tree(test: &str, options: &[&str]) {
    let _serial = serial();
    let dir = workspace(test);
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, options, "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).expect("mounts");

    fs.create_dir("new").unwrap();
    fs.create_dir("new/deeper").unwrap();
    write_all(&fs.create("new/deeper/bigger.bin").unwrap(), 0, &bigger());
    // Across the end of a block, in a file that's there.
    write_all(&fs.open_writable("big.bin").unwrap(), 4000, &[0xaa; 200]);
    // Emptied first.
    write_all(&fs.create("hello.txt").unwrap(), 0, b"rewritten\n");
    fs.rename("dir/nested/deep.txt", "new/deep.txt").unwrap();
    // A directory: its `..` must follow it.
    fs.rename("dir", "new/dir").unwrap();
    // Over a file that's there, which goes.
    write_all(&fs.create("new/replacement").unwrap(), 0, b"replaced\n");
    fs.rename("new/replacement", "empty").unwrap();
    // Over an empty directory, which goes too.
    fs.create_dir("new/gone").unwrap();
    fs.create_dir("new/going").unwrap();
    fs.rename("new/going", "new/gone").unwrap();
    fs.remove_dir("new/gone").unwrap();
    fs.remove_file("link").unwrap();
    // Symbolic links: a short one, kept in its inode, and one too long for
    // that, in a block of its own.
    let long = "new/deeper/".repeat(8) + "bigger.bin";
    fs.symlink(b"hello.txt", "short").unwrap();
    fs.symlink(long.as_bytes(), "long").unwrap();
    assert_eq!(fs.metadata("short").unwrap().kind, Kind::Symlink);
    assert_eq!(fs.read_link("short").unwrap(), b"hello.txt");
    assert_eq!(fs.read_link("long").unwrap(), long.as_bytes());
    // A hard link: two names for one file.
    fs.link("big.bin", "new/also-big.bin").unwrap();
    assert_eq!(fs.metadata("big.bin").unwrap().links, 2);
    drop(fs);

    let saved = dir.join("written.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    let mut big = big();
    big[4000..4200].fill(0xaa);
    assert!(debugfs(&saved, "cat big.bin") == big, "big.bin reads back different");
    assert!(debugfs(&saved, "cat new/deeper/bigger.bin") == bigger(), "bigger.bin too");
    assert_eq!(debugfs(&saved, "cat hello.txt"), b"rewritten\n");
    assert_eq!(debugfs(&saved, "cat new/deep.txt"), b"deep\n");
    assert_eq!(debugfs(&saved, "cat empty"), b"replaced\n");
    assert_eq!(
        debugfs_ls(&saved, "/"),
        ["big.bin", "empty", "hello.txt", "long", "lost+found", "new", "short"]
    );
    assert_eq!(debugfs_ls(&saved, "new"), ["also-big.bin", "deep.txt", "deeper", "dir"]);
    let stat = |path| String::from_utf8(debugfs(&saved, &format!("stat {path}"))).unwrap();
    assert!(stat("short").contains("Fast link dest: \"hello.txt\""), "{}", stat("short"));
    assert!(debugfs(&saved, "cat new/also-big.bin") == big, "the hard link reads back different");
    assert!(stat("big.bin").contains("Links: 2"), "{}", stat("big.bin"));
    assert_eq!(debugfs_ls(&saved, "new/dir"), ["nested"]);
    assert!(debugfs_ls(&saved, "new/dir/nested").is_empty());

    // And here, mounted again.
    let fs = Ext4::mount(Box::new(image)).unwrap();
    assert!(read_all(&fs, "new/deeper/bigger.bin") == bigger());
}

#[test]
fn writes_ext4_as_mkfs_makes_it_by_default() {
    writes_the_tree("default", &["-t", "ext4"]);
}

#[test]
fn writes_ext4_without_metadata_checksums() {
    writes_the_tree("no_csum", &["-t", "ext4", "-O", "^metadata_csum"]);
}

#[test]
fn writes_ext4_without_64bit() {
    writes_the_tree("no_64bit", &["-t", "ext4", "-O", "^64bit"]);
}

#[test]
fn writes_ext4_with_1k_blocks() {
    writes_the_tree("1k_blocks", &["-t", "ext4", "-b", "1024"]);
}

#[test]
fn writes_ext3() {
    writes_the_tree("ext3", &["-t", "ext3"]);
}

#[test]
fn writes_ext2() {
    writes_the_tree("ext2", &["-t", "ext2"]);
}

#[test]
fn fills_a_directory_until_it_needs_an_index() {
    let _serial = serial();
    let dir = workspace("htree");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    fs.create_dir("many").unwrap();
    for i in 0..3000 {
        let file = fs.create(&format!("many/file-{i:04}")).unwrap();
        write_all(&file, 0, format!("{i}\n").as_bytes());
    }
    drop(fs);

    let saved = dir.join("written.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    assert_eq!(debugfs_ls(&saved, "many").len(), 3000);
    for i in [0, 1234, 2999] {
        assert_eq!(debugfs(&saved, &format!("cat many/file-{i:04}")), format!("{i}\n").as_bytes());
    }
}

#[test]
fn fills_up_and_stays_consistent() {
    let _serial = serial();
    let dir = workspace("full");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "16M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    let file = fs.create("full").unwrap();
    let mut total = 0;
    loop {
        match file.write_at(total as u64, &[0x5a; 64 << 10]) {
            Ok(written) => total += written,
            Err(Error::NoSpace) => break,
            Err(error) => panic!("after {total} bytes: {error:?}"),
        }
    }
    assert!(total > 6 << 20, "full after only {total} bytes");
    assert_eq!(file.size().unwrap(), total as u64);
    drop(file);
    drop(fs);
    let saved = dir.join("full.img");
    image.save(&saved);
    fsck(&saved).unwrap();

    // Room again once it's gone.
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    fs.remove_file("full").unwrap();
    write_all(&fs.create("after").unwrap(), 0, &[0x5a; 1 << 20]);
    drop(fs);
    image.save(&saved);
    fsck(&saved).unwrap();
}

#[test]
fn refuses_what_linux_refuses() {
    let _serial = serial();
    let dir = workspace("refuses");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    // lwext4 alone would make the missing directories on the way.
    assert_eq!(fs.create("missing/file").err(), Some(Error::NotFound));
    assert_eq!(fs.create_dir("missing/dir").err(), Some(Error::NotFound));
    assert_eq!(fs.rename("hello.txt", "missing/hello.txt").err(), Some(Error::NotFound));
    assert_eq!(fs.create("hello.txt/file").err(), Some(Error::NotADirectory));
    assert_eq!(fs.create("dir").err(), Some(Error::IsADirectory));
    assert_eq!(fs.open_writable("missing").err(), Some(Error::NotFound));
    // lwext4 alone would say all is well to these two.
    assert_eq!(fs.create_dir("dir").err(), Some(Error::AlreadyExists));
    assert_eq!(fs.remove_file("dir").err(), Some(Error::IsADirectory));
    assert_eq!(fs.create_dir("hello.txt").err(), Some(Error::AlreadyExists));
    assert_eq!(fs.remove_file("missing").err(), Some(Error::NotFound));
    assert_eq!(fs.rename("missing", "elsewhere").err(), Some(Error::NotFound));
    assert_eq!(fs.rename("hello.txt", "dir").err(), Some(Error::IsADirectory));
    assert_eq!(fs.rename("dir", "empty").err(), Some(Error::NotADirectory));
    assert_eq!(fs.remove_dir("dir").err(), Some(Error::NotEmpty));
    assert_eq!(fs.remove_dir("hello.txt").err(), Some(Error::NotADirectory));
    assert_eq!(fs.remove_dir("missing").err(), Some(Error::NotFound));
    assert_eq!(fs.remove_dir("/").err(), Some(Error::Busy));
    // lwext4 alone would cut `dir` off, in a loop of its own.
    assert_eq!(fs.rename("dir", "dir/nested/dir").err(), Some(Error::InvalidArgument));
    // lwext4 alone would write to a file opened for reading.
    assert_eq!(fs.open("hello.txt").unwrap().write_at(0, b"no").err(), Some(Error::ReadOnly));
    // Links: none where there's something, none to a directory, and reading
    // one that isn't.
    assert_eq!(fs.symlink(b"anything", "hello.txt").err(), Some(Error::AlreadyExists));
    assert_eq!(fs.symlink(b"anything", "missing/link").err(), Some(Error::NotFound));
    assert_eq!(fs.link("dir", "dir-again").err(), Some(Error::NotPermitted));
    assert_eq!(fs.link("hello.txt", "empty").err(), Some(Error::AlreadyExists));
    assert_eq!(fs.link("missing", "elsewhere").err(), Some(Error::NotFound));
    assert_eq!(fs.read_link("hello.txt").err(), Some(Error::InvalidArgument));
    drop(fs);

    let saved = dir.join("refused.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    assert_eq!(
        debugfs_ls(&saved, "/"),
        ["big.bin", "dir", "empty", "hello.txt", "link", "lost+found"]
    );
    assert_eq!(debugfs_ls(&saved, "dir"), ["nested"]);
    assert_eq!(debugfs(&saved, "cat hello.txt"), b"hello from ext4\n");
}

#[test]
fn read_only_mounts_write_nothing() {
    let _serial = serial();
    let dir = workspace("read_only");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount(Box::new(image.clone())).unwrap();
    assert_eq!(fs.create("new").err(), Some(Error::ReadOnly));
    assert_eq!(fs.open_writable("hello.txt").err(), Some(Error::ReadOnly));
    assert_eq!(fs.create_dir("new").err(), Some(Error::ReadOnly));
    assert_eq!(fs.rename("hello.txt", "moved").err(), Some(Error::ReadOnly));
    assert_eq!(fs.remove_file("hello.txt").err(), Some(Error::ReadOnly));
    assert_eq!(fs.open("hello.txt").unwrap().write_at(0, b"no").err(), Some(Error::ReadOnly));
    drop(fs);
    assert!(image.writes.lock().unwrap().is_empty());
}

#[test]
fn reads_but_wont_write_features_it_cant_keep_up() {
    let _serial = serial();
    // Quotas: lwext4 would read them, but write without counting.
    let dir = workspace("quota");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4", "-O", "quota"], "64M");
    assert_eq!(Ext4::mount_writable(Box::new(image.clone())).err(), Some(Error::Unsupported));
    let fs = Ext4::mount(Box::new(image.clone())).unwrap();
    assert_eq!(read_all(&fs, "hello.txt"), b"hello from ext4\n");
    drop(fs);
    assert!(image.writes.lock().unwrap().is_empty());
}

#[test]
fn open_files_keep_their_own_place() {
    let _serial = serial();
    let dir = workspace("places");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    write_all(&fs.create("f").unwrap(), 0, b"0123456789");
    let reader = fs.open("f").unwrap();
    let mut buf = [0; 16];
    assert_eq!(reader.read_at(4, &mut buf).unwrap(), 6);
    // Emptied through another handle: the first sees the new end, and lwext4
    // alone would read on past it.
    let writer = fs.create("f").unwrap();
    assert_eq!(reader.read_at(4, &mut buf).unwrap(), 0);
    write_all(&writer, 0, b"ab");
    assert_eq!(reader.read_at(0, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"ab");
    // Past the end: zeros in between, where lwext4 alone would put "z" at the
    // end.
    write_all(&writer, 6, b"z");
    assert_eq!(reader.read_at(0, &mut buf).unwrap(), 7);
    assert_eq!(&buf[..7], b"ab\0\0\0\0z");
    assert_eq!(reader.metadata().unwrap().size, 7);
    drop((reader, writer, fs));

    let saved = dir.join("written.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    assert_eq!(debugfs(&saved, "cat f"), b"ab\0\0\0\0z");
}

#[test]
fn open_files_cant_be_removed() {
    let _serial = serial();
    let dir = workspace("busy");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    let (first, second) = (fs.open("hello.txt").unwrap(), fs.open("hello.txt").unwrap());
    assert_eq!(fs.remove_file("hello.txt").err(), Some(Error::Busy));
    drop(first);
    assert_eq!(fs.remove_file("hello.txt").err(), Some(Error::Busy));
    drop(second);
    fs.remove_file("hello.txt").unwrap();
    drop(fs);
    let saved = dir.join("written.img");
    image.save(&saved);
    fsck(&saved).unwrap();
}

#[test]
fn files_keep_their_file_system_mounted() {
    let _serial = serial();
    let dir = workspace("outlive");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    let file = fs.create("late").unwrap();
    let listing = fs.open_dir("/").unwrap();
    drop(fs);
    write_all(&file, 0, b"after the mount's owner let go\n");
    assert_eq!(listing.metadata().unwrap().kind, Kind::Directory);
    drop(file);
    // Still mounted: what's on the image isn't finished yet.
    let saved = dir.join("written.img");
    image.save(&saved);
    assert!(debugfs_ls(&saved, "/").contains(&String::from("late")));
    drop(listing);
    // Unmounted now, cleanly, and the slot given back.
    image.save(&saved);
    fsck(&saved).unwrap();
    assert_eq!(debugfs(&saved, "cat late"), b"after the mount's owner let go\n");
    Ext4::mount(Box::new(image)).unwrap();
}

#[test]
fn says_what_stat_says() {
    let _serial = serial();
    let dir = workspace("stat");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount(Box::new(image)).unwrap();
    // As Linux would say of the tree mke2fs copied.
    for path in ["hello.txt", "dir", "big.bin", "link"] {
        let ours = fs.metadata(path).unwrap();
        let theirs = std::fs::symlink_metadata(root.join(path)).unwrap();
        assert_eq!(ours.permissions, theirs.mode() & 0o7777, "{path}");
        assert_eq!(ours.links, theirs.nlink() as u32, "{path}");
        assert_eq!(ours.modified as i64, theirs.mtime(), "{path}");
        if ours.kind != Kind::Directory {
            assert_eq!(ours.size, theirs.size(), "{path}");
        }
    }
    assert_eq!(fs.metadata("/").unwrap().inode, 2, "the root's inode");
    assert_eq!(fs.metadata("big.bin").unwrap().blocks, (3 << 20) / 512);
    // An entry's inode is the one its name leads to.
    for entry in fs.read_dir("/").unwrap() {
        assert_eq!(entry.inode, fs.metadata(&entry.name).unwrap().inode, "{}", entry.name);
    }
}

#[test]
fn replaces_and_removes_only_what_it_may() {
    let _serial = serial();
    let dir = workspace("replaces");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    // Not while it's open: lwext4 would free it under whoever has it.
    let open = fs.open("hello.txt").unwrap();
    write_all(&fs.create("new").unwrap(), 0, b"new\n");
    assert_eq!(fs.rename("new", "hello.txt").err(), Some(Error::Busy));
    drop(open);
    fs.rename("new", "hello.txt").unwrap();
    // Onto itself: nothing happens.
    fs.rename("hello.txt", "hello.txt").unwrap();
    // A directory that's open can't go either.
    fs.create_dir("empty-dir").unwrap();
    let listing = fs.open_dir("empty-dir").unwrap();
    assert_eq!(fs.remove_dir("empty-dir").err(), Some(Error::Busy));
    drop(listing);
    fs.remove_dir("empty-dir").unwrap();
    // A directory onto one that isn't empty.
    fs.create_dir("other").unwrap();
    assert_eq!(fs.rename("other", "dir").err(), Some(Error::NotEmpty));
    fs.set_permissions("hello.txt", 0o600).unwrap();
    assert_eq!(fs.metadata("hello.txt").unwrap().permissions, 0o600);
    drop(fs);

    let saved = dir.join("written.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    assert_eq!(debugfs(&saved, "cat hello.txt"), b"new\n");
    assert_eq!(
        debugfs_ls(&saved, "/"),
        ["big.bin", "dir", "empty", "hello.txt", "link", "lost+found", "other"]
    );
    let stat = String::from_utf8(debugfs(&saved, "stat hello.txt")).unwrap();
    assert!(stat.contains("Mode:  0600"), "{stat}");
}

#[test]
fn keeps_times_owners_and_sizes() {
    let _serial = serial();
    let dir = workspace("times");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4"], "64M");
    let fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    let times = |path| {
        let metadata = fs.metadata(path).unwrap();
        (metadata.accessed, metadata.modified, metadata.changed)
    };
    let made_at = times("hello.txt");
    // What's made has all three times set to when.
    ext4::set_clock(|| 1_000_000_000);
    write_all(&fs.create("new.txt").unwrap(), 0, b"0123456789");
    fs.create_dir("newdir").unwrap();
    fs.symlink(b"new.txt", "newlink").unwrap();
    for path in ["new.txt", "newdir", "newlink"] {
        assert_eq!(times(path), (1_000_000_000, 1_000_000_000, 1_000_000_000), "{path}");
    }
    // Writing sets when it was modified and changed, not read.
    ext4::set_clock(|| 1_100_000_000);
    let hello = fs.open_writable("hello.txt").unwrap();
    write_all(&hello, 0, b"H");
    assert_eq!(times("hello.txt"), (made_at.0, 1_100_000_000, 1_100_000_000));
    // Cut short, then longer, with zeros.
    let new = fs.open_writable("new.txt").unwrap();
    new.set_len(4).unwrap();
    assert_eq!(read_all(&fs, "new.txt"), b"0123");
    new.set_len(8192).unwrap();
    let mut longer = b"0123".to_vec();
    longer.resize(8192, 0);
    assert!(read_all(&fs, "new.txt") == longer, "grown with zeros");
    // Changed as asked, by path and through a handle: and its change time.
    ext4::set_clock(|| 1_200_000_000);
    let changes = Changes {
        uid: Some(1000),
        gid: Some(100),
        accessed: Some(5),
        modified: Some(6),
        ..Changes::default()
    };
    fs.change("new.txt", changes).unwrap();
    new.change(Changes { permissions: Some(0o600), ..Changes::default() }).unwrap();
    let metadata = fs.metadata("new.txt").unwrap();
    assert_eq!((metadata.permissions, metadata.uid, metadata.gid), (0o600, 1000, 100));
    assert_eq!(times("new.txt"), (5, 6, 1_200_000_000));
    // A link is a change to the file it names.
    fs.link("hello.txt", "hello-again.txt").unwrap();
    assert_eq!(times("hello.txt").2, 1_200_000_000);
    drop((hello, new, fs));

    let saved = dir.join("times.img");
    image.save(&saved);
    fsck(&saved).unwrap();
    let stat = String::from_utf8(debugfs(&saved, "stat new.txt")).unwrap();
    assert!(stat.contains("User:  1000   Group:   100") && stat.contains("Size: 8192"), "{stat}");
    assert!(debugfs(&saved, "cat new.txt") == longer, "new.txt reads back different");
}
