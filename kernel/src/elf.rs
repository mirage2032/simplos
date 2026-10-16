//! The parts of an ELF64 executable a loader needs: its header and program
//! headers, read from wherever the program is (`Source`). Every offset and
//! size is checked before it is used: the file comes from outside the kernel.

use alloc::vec;
use alloc::vec::Vec;
use x86_64::structures::paging::PhysFrame;

pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

/// Size of one program header in an ELF64 file.
pub const PHENT_SIZE: u64 = 56;

/// The most program headers take, as Linux takes them.
const MOST_HEADERS: u64 = 64 << 10;

/// Why a program couldn't be loaded, when it was its file.
pub const UNREADABLE: &str = "the program couldn't be read";

/// The longest path of an interpreter, its NUL and all, as Linux has it.
const PATH_MAX: u64 = 4096;

/// Where a program is read from: its file on the disk, or memory, as the
/// ramdisk's is. A piece at a time, as a program can be bigger than the
/// kernel's heap.
pub trait Source {
    /// How long it is, in bytes.
    fn size(&self) -> u64;
    /// Fill `buf` from `offset` on: `None` if that's past the end, or it
    /// can't be read.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Option<()>;

    /// Page `index` of it, from `index * 4096` on, in a frame to map as it
    /// is (`AddressSpace::map_shared`), held for the caller: the page
    /// cache's, for a file on the disk. `None` for what isn't in it, or
    /// past the end.
    fn page(&self, _index: u64) -> Option<PhysFrame> {
        None
    }
}

impl Source for [u8] {
    fn size(&self) -> u64 {
        self.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Option<()> {
        let start = usize::try_from(offset).ok()?;
        buf.copy_from_slice(self.get(start..start.checked_add(buf.len())?)?);
        Some(())
    }
}

pub struct Elf {
    /// The program headers, as the file has them.
    headers: Vec<u8>,
    /// Position-independent (`ET_DYN`), to be loaded at a base of the
    /// loader's choosing, rather than at the addresses in its headers.
    pub pie: bool,
    pub entry: u64,
    /// Where the program headers are in the file, and how many there are.
    pub phoff: u64,
    pub phnum: u16,
}

pub struct Segment {
    pub kind: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

impl Elf {
    pub fn parse(file: &(impl Source + ?Sized)) -> Result<Elf, &'static str> {
        // The ELF header, or as much of it as there is.
        let mut header = [0u8; 64];
        let header = &mut header[..file.size().min(64) as usize];
        file.read_at(0, header).ok_or("not an ELF file")?;
        let header = &*header;
        if header.get(..4) != Some(b"\x7fELF") {
            return Err("not an ELF file");
        }
        // Class 64-bit, little-endian, ELF version 1.
        if header.get(4..7) != Some(&[2, 1, 1]) {
            return Err("not a 64-bit little-endian ELF file");
        }
        let pie = match u16_at(header, 16)? {
            2 => false,
            3 => true,
            _ => return Err("not an executable"),
        };
        if u16_at(header, 18)? != 62 {
            return Err("not an x86-64 program");
        }
        if u64::from(u16_at(header, 54)?) != PHENT_SIZE {
            return Err("unexpected program header size");
        }
        let (phoff, phnum) = (u64_at(header, 32)?, u16_at(header, 56)?);
        let size = u64::from(phnum) * PHENT_SIZE;
        if size > MOST_HEADERS {
            return Err("too many program headers");
        }
        if phoff.checked_add(size).is_none_or(|end| end > file.size()) {
            return Err("program headers past the end of the file");
        }
        let mut headers = vec![0; size as usize];
        file.read_at(phoff, &mut headers).ok_or(UNREADABLE)?;
        let elf = Elf { headers, pie, entry: u64_at(header, 24)?, phoff, phnum };
        for segment in elf.segments() {
            let in_file = segment.offset.checked_add(segment.filesz);
            if segment.kind == PT_LOAD
                && (segment.filesz > segment.memsz || in_file.is_none_or(|end| end > file.size()))
            {
                return Err("a segment past the end of the file");
            }
        }
        Ok(elf)
    }

    /// The path of the program that loads this one, its interpreter, if
    /// its first PT_INTERP names one: as Linux takes it, in the file, two
    /// bytes to PATH_MAX, ending in a NUL; up to its first NUL.
    pub fn interpreter(
        &self,
        file: &(impl Source + ?Sized),
    ) -> Result<Option<Vec<u8>>, &'static str> {
        let Some(segment) = self.segments().find(|segment| segment.kind == PT_INTERP) else {
            return Ok(None);
        };
        if !(2..=PATH_MAX).contains(&segment.filesz) {
            return Err("an interpreter path of the wrong length");
        }
        let mut path = vec![0; segment.filesz as usize];
        file.read_at(segment.offset, &mut path).ok_or(UNREADABLE)?;
        if path.last() != Some(&0) {
            return Err("an interpreter path without its NUL");
        }
        path.truncate(path.iter().position(|&byte| byte == 0).unwrap_or(path.len()));
        Ok(Some(path))
    }

    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.headers.as_chunks::<{ PHENT_SIZE as usize }>().0.iter().map(|header| Segment {
            // In bounds: each is a whole program header.
            kind: u32_at(header, 0).unwrap(),
            flags: u32_at(header, 4).unwrap(),
            offset: u64_at(header, 8).unwrap(),
            vaddr: u64_at(header, 16).unwrap(),
            filesz: u64_at(header, 32).unwrap(),
            memsz: u64_at(header, 40).unwrap(),
        })
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, &'static str> {
    let field = bytes.get(at..at + 2).ok_or("truncated ELF file")?;
    Ok(u16::from_le_bytes(field.try_into().unwrap()))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, &'static str> {
    let field = bytes.get(at..at + 4).ok_or("truncated ELF file")?;
    Ok(u32::from_le_bytes(field.try_into().unwrap()))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, &'static str> {
    let field = bytes.get(at..at + 8).ok_or("truncated ELF file")?;
    Ok(u64::from_le_bytes(field.try_into().unwrap()))
}

#[cfg(test)]
fn probe() -> alloc::vec::Vec<u8> {
    crate::ramdisk().expect("tools/boot-test attaches user/probe").to_vec()
}

#[test_case]
fn reads_the_probe_programs_headers() {
    let file = probe();
    let elf = Elf::parse(&file[..]).expect("probe parses");
    assert!(elf.pie, "probe is built as a static PIE");
    let loads = elf.segments().filter(|s| s.kind == PT_LOAD).count();
    assert!(loads >= 2, "{loads} loadable segments");
}

#[test_case]
fn refuses_broken_files() {
    let file = probe();
    assert_eq!(Elf::parse(&file[..40]).err(), Some("truncated ELF file"));
    assert_eq!(Elf::parse(&file[..2]).err(), Some("not an ELF file"));

    let mut bad = file.clone();
    bad[0] = 0;
    assert_eq!(Elf::parse(&bad[..]).err(), Some("not an ELF file"));

    // Program headers said to start near the end of the file.
    let mut bad = file.clone();
    bad[32..40].copy_from_slice(&(file.len() as u64 - 8).to_le_bytes());
    assert_eq!(Elf::parse(&bad[..]).err(), Some("program headers past the end of the file"));

    // The first loadable segment claims more of the file than there is.
    let mut bad = file.clone();
    let elf = Elf::parse(&file[..]).unwrap();
    let index = elf.segments().position(|s| s.kind == PT_LOAD).unwrap() as u64;
    let filesz_at = (elf.phoff + index * PHENT_SIZE + 32) as usize;
    bad[filesz_at..filesz_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(Elf::parse(&bad[..]).err(), Some("a segment past the end of the file"));
}

#[test_case]
fn reads_an_interpreter_path_as_linux_does() {
    let file = probe();
    let elf = Elf::parse(&file[..]).unwrap();
    assert_eq!(elf.interpreter(&file[..]), Ok(None), "probe is static");
    // probe, its PT_GNU_STACK made a PT_INTERP for `path`, past its end:
    // what interpreter it then names.
    let stack = elf.segments().position(|s| s.kind == 0x6474_e551).unwrap() as u64;
    let header = (elf.phoff + stack * PHENT_SIZE) as usize;
    let named = |path: &[u8], offset: u64| {
        let mut file = file.clone();
        file[header..header + 4].copy_from_slice(&PT_INTERP.to_le_bytes());
        file[header + 8..header + 16].copy_from_slice(&offset.to_le_bytes());
        file[header + 32..header + 40].copy_from_slice(&(path.len() as u64).to_le_bytes());
        file.extend_from_slice(path);
        let elf = Elf::parse(&file[..]).unwrap();
        elf.interpreter(&file[..])
            .map(|path| path.map(|path| alloc::string::String::from_utf8(path).unwrap()))
    };
    let end = file.len() as u64;
    assert_eq!(named(b"/lib/ld.so\0", end), Ok(Some("/lib/ld.so".into())));
    assert_eq!(named(b"/lib/ld.so\0and more\0", end), Ok(Some("/lib/ld.so".into())));
    assert_eq!(named(b"/lib/ld.so", end), Err("an interpreter path without its NUL"));
    assert_eq!(named(b"\0", end), Err("an interpreter path of the wrong length"));
    assert_eq!(named(b"/lib/ld.so\0", end + 1), Err(UNREADABLE));
    let long = [b"/".repeat(4096).as_slice(), b"\0"].concat();
    assert_eq!(named(&long, end), Err("an interpreter path of the wrong length"));
}
