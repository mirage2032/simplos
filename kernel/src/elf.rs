//! The parts of an ELF64 executable a loader needs: its header and program
//! headers. Every offset and size is checked before it is used: the file
//! comes from outside the kernel.

pub const PT_LOAD: u32 = 1;
pub const PT_PHDR: u32 = 6;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

/// Size of one program header in an ELF64 file.
pub const PHENT_SIZE: u64 = 56;

pub struct Elf<'a> {
    file: &'a [u8],
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

impl<'a> Elf<'a> {
    pub fn parse(file: &'a [u8]) -> Result<Elf<'a>, &'static str> {
        if file.get(..4) != Some(b"\x7fELF") {
            return Err("not an ELF file");
        }
        // Class 64-bit, little-endian, ELF version 1.
        if file.get(4..7) != Some(&[2, 1, 1]) {
            return Err("not a 64-bit little-endian ELF file");
        }
        let pie = match u16_at(file, 16)? {
            2 => false,
            3 => true,
            _ => return Err("not an executable"),
        };
        if u16_at(file, 18)? != 62 {
            return Err("not an x86-64 program");
        }
        if u64::from(u16_at(file, 54)?) != PHENT_SIZE {
            return Err("unexpected program header size");
        }
        let elf = Elf {
            file,
            pie,
            entry: u64_at(file, 24)?,
            phoff: u64_at(file, 32)?,
            phnum: u16_at(file, 56)?,
        };
        let table_end = elf.phoff.checked_add(u64::from(elf.phnum) * PHENT_SIZE);
        if table_end.is_none_or(|end| end > file.len() as u64) {
            return Err("program headers past the end of the file");
        }
        for segment in elf.segments() {
            let in_file = segment.offset.checked_add(segment.filesz);
            if segment.kind == PT_LOAD
                && (segment.filesz > segment.memsz
                    || in_file.is_none_or(|end| end > file.len() as u64))
            {
                return Err("a segment past the end of the file");
            }
        }
        Ok(elf)
    }

    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        (0..u64::from(self.phnum)).map(|i| {
            // In bounds: `parse` checked the whole table is in the file.
            let at = (self.phoff + i * PHENT_SIZE) as usize;
            let header = &self.file[at..at + PHENT_SIZE as usize];
            Segment {
                kind: u32_at(header, 0).unwrap(),
                flags: u32_at(header, 4).unwrap(),
                offset: u64_at(header, 8).unwrap(),
                vaddr: u64_at(header, 16).unwrap(),
                filesz: u64_at(header, 32).unwrap(),
                memsz: u64_at(header, 40).unwrap(),
            }
        })
    }

    /// What a loadable segment copies from the file.
    pub fn contents(&self, segment: &Segment) -> &'a [u8] {
        // In bounds for every PT_LOAD: `parse` checked.
        &self.file[segment.offset as usize..(segment.offset + segment.filesz) as usize]
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
    let elf = Elf::parse(&file).expect("probe parses");
    assert!(elf.pie, "probe is built as a static PIE");
    let loads = elf.segments().filter(|s| s.kind == PT_LOAD).count();
    assert!(loads >= 2, "{loads} loadable segments");
}

#[test_case]
fn refuses_broken_files() {
    let file = probe();
    assert_eq!(Elf::parse(&file[..40]).err(), Some("truncated ELF file"));

    let mut bad = file.clone();
    bad[0] = 0;
    assert_eq!(Elf::parse(&bad).err(), Some("not an ELF file"));

    // Program headers said to start near the end of the file.
    let mut bad = file.clone();
    bad[32..40].copy_from_slice(&(file.len() as u64 - 8).to_le_bytes());
    assert_eq!(Elf::parse(&bad).err(), Some("program headers past the end of the file"));

    // The first loadable segment claims more of the file than there is.
    let mut bad = file.clone();
    let elf = Elf::parse(&file).unwrap();
    let index = elf.segments().position(|s| s.kind == PT_LOAD).unwrap() as u64;
    let filesz_at = (elf.phoff + index * PHENT_SIZE + 32) as usize;
    bad[filesz_at..filesz_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(Elf::parse(&bad).err(), Some("a segment past the end of the file"));
}
