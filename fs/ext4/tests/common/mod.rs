//! What the tests share. e2fsprogs must be installed: it makes the images
//! and is the reference they're checked against.

// Each test file uses its own share of these.
#![allow(dead_code)]

use ext4::{BlockDevice, DirEntry, Error, Ext4, Kind};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

/// Held by every test: they share lwext4's few mount slots, and a test
/// shouldn't fail for finding them taken by another.
pub fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A file system image held in memory. Clones share it, so a test can look
/// at it after the file system it was mounted as lets go; every write to it
/// is logged too, in order.
#[derive(Clone)]
pub struct Image {
    pub bytes: Arc<Mutex<Vec<u8>>>,
    pub writes: Arc<Mutex<Vec<Write>>>,
}

/// The sector a write started at, and what it wrote.
pub type Write = (u64, Vec<u8>);

impl Image {
    pub fn new(bytes: Vec<u8>) -> Image {
        Image { bytes: Arc::new(Mutex::new(bytes)), writes: Arc::default() }
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// Save it to `path`, for e2fsprogs to look at.
    pub fn save(&self, path: &Path) {
        fs::write(path, &*self.bytes.lock().unwrap()).unwrap();
    }
}

impl BlockDevice for Image {
    fn sectors(&self) -> u64 {
        (self.bytes.lock().unwrap().len() / ext4::SECTOR) as u64
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), Error> {
        let start = sector as usize * ext4::SECTOR;
        let bytes = self.bytes.lock().unwrap();
        buf.copy_from_slice(bytes.get(start..start + buf.len()).ok_or(Error::Io)?);
        Ok(())
    }

    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), Error> {
        let start = sector as usize * ext4::SECTOR;
        let mut bytes = self.bytes.lock().unwrap();
        bytes.get_mut(start..start + buf.len()).ok_or(Error::Io)?.copy_from_slice(buf);
        self.writes.lock().unwrap().push((sector, buf.to_vec()));
        Ok(())
    }
}

/// Fixed, so the images are the same on every run.
const UUID: &str = "5ab1e0e5-0000-4000-8000-00000000e4e4";

/// e2fsprogs lives in /sbin, which isn't on every user's PATH.
pub fn e2fsprogs(tool: &str) -> Command {
    let sbin = Path::new("/usr/sbin").join(tool);
    Command::new(if sbin.exists() { sbin } else { PathBuf::from(tool) })
}

/// A directory of its own for `test`, under cargo's scratch space.
pub fn workspace(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 3 MiB, enough for several extents, and no two neighbouring bytes alike.
pub fn big() -> Vec<u8> {
    (0..3 << 20).map(|i: u32| (i % 251) as u8).collect()
}

/// The tree every image holds.
pub fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("dir/nested")).unwrap();
    fs::write(root.join("hello.txt"), "hello from ext4\n").unwrap();
    fs::write(root.join("dir/nested/deep.txt"), "deep\n").unwrap();
    fs::write(root.join("empty"), "").unwrap();
    fs::write(root.join("big.bin"), big()).unwrap();
    std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
}

/// `mke2fs` `options` over the tree at `root`, `size` big, as an image in
/// memory.
pub fn image(dir: &Path, root: &Path, options: &[&str], size: &str) -> Image {
    let image = dir.join("fs.img");
    let status = e2fsprogs("mke2fs")
        .args(["-q", "-F", "-U", UUID, "-E", &format!("hash_seed={UUID}"), "-d"])
        .arg(root)
        .args(options)
        .arg(&image)
        .arg(size)
        .status()
        .expect("mke2fs, from e2fsprogs, makes the test images");
    assert!(status.success(), "mke2fs {options:?} failed");
    Image::new(fs::read(image).unwrap())
}

/// What e2fsck finds wrong with the file system in the file `image`, if
/// anything. It changes nothing (`-n`) and looks at everything (`-f`).
pub fn fsck(image: &Path) -> Result<(), String> {
    let output =
        e2fsprogs("e2fsck").arg("-fn").arg(image).output().expect("e2fsck, from e2fsprogs");
    match output.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&output.stdout).into_owned()),
    }
}

/// What e2fsprogs' debugfs prints for `request` on the file `image`.
pub fn debugfs(image: &Path, request: &str) -> Vec<u8> {
    let output = e2fsprogs("debugfs")
        .args(["-R", request])
        .arg(image)
        .output()
        .expect("debugfs, from e2fsprogs");
    assert!(output.status.success(), "debugfs -R {request:?} failed");
    output.stdout
}

/// The names in the directory `path`, as debugfs reads them, sorted and
/// without `.` and `..`.
pub fn debugfs_ls(image: &Path, path: &str) -> Vec<String> {
    let listing = String::from_utf8(debugfs(image, &format!("ls -p {path}"))).unwrap();
    // Lines like `/12/100644/0/0/hello.txt/16/`.
    let mut names: Vec<_> = listing
        .lines()
        .filter_map(|line| line.split('/').nth(5))
        .filter(|name| !matches!(*name, "" | "." | ".."))
        .map(String::from)
        .collect();
    names.sort();
    names
}

pub fn read_all(fs: &Ext4, path: &str) -> Vec<u8> {
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

pub fn names(mut entries: Vec<DirEntry>) -> Vec<(String, Kind)> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.into_iter().map(|entry| (entry.name, entry.kind)).collect()
}
