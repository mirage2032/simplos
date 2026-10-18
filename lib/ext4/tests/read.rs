//! Reading file systems made by e2fsprogs' `mke2fs`, the tool Linux makes
//! them with, across the features it can turn on and off.

mod common;

use common::*;
use ext4::{Error, Ext4, Kind};
use std::fs;

/// Make an image with `options` and read the whole tree back.
fn reads_the_tree(test: &str, options: &[&str]) {
    let _serial = serial();
    let dir = workspace(test);
    let root = dir.join("tree");
    make_tree(&root);
    let fs = Ext4::mount(Box::new(image(&dir, &root, options, "64M"))).expect("mounts");

    let entry = |name: &str, kind| (String::from(name), kind);
    assert_eq!(
        names(fs.read_dir("/").unwrap()),
        [
            entry("big.bin", Kind::File),
            entry("dir", Kind::Directory),
            entry("empty", Kind::File),
            entry("hello.txt", Kind::File),
            entry("link", Kind::Symlink),
            entry("lost+found", Kind::Directory),
        ]
    );
    assert_eq!(names(fs.read_dir("dir").unwrap()), [entry("nested", Kind::Directory)]);
    assert_eq!(read_all(&fs, "/hello.txt"), b"hello from ext4\n");
    assert_eq!(read_all(&fs, "dir/nested/deep.txt"), b"deep\n");
    assert_eq!(read_all(&fs, "empty"), b"");
    assert!(read_all(&fs, "big.bin") == big(), "big.bin reads back different");

    assert_eq!(fs.metadata("dir").unwrap().kind, Kind::Directory);
    assert_eq!(fs.metadata("hello.txt").unwrap().kind, Kind::File);
    assert_eq!(fs.metadata("link").unwrap().kind, Kind::Symlink);
}

#[test]
fn reads_ext4_as_mkfs_makes_it_by_default() {
    reads_the_tree("default", &["-t", "ext4"]);
}

#[test]
fn reads_ext4_without_metadata_checksums() {
    reads_the_tree("no_csum", &["-t", "ext4", "-O", "^metadata_csum"]);
}

#[test]
fn reads_ext4_without_64bit() {
    reads_the_tree("no_64bit", &["-t", "ext4", "-O", "^64bit"]);
}

#[test]
fn reads_ext4_with_1k_blocks() {
    reads_the_tree("1k_blocks", &["-t", "ext4", "-b", "1024"]);
}

#[test]
fn reads_ext3() {
    reads_the_tree("ext3", &["-t", "ext3"]);
}

#[test]
fn reads_ext2() {
    reads_the_tree("ext2", &["-t", "ext2"]);
}

#[test]
fn reads_a_directory_big_enough_to_be_indexed() {
    let _serial = serial();
    // Thousands of entries: mke2fs gives the directory an htree index.
    let dir = workspace("htree");
    let root = dir.join("tree");
    fs::create_dir_all(root.join("many")).unwrap();
    for i in 0..3000 {
        fs::write(root.join(format!("many/file-{i:04}")), format!("{i}\n")).unwrap();
    }
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"], "64M"))).unwrap();
    assert_eq!(fs.read_dir("many").unwrap().len(), 3000);
    for i in [0, 1234, 2999] {
        assert_eq!(read_all(&fs, &format!("many/file-{i:04}")), format!("{i}\n").as_bytes());
    }
}

#[test]
fn reads_from_anywhere_in_a_file() {
    let _serial = serial();
    let dir = workspace("seek");
    let root = dir.join("tree");
    make_tree(&root);
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"], "64M"))).unwrap();
    let file = fs.open("big.bin").unwrap();
    // Across the end of one 4 KiB block into the next.
    let mut buf = [0; 100];
    assert_eq!(file.read_at(8190, &mut buf).unwrap(), 100);
    assert_eq!(buf[..], big()[8190..8290]);
}

#[test]
fn says_what_went_wrong() {
    let _serial = serial();
    let dir = workspace("errors");
    let root = dir.join("tree");
    make_tree(&root);
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"], "64M"))).unwrap();
    assert_eq!(fs.open("missing").err(), Some(Error::NotFound));
    assert_eq!(fs.open("dir").err(), Some(Error::IsADirectory));
    assert_eq!(fs.read_dir("hello.txt").err(), Some(Error::NotADirectory));
    assert_eq!(fs.metadata("dir/missing").err(), Some(Error::NotFound));
}

#[test]
fn refuses_features_it_cant_handle() {
    let _serial = serial();
    // Inline data puts small files inside their inodes, which lwext4 can't
    // read: refused, not misread.
    let dir = workspace("inline_data");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4", "-O", "inline_data"], "64M");
    assert_eq!(Ext4::mount(Box::new(image)).err(), Some(Error::Unsupported));
}

#[test]
fn mounts_as_many_as_it_can_and_gets_them_back() {
    let _serial = serial();
    let dir = workspace("mounts");
    let root = dir.join("tree");
    make_tree(&root);
    let bytes = image(&dir, &root, &["-t", "ext4"], "64M").bytes();
    let fresh = || Box::new(Image::new(bytes.clone()));
    // Again and again, so each unmount gives its slot back.
    for _ in 0..3 {
        let held: Vec<_> = (0..ext4::MOUNTS).map(|_| Ext4::mount(fresh()).unwrap()).collect();
        assert_eq!(Ext4::mount(fresh()).err(), Some(Error::TooManyMounts));
        drop(held);
    }
}

#[test]
fn reads_holes_as_zeros() {
    // mke2fs -d makes a hole of each block of zeros in what it copies: a
    // file whose blocks of data are between holes, one block or many, as
    // in Rust's .rmeta files. Read whole, and from every alignment around
    // the edges of the holes, in pieces long and short.
    let _serial = serial();
    let dir = workspace("holes");
    let root = dir.join("tree");
    fs::create_dir_all(&root).unwrap();
    let block = 4096;
    let mut sparse = Vec::new();
    for (index, data) in
        [true, false, true, true, false, false, false, true, false, true].into_iter().enumerate()
    {
        let fill = |byte: usize| if data { (index * 37 + byte % 251) as u8 | 1 } else { 0 };
        sparse.extend((0..block).map(fill));
    }
    // And a part of a block at the end, after a hole.
    sparse.extend([0; 100]);
    sparse.extend([7; 100]);
    fs::write(root.join("sparse.bin"), &sparse).unwrap();
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4", "-b", "4096"], "16M")))
        .expect("mounts");
    assert!(read_all(&fs, "sparse.bin") == sparse, "sparse.bin reads back different");
    let file = fs.open("sparse.bin").unwrap();
    for start in [0, 100, block - 1, block, block + 1, 4 * block - 5, 4 * block + 3, 9 * block] {
        for len in [1, 200, block, block + 7, 3 * block + 11, sparse.len()] {
            let len = len.min(sparse.len() - start);
            let mut read = vec![0xAA; len];
            let mut done = 0;
            while done < len {
                done += file.read_at((start + done) as u64, &mut read[done..]).unwrap();
            }
            assert!(read == sparse[start..start + len], "{len} bytes from {start} read different");
        }
    }
}
