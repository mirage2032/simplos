//! User programs: Linux x86-64 executables, run in ring 3 in an address space
//! of their own. Each runs on one thread, whose system calls `syscall.rs`
//! handles, and which a fault ends without touching anything else.
//!
//! The lower half of a program's address space:
//!
//!   0x0040_0000          the program (a PIE is loaded here), then its heap (`brk`)
//!   0x1000_0000_0000     `mmap`, growing up
//!   0x7FFF_FFFF_0000     the top of the stack, which grows down. The 64 KiB above
//!                        it are never mapped, so no `syscall` can sit at the very
//!                        end of the lower half (see `syscall.rs`).

use crate::elf::{self, Elf};
use crate::fs;
use crate::io::interrupts::gdt;
use crate::memory::AddressSpace;
use crate::thread::{self, WaitQueue};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags, Cr4, Cr4Flags};
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::page::PageRange;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame};

/// Where a position-independent program is loaded.
const PIE_BASE: u64 = 0x40_0000;
/// Nothing is mapped below this, so a null pointer and small offsets from one
/// always fault.
pub const LOWEST: u64 = 0x1_0000;
pub const MMAP_START: u64 = 0x0000_1000_0000_0000;
const STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
/// shortcut: a fixed 256 KiB, mapped up front; grow it on page faults when a
/// program needs more.
const STACK_SIZE: u64 = 256 * 1024;
/// Where `mmap` stops: a gap below the stack.
pub const MMAP_END: u64 = STACK_TOP - STACK_SIZE - 0x10_0000;

/// What `status` holds until the program ends.
const RUNNING: i32 = i32::MIN;

/// Exit statuses for programs a fault ended, as a shell reports them: 128 plus
/// the signal Linux would have sent.
pub const KILLED_SIGSEGV: i32 = 128 + 11;
pub const KILLED_SIGILL: i32 = 128 + 4;
pub const KILLED_SIGFPE: i32 = 128 + 8;
pub const KILLED_SIGTRAP: i32 = 128 + 5;
pub const KILLED_SIGBUS: i32 = 128 + 7;

pub struct Process {
    pub id: u64,
    pml4: PhysFrame,
    /// Taken only with interrupts on, by the program's own system calls.
    pub memory: Mutex<Memory>,
    status: AtomicI32,
    exited: WaitQueue,
    /// Where it writes to its standard output and error.
    pub output: Mutex<Output>,
    /// Its file descriptors, by number; `None` for one that's closed. Taken
    /// only with interrupts on, by the program's own system calls.
    pub files: Mutex<Vec<Option<Arc<fs::Handle>>>>,
    /// Where its relative paths start: a path from the root, without the
    /// leading slash, as `fs::resolve` makes them.
    pub cwd: Mutex<String>,
    /// The permission bits taken off what it creates.
    pub umask: AtomicU32,
}

/// What a program's standard output and error are written to.
pub type Output = Box<dyn FnMut(&[u8]) + Send>;

pub struct Memory {
    pub space: AddressSpace,
    /// Where the heap starts, and where it ends now.
    pub brk_start: u64,
    pub brk: u64,
    /// Where the next `mmap` without an address goes.
    pub mmap_next: u64,
}

impl Process {
    /// Block until the program ends, and return its exit status: what it
    /// passed to `exit`, or 128 plus a signal number if a fault ended it.
    pub fn wait(&self) -> i32 {
        self.exited.wait_until(|| self.status.load(Ordering::Acquire) != RUNNING);
        self.status.load(Ordering::Acquire)
    }
}

/// Load `elf` into a new address space and start it on a thread of its own,
/// with `args` as its arguments (the first being its name, by convention).
/// Whatever it writes to its standard output or error goes to `output`.
pub fn spawn(
    elf: &[u8],
    args: &[&str],
    output: impl FnMut(&[u8]) + Send + 'static,
) -> Result<Arc<Process>, &'static str> {
    const NO_MEMORY: &str = "out of memory";
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    let mut space = AddressSpace::new().ok_or(NO_MEMORY)?;
    let image = load(&mut space, elf)?;
    let rsp = build_stack(&mut space, args, &image)?;
    let process = Arc::new(Process {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        pml4: space.pml4(),
        memory: Mutex::new(Memory {
            space,
            brk_start: image.end,
            brk: image.end,
            mmap_next: MMAP_START,
        }),
        status: AtomicI32::new(RUNNING),
        exited: WaitQueue::new(),
        output: Mutex::new(Box::new(output)),
        files: Mutex::new(vec![Some(Arc::new(fs::Handle::Console)); 3]),
        cwd: Mutex::new(String::new()),
        umask: AtomicU32::new(0o022),
    });
    let user = UserState {
        process: process.clone(),
        start: (image.entry, rsp),
        kernel_stack_top: VirtAddr::zero(),
        fs_base: 0,
        fpu: FpuState::new(),
    };
    thread::spawn_user(Box::new(user)).ok_or(NO_MEMORY)?;
    Ok(process)
}

/// End the current thread's program with `status`, from one of its system
/// calls or a fault it caused. Its memory is freed once its thread is reaped
/// and whoever waits for it lets go.
pub fn exit_current(status: i32) -> ! {
    if let Some(process) = thread::current_process() {
        process.status.store(status, Ordering::Release);
        process.exited.wake_all();
    }
    thread::exit()
}

/// What the scheduler swaps in and out for a thread that runs a program: the
/// parts of the CPU's state that are the program's and that a switch would
/// otherwise leave for the next thread to see or to clobber.
pub struct UserState {
    pub process: Arc<Process>,
    /// Where the thread first enters ring 3, and its stack pointer there.
    pub start: (u64, u64),
    /// The top of the thread's kernel stack, where the CPU switches to from
    /// ring 3. Set by `thread::spawn_user`.
    pub kernel_stack_top: VirtAddr,
    /// Where `fs` points: musl keeps its thread-local storage there.
    fs_base: u64,
    fpu: FpuState,
}

impl UserState {
    /// The thread is about to run: give the CPU its page table, its `fs`, its
    /// stack for entering the kernel and its SSE registers.
    pub fn switch_in(&self) {
        if Cr3::read().0 != self.process.pml4 {
            // Safety: the program's page table, whose upper half is the kernel's.
            unsafe { Cr3::write(self.process.pml4, Cr3Flags::empty()) };
        }
        FsBase::write(VirtAddr::new(self.fs_base));
        gdt::set_kernel_stack(self.kernel_stack_top);
        crate::syscall::set_kernel_stack(self.kernel_stack_top);
        // Safety: a 512-byte, 16-byte aligned area filled by `fxsave` or `new`.
        unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) &self.fpu, options(nostack)) };
    }

    /// The thread is about to stop running: keep its SSE registers. The kernel
    /// itself never uses them, so they still hold the program's.
    pub fn switch_out(&mut self) {
        // Safety: as in `switch_in`.
        unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) &mut self.fpu, options(nostack)) };
    }

    pub fn fs_base(&self) -> u64 {
        self.fs_base
    }

    /// Set `fs`, for `arch_prctl`. Only for the running thread.
    pub fn set_fs_base(&mut self, base: u64) {
        self.fs_base = base;
        FsBase::write(VirtAddr::new(base));
    }
}

/// What `fxsave` stores: the x87 and SSE registers.
#[repr(C, align(16))]
struct FpuState([u8; 512]);

impl FpuState {
    /// As after `fninit`: x87 control word 0x37F and MXCSR 0x1F80, every
    /// exception masked, round to nearest.
    fn new() -> FpuState {
        let mut area = [0; 512];
        area[0..2].copy_from_slice(&0x037Fu16.to_le_bytes());
        area[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        FpuState(area)
    }
}

/// Make the CPU ready for user programs: SSE, which every x86-64 program
/// uses, and the `syscall` instruction. `pre_init` calls it.
pub fn init() {
    // Safety: SSE on, its exceptions reported as #XM, and x87 instructions
    // executed rather than trapped.
    unsafe {
        Cr0::update(|flags| {
            flags.remove(Cr0Flags::EMULATE_COPROCESSOR);
            flags.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|flags| flags.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE));
    }
    crate::syscall::init();
}

/// Drop to ring 3 at `rip` with stack `rsp`, never to return: the program's
/// thread comes back to the kernel only through system calls and interrupts,
/// each on a fresh stack, and leaves through `exit_current`.
pub fn enter_user(rip: u64, rsp: u64) -> ! {
    let selectors = gdt::selectors();
    // Interrupts on in ring 3, and bit 1, which is always set.
    const RFLAGS: u64 = 0x202;
    // Safety: the thread's address space is active (`switch_in`), with the
    // program loaded and `rsp` in its stack. The kernel's registers are
    // cleared so the program can't read them.
    unsafe {
        core::arch::asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "iretq",
            ss = in(reg) u64::from(selectors.user_data_selector.0),
            rsp = in(reg) rsp,
            rflags = in(reg) RFLAGS,
            cs = in(reg) u64::from(selectors.user_code_selector.0),
            rip = in(reg) rip,
            options(noreturn),
        )
    }
}

/// Where the loader put things, for the stack and the heap.
struct Image {
    entry: u64,
    /// The program headers, in the program's memory, and how many.
    phdr: u64,
    phnum: u64,
    /// The first page past the highest segment, where the heap starts.
    end: u64,
}

fn load(space: &mut AddressSpace, file: &[u8]) -> Result<Image, &'static str> {
    let elf = Elf::parse(file)?;
    let base = if elf.pie { PIE_BASE } else { 0 };
    let outside = "a segment outside the program's area";
    let mut end = 0;
    let mut phdr = None;
    for segment in elf.segments() {
        match segment.kind {
            elf::PT_LOAD => {
                let start = base.checked_add(segment.vaddr).ok_or(outside)?;
                let stop = start.checked_add(segment.memsz).ok_or(outside)?;
                if start < LOWEST || stop > MMAP_START {
                    return Err(outside);
                }
                let mut flags = PageTableFlags::empty();
                if segment.flags & elf::PF_W != 0 {
                    flags |= PageTableFlags::WRITABLE;
                }
                if segment.flags & elf::PF_X == 0 {
                    flags |= PageTableFlags::NO_EXECUTE;
                }
                space.map(pages(start, stop), flags).ok_or("out of memory")?;
                space.load(start, elf.contents(&segment)).ok_or(outside)?;
                end = end.max(stop);
                // Without a PT_PHDR, the headers are in the segment that
                // starts at the beginning of the file, if any is.
                let headers = elf.phoff + u64::from(elf.phnum) * elf::PHENT_SIZE;
                if segment.offset == 0 && headers <= segment.filesz {
                    phdr = phdr.or(Some(start + elf.phoff));
                }
            }
            elf::PT_PHDR => phdr = Some(base.checked_add(segment.vaddr).ok_or(outside)?),
            _ => {}
        }
    }
    let entry = base.checked_add(elf.entry).filter(|&entry| entry >= LOWEST && entry < end);
    Ok(Image {
        entry: entry.ok_or("an entry point outside the program")?,
        phdr: phdr.ok_or("no program headers in the program's memory")?,
        phnum: u64::from(elf.phnum),
        end: end.next_multiple_of(4096),
    })
}

/// Map the stack and lay it out as Linux does for a new program: from the
/// stack pointer up, `argc`, the `argv` pointers, an empty environment, and
/// the auxiliary vector, which tells musl where its program headers (and so
/// its thread-local storage) are; the strings above them.
fn build_stack(
    space: &mut AddressSpace,
    args: &[&str],
    image: &Image,
) -> Result<u64, &'static str> {
    const AT_NULL: u64 = 0;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;
    let too_long = "arguments too long for the stack";

    let bottom = STACK_TOP - STACK_SIZE;
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    space.map(pages(bottom, STACK_TOP), flags).ok_or("out of memory")?;

    let mut top = STACK_TOP;
    let mut push = |space: &mut AddressSpace, bytes: &[u8]| {
        top = top.checked_sub(bytes.len() as u64).filter(|&top| top > bottom + 4096)?;
        space.load(top, bytes).map(|()| top)
    };
    let random = push(space, &random_bytes()).ok_or(too_long)?;
    let mut argv = Vec::with_capacity(args.len());
    for arg in args {
        let mut string = vec![0; arg.len() + 1];
        string[..arg.len()].copy_from_slice(arg.as_bytes());
        argv.push(push(space, &string).ok_or(too_long)?);
    }

    let mut words = vec![args.len() as u64];
    words.extend(argv);
    words.extend([0, 0]); // the end of `argv`, and an empty environment
    words.extend([
        AT_PHDR,
        image.phdr,
        AT_PHENT,
        elf::PHENT_SIZE,
        AT_PHNUM,
        image.phnum,
        AT_PAGESZ,
        4096,
        AT_ENTRY,
        image.entry,
        AT_RANDOM,
        random,
        AT_NULL,
        0,
    ]);
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    // `argc` on a 16-byte boundary, as the ABI requires at entry.
    let rsp = (top - bytes.len() as u64) & !0xF;
    if rsp <= bottom + 4096 {
        return Err(too_long);
    }
    space.load(rsp, &bytes).ok_or(too_long)?;
    Ok(rsp)
}

/// The 16 bytes behind `AT_RANDOM`, which seed the program's stack canaries
/// and hash maps.
///
/// shortcut: RDRAND when the CPU has it, else the timestamp counter, which is
/// guessable; take entropy from somewhere better once there is somewhere.
fn random_bytes() -> [u8; 16] {
    let rdrand = x86_64::instructions::random::RdRand::new();
    let word = || {
        rdrand
            .and_then(|r| r.get_u64())
            // Safety: rdtsc reads a counter, nothing more.
            .unwrap_or_else(|| unsafe { core::arch::x86_64::_rdtsc() }.rotate_left(17))
    };
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&word().to_le_bytes());
    bytes[8..].copy_from_slice(&word().to_le_bytes());
    bytes
}

/// The pages covering `start..end`.
pub fn pages(start: u64, end: u64) -> PageRange {
    let first = Page::containing_address(VirtAddr::new(start));
    let past = Page::containing_address(VirtAddr::new(end.next_multiple_of(4096)));
    Page::range(first, past)
}
