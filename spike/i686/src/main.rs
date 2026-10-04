//! A 32-bit simplos spike for v86.
//!
//! This exists to answer three questions before committing to an i686 port of
//! the kernel. Everything else in that port is mechanical; these are the parts
//! that could actually fail:
//!
//! 1. Does v86's multiboot loader start a bare Rust ELF at all? (serial proves it)
//! 2. Can the kernel set its own video mode? v86's multiboot path loads no BIOS,
//!    and 32-bit protected mode can't call `int 0x10`, so the mode has to come
//!    from the Bochs VBE dispi registers — which v86 implements in hardware.
//! 3. Does the existing drawing code — embedded-graphics, a heap, `format!` —
//!    build and run on i686?
//!
//! Deliberately no paging: multiboot hands over protected mode with flat
//! segments and paging off, which is why `memory.rs` largely disappears in the
//! real port.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use alloc::format;
use core::arch::{asm, global_asm};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_7X13;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Alignment, Text};
use linked_list_allocator::LockedHeap;

// Multiboot hands over a stack-less machine: ESP is undefined, so the very
// first thing to do is point it somewhere real. EBX holds the multiboot info
// pointer, EAX the magic.
global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "    mov esp, offset STACK + 65536",
    "    push ebx",
    "    push eax",
    "    call kernel_main",
    "1:  hlt",
    "    jmp 1b",
);

#[unsafe(no_mangle)]
static mut STACK: [u8; 65536] = [0; 65536];

/// Multiboot v1 header. v86 only implements the memory-map, cmdline and module
/// flags — it `dbg_assert`s on anything else, including the video-mode request
/// (bit 2) — so this asks for memory info and nothing more, and the kernel sets
/// the video mode itself below.
#[repr(C, align(4))]
struct MultibootHeader {
    magic: u32,
    flags: u32,
    checksum: u32,
}

const MULTIBOOT_MAGIC: u32 = 0x1BAD_B002;
const MULTIBOOT_FLAGS: u32 = 0x2; // memory info

#[used]
#[unsafe(link_section = ".multiboot")]
static MULTIBOOT_HEADER: MultibootHeader = MultibootHeader {
    magic: MULTIBOOT_MAGIC,
    flags: MULTIBOOT_FLAGS,
    checksum: (0u32).wrapping_sub(MULTIBOOT_MAGIC.wrapping_add(MULTIBOOT_FLAGS)),
};

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// 4 MiB of heap, in .bss. With paging off there is nothing to map: the
/// multiboot memory map says low memory is usable and this lives inside it.
#[unsafe(no_mangle)]
static mut HEAP: [u8; 4 * 1024 * 1024] = [0; 4 * 1024 * 1024];

// ---------------------------------------------------------------- port I/O

unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) }
}

unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack)) }
    value
}

unsafe fn outw(port: u16, value: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack)) }
}

unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    unsafe { asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack)) }
    value
}

unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack)) }
}

unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack)) }
    value
}

// ----------------------------------------------------------------- serial

const COM1: u16 = 0x3F8;

fn serial_init() {
    unsafe {
        outb(COM1 + 1, 0x00); // interrupts off
        outb(COM1 + 3, 0x80); // DLAB
        outb(COM1, 0x03); // 38400 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7);
        outb(COM1 + 4, 0x0B);
    }
}

fn serial_write(text: &str) {
    for byte in text.bytes() {
        unsafe {
            while inb(COM1 + 5) & 0x20 == 0 {}
            outb(COM1, byte);
        }
    }
}

macro_rules! serial_println {
    ($($arg:tt)*) => {{
        serial_write(&format!($($arg)*));
        serial_write("\n");
    }};
}

// -------------------------------------------------------------------- PCI

/// BAR0 of the first display-class device, which is where the linear
/// framebuffer lives. v86 defaults it to 0xE0000000, but reading it means the
/// same binary also works under QEMU, which puts it elsewhere.
fn find_framebuffer() -> Option<u32> {
    for bus in 0u32..1 {
        for device in 0u32..32 {
            let address = 0x8000_0000 | (bus << 16) | (device << 11);
            let class = unsafe {
                outl(0xCF8, address | 0x08);
                inl(0xCFC)
            } >> 24;
            if class != 0x03 {
                continue;
            }
            let bar0 = unsafe {
                outl(0xCF8, address | 0x10);
                inl(0xCFC)
            };
            if bar0 & 1 == 0 && bar0 & !0xF != 0 {
                return Some(bar0 & !0xF);
            }
        }
    }
    None
}

// -------------------------------------------------------------- Bochs VBE

const VBE_INDEX: u16 = 0x01CE;
const VBE_DATA: u16 = 0x01CF;
const VBE_XRES: u16 = 1;
const VBE_YRES: u16 = 2;
const VBE_BPP: u16 = 3;
const VBE_ENABLE: u16 = 4;
const VBE_ENABLED: u16 = 0x01;
const VBE_LFB: u16 = 0x40;

unsafe fn vbe_write(index: u16, value: u16) {
    unsafe {
        outw(VBE_INDEX, index);
        outw(VBE_DATA, value);
    }
}

unsafe fn vbe_read(index: u16) -> u16 {
    unsafe {
        outw(VBE_INDEX, index);
        inw(VBE_DATA)
    }
}

/// Set a linear 32-bpp mode. This is the piece that replaces the whole
/// bootloader VESA dance on the 64-bit side.
fn set_video_mode(width: u16, height: u16) -> bool {
    unsafe {
        vbe_write(VBE_ENABLE, 0);
        vbe_write(VBE_XRES, width);
        vbe_write(VBE_YRES, height);
        vbe_write(VBE_BPP, 32);
        vbe_write(VBE_ENABLE, VBE_ENABLED | VBE_LFB);
        vbe_read(VBE_XRES) == width && vbe_read(VBE_YRES) == height
    }
}

// ----------------------------------------------------------------- display

/// The same shape as the 64-bit kernel's `Display`, to prove the drawing code
/// ports unchanged: a back buffer, dirty-row tracking and a partial present.
struct Display {
    framebuffer: *mut u8,
    width: u32,
    height: u32,
    back_buffer: alloc::vec::Vec<u8>,
    dirty: Option<(u32, u32)>,
}

impl Display {
    fn new(framebuffer: u32, width: u32, height: u32) -> Self {
        Self {
            framebuffer: framebuffer as *mut u8,
            width,
            height,
            back_buffer: alloc::vec![0; (width * height * 4) as usize],
            dirty: Some((0, height - 1)),
        }
    }

    fn mark_dirty(&mut self, top: u32, bottom: u32) {
        self.dirty = Some(match self.dirty {
            Some((old_top, old_bottom)) => (old_top.min(top), old_bottom.max(bottom)),
            None => (top, bottom),
        });
    }

    fn fill_rect(&mut self, area: &Rectangle, color: Rgb888) {
        let left = area.top_left.x.max(0) as u32;
        let top = area.top_left.y.max(0) as u32;
        let right = ((area.top_left.x + area.size.width as i32 - 1).max(0) as u32)
            .min(self.width.saturating_sub(1));
        let bottom = ((area.top_left.y + area.size.height as i32 - 1).max(0) as u32)
            .min(self.height.saturating_sub(1));
        if left >= self.width || top >= self.height {
            return;
        }
        for y in top..=bottom {
            for x in left..=right {
                let index = ((y * self.width + x) * 4) as usize;
                self.back_buffer[index] = color.b();
                self.back_buffer[index + 1] = color.g();
                self.back_buffer[index + 2] = color.r();
            }
        }
        self.mark_dirty(top, bottom);
    }

    fn present(&mut self) {
        let Some((top, bottom)) = self.dirty.take() else {
            return;
        };
        let pitch = (self.width * 4) as usize;
        for y in top as usize..=bottom as usize {
            let offset = y * pitch;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.back_buffer.as_ptr().add(offset),
                    self.framebuffer.add(offset),
                    pitch,
                );
            }
        }
    }
}

impl Dimensions for Display {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(Point::new(0, 0), Size::new(self.width, self.height))
    }
}

impl DrawTarget for Display {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            if coord.x < 0 || coord.y < 0 || coord.x as u32 >= self.width || coord.y as u32 >= self.height {
                continue;
            }
            let index = ((coord.y as u32 * self.width + coord.x as u32) * 4) as usize;
            self.back_buffer[index] = color.b();
            self.back_buffer[index + 1] = color.g();
            self.back_buffer[index + 2] = color.r();
            let row = coord.y as u32;
            self.mark_dirty(row, row);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        self.fill_rect(area, color);
        Ok(())
    }
}


// ------------------------------------------------------- GDT / IDT / PIC
//
// The last unproven piece of the port: 32-bit descriptor tables and hardware
// interrupts. Multiboot leaves the GDT unspecified and interrupts off, so all
// of this has to be built before a keystroke can arrive.

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GdtEntry {
    limit_low: u16,
    base_low: u16,
    base_middle: u8,
    access: u8,
    granularity: u8,
    base_high: u8,
}

impl GdtEntry {
    const fn new(access: u8, granularity: u8) -> Self {
        Self {
            limit_low: 0xFFFF,
            base_low: 0,
            base_middle: 0,
            access,
            granularity,
            base_high: 0,
        }
    }
    const NULL: Self = Self {
        limit_low: 0,
        base_low: 0,
        base_middle: 0,
        access: 0,
        granularity: 0,
        base_high: 0,
    };
}

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u32,
}

static mut GDT: [GdtEntry; 3] = [
    GdtEntry::NULL,
    GdtEntry::new(0x9A, 0xCF), // ring-0 code, 4 KiB granularity, 32-bit
    GdtEntry::new(0x92, 0xCF), // ring-0 data
];

fn init_gdt() {
    unsafe {
        let pointer = DescriptorPointer {
            limit: (core::mem::size_of::<[GdtEntry; 3]>() - 1) as u16,
            base: (&raw const GDT) as u32,
        };
        // Reloading CS needs a far transfer. `retf` with the selector and
        // return address pushed is the portable way to do it in Intel syntax.
        asm!(
            "lgdt [{ptr}]",
            "push 0x08",
            "lea {tmp}, [2f]",
            "push {tmp}",
            "retf",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            ptr = in(reg) &pointer,
            tmp = out(reg) _,
            out("eax") _,
        );
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    zero: u8,
    type_attr: u8,
    offset_high: u16,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        zero: 0,
        type_attr: 0,
        offset_high: 0,
    };

    fn set(&mut self, handler: u32) {
        self.offset_low = (handler & 0xFFFF) as u16;
        self.selector = 0x08;
        self.zero = 0;
        self.type_attr = 0x8E; // present, ring 0, 32-bit interrupt gate
        self.offset_high = (handler >> 16) as u16;
    }
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

/// What the CPU pushes for a 32-bit interrupt gate with no error code.
#[repr(C)]
struct InterruptStackFrame {
    eip: u32,
    cs: u32,
    eflags: u32,
}

static TICKS: AtomicU32 = AtomicU32::new(0);
static LAST_KEY: AtomicU8 = AtomicU8::new(0);

/// A tiny lock-free ring, the same idea as the real kernel's CONSOLE.
static mut KEY_RING: [u8; 64] = [0; 64];
static KEY_WRITE: AtomicU32 = AtomicU32::new(0);
static KEY_READ: AtomicU32 = AtomicU32::new(0);

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    unsafe { outb(0x20, 0x20) } // EOI
}

extern "x86-interrupt" fn keyboard_handler(_frame: InterruptStackFrame) {
    let scancode = unsafe { inb(0x60) };
    LAST_KEY.store(scancode, Ordering::Relaxed);
    if scancode & 0x80 == 0
        && let Some(ascii) = scancode_to_ascii(scancode)
    {
        let slot = KEY_WRITE.fetch_add(1, Ordering::Relaxed) as usize % 64;
        unsafe { (&raw mut KEY_RING).cast::<u8>().add(slot).write(ascii) };
    }
    unsafe { outb(0x20, 0x20) } // EOI
}

fn pop_key() -> Option<u8> {
    let read = KEY_READ.load(Ordering::Relaxed);
    if read == KEY_WRITE.load(Ordering::Relaxed) {
        return None;
    }
    let value = unsafe { (&raw const KEY_RING).cast::<u8>().add(read as usize % 64).read() };
    KEY_READ.store(read.wrapping_add(1), Ordering::Relaxed);
    Some(value)
}

/// Scancode set 1, make codes only — enough to type with.
fn scancode_to_ascii(scancode: u8) -> Option<u8> {
    const MAP: [u8; 58] = *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
    MAP.get(scancode as usize).copied().filter(|c| *c != 0)
}

fn init_idt() {
    unsafe {
        let idt = &raw mut IDT;
        (*idt)[0x20].set(timer_handler as u32);
        (*idt)[0x21].set(keyboard_handler as u32);
        let pointer = DescriptorPointer {
            limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: idt as u32,
        };
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack));
    }
}

/// Remap the 8259s away from the CPU exception vectors and unmask IRQ0/IRQ1.
fn init_pic() {
    unsafe {
        outb(0x20, 0x11);
        outb(0xA0, 0x11);
        outb(0x21, 0x20); // master -> vectors 0x20..
        outb(0xA1, 0x28); // slave  -> vectors 0x28..
        outb(0x21, 0x04);
        outb(0xA1, 0x02);
        outb(0x21, 0x01);
        outb(0xA1, 0x01);
        outb(0x21, 0xFC); // timer + keyboard
        outb(0xA1, 0xFF);
    }
}

/// 100 Hz, so the redraw gate has something regular to ride on.
fn init_pit() {
    const DIVISOR: u16 = 11932;
    unsafe {
        outb(0x43, 0x36);
        outb(0x40, (DIVISOR & 0xFF) as u8);
        outb(0x40, (DIVISOR >> 8) as u8);
    }
}

// -------------------------------------------------------------------- main

const WIDTH: u16 = 1024;
const HEIGHT: u16 = 768;

#[unsafe(no_mangle)]
extern "C" fn kernel_main(magic: u32, multiboot_info: u32) -> ! {
    serial_init();
    serial_write("\nsimplos-i686: alive\n");

    unsafe {
        let heap = &raw mut HEAP;
        ALLOCATOR.lock().init(heap.cast::<u8>(), (*heap).len());
    }

    serial_println!("simplos-i686: magic={magic:#x} mbi={multiboot_info:#x}");

    let framebuffer = match find_framebuffer() {
        Some(address) => {
            serial_println!("simplos-i686: framebuffer BAR0={address:#x}");
            address
        }
        None => {
            // v86's hardware default when PCI probing comes up empty.
            serial_write("simplos-i686: no display device on PCI, assuming 0xE0000000\n");
            0xE000_0000
        }
    };

    if set_video_mode(WIDTH, HEIGHT) {
        serial_println!("simplos-i686: VBE mode {WIDTH}x{HEIGHT}x32 set");
    } else {
        serial_write("simplos-i686: VBE mode set FAILED\n");
    }

    let mut display = Display::new(framebuffer, WIDTH as u32, HEIGHT as u32);
    let background = Rgb888::new(23, 114, 243);
    let style = MonoTextStyle::new(&FONT_7X13, Rgb888::new(94, 243, 23));

    let area = display.bounding_box();
    display.fill_rect(&area, background);
    display.present();
    serial_write("simplos-i686: background painted\n");

    init_gdt();
    serial_write("simplos-i686: GDT loaded\n");
    init_idt();
    init_pic();
    init_pit();
    unsafe { asm!("sti", options(nomem, nostack)) };
    serial_write("simplos-i686: interrupts enabled\n");

    let mut frame: u64 = 0;
    let mut previous = Rectangle::new(Point::zero(), Size::zero());
    let mut typed = alloc::string::String::new();
    let mut last_tick = u32::MAX;
    loop {
        // Same pacing as the real kernel: redraw on a tick, halt in between.
        // If v86 mishandled `hlt` or IRQ delivery, this would simply freeze.
        let tick = TICKS.load(Ordering::Relaxed);
        let mut got_key = false;
        while let Some(key) = pop_key() {
            if key == 8 {
                typed.pop();
            } else {
                typed.push(key as char);
            }
            if typed.len() > 40 {
                let _ = typed.remove(0);
            }
            got_key = true;
        }
        if tick == last_tick && !got_key {
            unsafe { asm!("hlt", options(nomem, nostack)) };
            continue;
        }
        last_tick = tick;

        let scancode = LAST_KEY.load(Ordering::Relaxed);
        let status = format!(
            "\
            simplos (32-bit spike)\n\
            Frame: {frame}\n\
            Ticks: {tick}\n\
            Mode: {WIDTH}x{HEIGHT}x32\n\
            Framebuffer: {framebuffer:#x}\n\
            Multiboot: {magic:#x}\n\
            Scancode: {scancode:#04x}\n\
            \n\
            Keyboard: {typed}\n\
            "
        );

        let center = display.bounding_box().center();
        let text = Text::with_alignment(status.as_str(), center, style, Alignment::Center);
        display.fill_rect(&previous, background);
        previous = text.bounding_box();
        let _ = text.draw(&mut display);
        display.present();

        if frame == 0 {
            serial_write("simplos-i686: first frame drawn\n");
        }
        if frame % 100 == 0 {
            serial_println!("simplos-i686: frame={frame}");
        }
        frame += 1;

    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_write("\n*** SPIKE PANIC ***\n");
    serial_write(&format!("{info}\n"));
    loop {
        unsafe { asm!("hlt", options(nomem, nostack)) }
    }
}
