//! Reading file systems made by e2fsprogs' `mke2fs`, the tool Linux makes
//! them with, across the features it can turn on and off. e2fsprogs must be
//! installed: it's the reference these tests compare against.

use ext4::{BlockDevice, DirEntry, Error, Ext4, Kind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

/// Held by every test: they share lwext4's few mount slots, and a test
/// shouldn't fail for finding them taken by another.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A file system image held in memory.
struct Image(Vec<u8>);

impl BlockDevice for Image {
    fn sectors(&self) -> u64 {
        (self.0.len() / ext4::SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), Error> {
        let start = sector as usize * ext4::SECTOR;
        buf.copy_from_slice(self.0.get(start..start + buf.len()).ok_or(Error::Io)?);
        Ok(())
    }

    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), Error> {
        let start = sector as usize * ext4::SECTOR;
        self.0.get_mut(start..start + buf.len()).ok_or(Error::Io)?.copy_from_slice(buf);
        Ok(())
    }
}

/// Fixed, so the images are the same on every run.
const UUID: &str = "5ab1e0e5-0000-4000-8000-00000000e4e4";

/// e2fsprogs lives in /sbin, which isn't on every user's PATH.
fn e2fsprogs(tool: &str) -> Command {
    let sbin = Path::new("/usr/sbin").join(tool);
    Command::new(if sbin.exists() { sbin } else { PathBuf::from(tool) })
}

/// A directory of its own for `test`, under cargo's scratch space.
fn workspace(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 3 MiB, enough for several extents, and no two neighbouring bytes alike.
fn big() -> Vec<u8> {
    (0..3 << 20).map(|i: u32| (i % 251) as u8).collect()
}

/// The tree every image holds.
fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("dir/nested")).unwrap();
    fs::write(root.join("hello.txt"), "hello from ext4\n").unwrap();
    fs::write(root.join("dir/nested/deep.txt"), "deep\n").unwrap();
    fs::write(root.join("empty"), "").unwrap();
    fs::write(root.join("big.bin"), big()).unwrap();
    std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
}

/// `mke2fs` `options` over the tree at `root`, as an image in memory.
fn image(dir: &Path, root: &Path, options: &[&str]) -> Image {
    let image = dir.join("fs.img");
    let status = e2fsprogs("mke2fs")
        .args(["-q", "-F", "-U", UUID, "-E", &format!("hash_seed={UUID}"), "-d"])
        .arg(root)
        .args(options)
        .arg(&image)
        .arg("64M")
        .status()
        .expect("mke2fs, from e2fsprogs, makes the test images");
    assert!(status.success(), "mke2fs {options:?} failed");
    Image(fs::read(image).unwrap())
}

fn read_all(fs: &Ext4, path: &str) -> Vec<u8> {
    let mut file = fs.open(path).unwrap();
    let mut contents = vec![0; file.size() as usize];
    let mut done = 0;
    while done < contents.len() {
        let read = file.read(&mut contents[done..]).unwrap();
        assert_ne!(read, 0, "{path} ended early, at {done}");
        done += read;
    }
    assert_eq!(file.read(&mut [0; 16]).unwrap(), 0, "{path} goes on past its size");
    contents
}

fn names(mut entries: Vec<DirEntry>) -> Vec<(String, Kind)> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.into_iter().map(|entry| (entry.name, entry.kind)).collect()
}

/// Make an image with `options` and read the whole tree back.
fn reads_the_tree(test: &str, options: &[&str]) {
    let _serial = serial();
    let dir = workspace(test);
    let root = dir.join("tree");
    make_tree(&root);
    let fs = Ext4::mount(Box::new(image(&dir, &root, options))).expect("mounts");

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
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"]))).unwrap();
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
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"]))).unwrap();
    let mut file = fs.open("big.bin").unwrap();
    // Across the end of one 4 KiB block into the next.
    let mut buf = [0; 100];
    file.seek(8190).unwrap();
    assert_eq!(file.read(&mut buf).unwrap(), 100);
    assert_eq!(buf[..], big()[8190..8290]);
}

#[test]
fn says_what_went_wrong() {
    let _serial = serial();
    let dir = workspace("errors");
    let root = dir.join("tree");
    make_tree(&root);
    let fs = Ext4::mount(Box::new(image(&dir, &root, &["-t", "ext4"]))).unwrap();
    assert_eq!(fs.open("missing").err(), Some(Error::NotFound));
    assert_eq!(fs.open("dir").err(), Some(Error::IsADirectory));
    assert_eq!(fs.read_dir("hello.txt").err(), Some(Error::NotADirectory));
    assert_eq!(fs.metadata("dir/missing").err(), Some(Error::NotFound));
}

#[test]
fn refuses_a_journal_that_needs_recovery() {
    let _serial = serial();
    // As if Linux had crashed mid-write: lwext4 would mount it as clean.
    let dir = workspace("needs_recovery");
    let root = dir.join("tree");
    make_tree(&root);
    let Image(_) = image(&dir, &root, &["-t", "ext4"]);
    let status = e2fsprogs("debugfs")
        .args(["-w", "-R", "feature needs_recovery"])
        .arg(dir.join("fs.img"))
        .output()
        .expect("debugfs, from e2fsprogs");
    assert!(status.status.success());
    let dirty = Image(fs::read(dir.join("fs.img")).unwrap());
    assert_eq!(Ext4::mount(Box::new(dirty)).err(), Some(Error::NeedsRecovery));
}

#[test]
fn refuses_features_it_cant_handle() {
    let _serial = serial();
    // Inline data puts small files inside their inodes, which lwext4 can't
    // read: refused, not misread.
    let dir = workspace("inline_data");
    let root = dir.join("tree");
    make_tree(&root);
    let image = image(&dir, &root, &["-t", "ext4", "-O", "inline_data"]);
    assert_eq!(Ext4::mount(Box::new(image)).err(), Some(Error::Unsupported));
}

#[test]
fn mounts_as_many_as_it_can_and_gets_them_back() {
    let _serial = serial();
    let dir = workspace("mounts");
    let root = dir.join("tree");
    make_tree(&root);
    let Image(bytes) = image(&dir, &root, &["-t", "ext4"]);
    let fresh = || Box::new(Image(bytes.clone()));
    // Again and again, so each unmount gives its slot back.
    for _ in 0..3 {
        let held: Vec<_> = (0..ext4::MOUNTS).map(|_| Ext4::mount(fresh()).unwrap()).collect();
        assert_eq!(Ext4::mount(fresh()).err(), Some(Error::TooManyMounts));
        drop(held);
    }
}
