//! Writing, then letting e2fsprogs judge: e2fsck must find nothing wrong,
//! and debugfs must read back what was written.

mod common;

use common::*;
use ext4::{Error, Ext4, File};

fn write_all(file: &mut File, mut buf: &[u8]) {
    while !buf.is_empty() {
        let written = file.write(buf).unwrap();
        buf = &buf[written..];
    }
}

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
    let mut fs = Ext4::mount_writable(Box::new(image.clone())).expect("mounts");

    fs.create_dir("new").unwrap();
    fs.create_dir("new/deeper").unwrap();
    write_all(&mut fs.create("new/deeper/bigger.bin").unwrap(), &bigger());
    // Across the end of a block, in a file that's there.
    let mut file = fs.open_writable("big.bin").unwrap();
    file.seek(4000).unwrap();
    write_all(&mut file, &[0xaa; 200]);
    drop(file);
    // Emptied first.
    write_all(&mut fs.create("hello.txt").unwrap(), b"rewritten\n");
    fs.rename("dir/nested/deep.txt", "new/deep.txt").unwrap();
    // A directory: its `..` must follow it.
    fs.rename("dir", "new/dir").unwrap();
    fs.remove_file("empty").unwrap();
    fs.remove_file("link").unwrap();
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
    assert_eq!(debugfs_ls(&saved, "/"), ["big.bin", "hello.txt", "lost+found", "new"]);
    assert_eq!(debugfs_ls(&saved, "new"), ["deep.txt", "deeper", "dir"]);
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
        write_all(
            &mut fs.create(&format!("many/file-{i:04}")).unwrap(),
            format!("{i}\n").as_bytes(),
        );
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
    let mut file = fs.create("full").unwrap();
    let mut total = 0;
    loop {
        match file.write(&[0x5a; 64 << 10]) {
            Ok(written) => total += written,
            Err(Error::NoSpace) => break,
            Err(error) => panic!("after {total} bytes: {error:?}"),
        }
    }
    assert!(total > 6 << 20, "full after only {total} bytes");
    assert_eq!(file.size(), total as u64);
    drop(file);
    drop(fs);
    let saved = dir.join("full.img");
    image.save(&saved);
    fsck(&saved).unwrap();

    // Room again once it's gone.
    let mut fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
    fs.remove_file("full").unwrap();
    write_all(&mut fs.create("after").unwrap(), &[0x5a; 1 << 20]);
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
    let mut fs = Ext4::mount_writable(Box::new(image.clone())).unwrap();
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
    assert_eq!(fs.rename("hello.txt", "empty").err(), Some(Error::AlreadyExists));
    // lwext4 alone would cut `dir` off, in a loop of its own.
    assert_eq!(fs.rename("dir", "dir/nested/dir").err(), Some(Error::InvalidArgument));
    // lwext4 alone would write to a file opened for reading.
    assert_eq!(fs.open("hello.txt").unwrap().write(b"no").err(), Some(Error::ReadOnly));
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
    let mut fs = Ext4::mount(Box::new(image.clone())).unwrap();
    assert_eq!(fs.create("new").err(), Some(Error::ReadOnly));
    assert_eq!(fs.open_writable("hello.txt").err(), Some(Error::ReadOnly));
    assert_eq!(fs.create_dir("new").err(), Some(Error::ReadOnly));
    assert_eq!(fs.rename("hello.txt", "moved").err(), Some(Error::ReadOnly));
    assert_eq!(fs.remove_file("hello.txt").err(), Some(Error::ReadOnly));
    assert_eq!(fs.open("hello.txt").unwrap().write(b"no").err(), Some(Error::ReadOnly));
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
