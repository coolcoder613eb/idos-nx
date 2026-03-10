//! In order for DOS programs to run, IDOS needs to put a DOS compatibility
//! layer in higher userspace memory. This 32-bit code runs in a loop, entering
//! a 8086 VM before returning on an interrupt or GPF fault.
//!
//! DOSLAYER is loaded by the kernel's exec_program as a userspace loader (like
//! ELFLOAD for ELF binaries). It receives a load info page via EBX containing
//! the path to the DOS executable, then maps and loads the program itself.
//! Supports both .COM files and MZ .EXE files (with relocations).

#![no_std]
#![no_main]
#![feature(lang_items)]

extern crate idos_api;

pub mod api;
pub mod panic;
use core::arch::global_asm;

use idos_api::{
    compat::{LdtDescriptorParams, VMRegisters},
    io::{
        file::FileStatus,
        sync::{close_sync, io_sync, ioctl_sync, open_sync, read_sync, write_sync},
        termios::Termios,
        Handle, FILE_OP_RENAME, FILE_OP_STAT, FILE_OP_UNLINK, OPEN_FLAG_CREATE,
    },
    syscall::{io::create_file_handle, memory::map_memory},
};

const LOAD_INFO_MAGIC: u32 = 0x4C4F4144;

/// Layout of the load info page header, matching kernel/src/exec.rs
#[repr(C)]
struct LoadInfoHeader {
    magic: u32,
    exec_path_offset: u32,
    exec_path_len: u32,
    argc: u32,
    argv_offset: u32,
    argv_total_len: u32,
}

/// MZ executable header (28 bytes at the start of a DOS .EXE file)
#[repr(C, packed)]
#[derive(Default)]
struct MzHeader {
    magic: [u8; 2],         // 'M','Z' or 'Z','M'
    last_page_bytes: u16,   // bytes used in last 512-byte page
    total_pages: u16,       // total number of 512-byte pages
    relocation_count: u16,  // number of relocation entries
    header_paragraphs: u16, // header size in 16-byte paragraphs
    min_extra_paragraphs: u16,
    max_extra_paragraphs: u16,
    initial_ss: u16, // initial SS relative to load segment
    initial_sp: u16, // initial SP
    checksum: u16,
    initial_ip: u16,        // initial IP
    initial_cs: u16,        // initial CS relative to load segment
    relocation_offset: u16, // offset of relocation table in file
    overlay_number: u16,
}

/// A single MZ relocation entry: segment:offset pair
#[repr(C, packed)]
#[derive(Default)]
struct MzRelocation {
    offset: u16,
    segment: u16,
}

/// DOS segment base address (flat). Segment number is PSP_BASE / 16.
const PSP_BASE: u32 = 0x8000;
/// PSP segment number for 8086 VM registers
const PSP_SEGMENT: u32 = PSP_BASE / 16; // 0x800
/// The program image loads 0x10 paragraphs (256 bytes) past the PSP segment
const PROGRAM_SEGMENT: u32 = PSP_SEGMENT + 0x10;
/// Top of conventional memory available to DOS programs (640KB boundary)
const DOS_MEM_TOP: u32 = 0xA000_0;
/// Top of memory as a segment
const DOS_MEM_TOP_SEGMENT: u16 = (DOS_MEM_TOP / 16) as u16;

/// Program Segment Prefix — the 256-byte header DOS places before every program.
#[repr(C, packed)]
struct Psp {
    /// 0x00: INT 20h instruction (CD 20)
    int20: [u8; 2],
    /// 0x02: Top of memory segment
    mem_top_segment: u16,
    /// 0x04: Reserved
    _reserved1: u8,
    /// 0x05: Far call to DOS dispatcher (5 bytes)
    dos_far_call: [u8; 5],
    /// 0x0A: Terminate address (IP:CS)
    terminate_vector: u32,
    /// 0x0E: Ctrl-Break handler (IP:CS)
    break_vector: u32,
    /// 0x12: Critical error handler (IP:CS)
    error_vector: u32,
    /// 0x16: Parent PSP segment
    parent_psp: u16,
    /// 0x18: Job File Table (20 entries)
    jft: [u8; 20],
    /// 0x2C: Environment segment
    env_segment: u16,
    /// 0x2E: SS:SP on last INT 21h
    last_stack: u32,
    /// 0x32: JFT size
    jft_size: u16,
    /// 0x34: JFT far pointer
    jft_pointer: u32,
    /// 0x38: Previous PSP far pointer
    prev_psp: u32,
    /// 0x3C: Reserved
    _reserved2: [u8; 20],
    /// 0x50: INT 21h / RETF trampoline
    int21_retf: [u8; 3],
    /// 0x53: Reserved
    _reserved3: [u8; 45],
    /// 0x80: Command tail length
    cmdtail_len: u8,
    /// 0x81: Command tail (127 bytes, CR-terminated)
    cmdtail: [u8; 127],
}

// --- DOS File Descriptor Table ---

const MAX_DOS_FILES: usize = 20;
/// Flag: this descriptor refers to a character device (stdin/stdout/etc.)
const FD_DEVICE: u8 = 0x80;
/// Flag: this descriptor is currently open
const FD_OPEN: u8 = 0x01;

#[derive(Copy, Clone)]
struct DosFileDescriptor {
    handle: Handle,
    cursor: u32,
    flags: u8,
}

impl DosFileDescriptor {
    const fn empty() -> Self {
        DosFileDescriptor {
            handle: Handle::new(0),
            cursor: 0,
            flags: 0,
        }
    }

    fn is_open(&self) -> bool {
        self.flags & FD_OPEN != 0
    }

    fn is_device(&self) -> bool {
        self.flags & FD_DEVICE != 0
    }
}

static mut DOS_FDS: [DosFileDescriptor; MAX_DOS_FILES] =
    [DosFileDescriptor::empty(); MAX_DOS_FILES];

/// Initialize the DOS file descriptor table with the standard handles.
/// Called from compat_start after IDOS handles are opened.
fn init_file_table(stdaux_handle: Handle) {
    unsafe {
        // 0 = stdin
        DOS_FDS[0] = DosFileDescriptor {
            handle: Handle::new(0),
            cursor: 0,
            flags: FD_OPEN | FD_DEVICE,
        };
        // 1 = stdout
        DOS_FDS[1] = DosFileDescriptor {
            handle: Handle::new(1),
            cursor: 0,
            flags: FD_OPEN | FD_DEVICE,
        };
        // 2 = stderr (same IDOS handle as stdout)
        DOS_FDS[2] = DosFileDescriptor {
            handle: Handle::new(1),
            cursor: 0,
            flags: FD_OPEN | FD_DEVICE,
        };
        // 3 = stdaux (COM1)
        DOS_FDS[3] = DosFileDescriptor {
            handle: stdaux_handle,
            cursor: 0,
            flags: FD_OPEN | FD_DEVICE,
        };
        // 4 = stdprn — not connected
        // slots 4..19 remain closed
    }
}

/// Allocate a DOS file descriptor. Returns the DOS handle number, or None if full.
fn alloc_dos_fd() -> Option<u16> {
    unsafe {
        for i in 5..MAX_DOS_FILES {
            if !DOS_FDS[i].is_open() {
                return Some(i as u16);
            }
        }
        None
    }
}

/// Get a reference to a DOS file descriptor, if valid and open.
fn get_dos_fd(dos_handle: u16) -> Option<&'static DosFileDescriptor> {
    let idx = dos_handle as usize;
    if idx >= MAX_DOS_FILES {
        return None;
    }
    unsafe {
        if DOS_FDS[idx].is_open() {
            Some(&DOS_FDS[idx])
        } else {
            None
        }
    }
}

/// Get a mutable reference to a DOS file descriptor, if valid and open.
fn get_dos_fd_mut(dos_handle: u16) -> Option<&'static mut DosFileDescriptor> {
    let idx = dos_handle as usize;
    if idx >= MAX_DOS_FILES {
        return None;
    }
    unsafe {
        if DOS_FDS[idx].is_open() {
            Some(&mut DOS_FDS[idx])
        } else {
            None
        }
    }
}

fn setup_psp() {
    let psp = unsafe { &mut *(PSP_BASE as *mut Psp) };
    // Zero the whole thing first
    unsafe {
        core::ptr::write_bytes(PSP_BASE as *mut u8, 0, 256);
    }
    psp.int20 = [0xCD, 0x20];
    psp.mem_top_segment = DOS_MEM_TOP_SEGMENT;
    psp.int21_retf = [0xCD, 0x21, 0xCB];
    // Standard JFT: stdin=0, stdout=1, stderr=1, stdaux=2, stdprn=0xFF
    psp.jft[0] = 0x00; // stdin
    psp.jft[1] = 0x01; // stdout
    psp.jft[2] = 0x01; // stderr
    psp.jft[3] = 0x02; // stdaux
    psp.jft[4] = 0xFF; // stdprn (not open)
    for i in 5..20 {
        psp.jft[i] = 0xFF;
    }
    psp.jft_size = 20;
    // Command tail: empty
    psp.cmdtail_len = 0;
    psp.cmdtail[0] = 0x0D;
}

global_asm!(
    r#"
.global _start

_start:
    push ebx
    call dos_loader_start
"#
);

static mut TERMIOS_ORIG: Termios = Termios::default();
static STDIN: Handle = Handle::new(0);
static KBD_HANDLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static LOG_HANDLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn dos_log(msg: &[u8]) {
    let h = LOG_HANDLE.load(core::sync::atomic::Ordering::Relaxed);
    if h != 0 {
        let _ = write_sync(Handle::new(h), msg, 0);
    }
}

/// Resolve a seg:offset pointer to a linear address.
/// In v86 mode: (seg << 4) + (offset & 0xffff) (real-mode addressing).
/// In DPMI mode: descriptor base + offset (flat addressing via LDT shadow).
fn resolve_ptr(seg: u32, offset: u32) -> u32 {
    unsafe {
        if DPMI_ACTIVE {
            let base = dpmi_ldt_read(seg).base;
            base + offset
        } else {
            (seg << 4) + (offset & 0xffff)
        }
    }
}

/// Format "PREFIX XX\n" where XX is a hex byte.
fn fmt_unsupported(prefix: &[u8], value: u8, buf: &mut [u8; 32]) -> usize {
    let hex = b"0123456789ABCDEF";
    let mut i = 0;
    for &b in prefix {
        buf[i] = b;
        i += 1;
    }
    buf[i] = hex[(value >> 4) as usize];
    i += 1;
    buf[i] = hex[(value & 0xf) as usize];
    i += 1;
    buf[i] = b'\n';
    i + 1
}
/// IRQ mask passed to enter_8086, built up as the DOS program sets interrupt vectors
static mut VM86_IRQ_MASK: u32 = 0;
/// Virtual interrupt flag — tracks whether the DOS program has done CLI/STI
static mut VM86_IF: bool = true;

/// Current working directory — full IDOS path ending in '\', e.g. "A:\"
static mut DOS_CWD: [u8; 256] = [0; 256];
static mut DOS_CWD_LEN: usize = 0;

/// Current VGA video mode (default 0x03 = 80x25 text)
static mut VGA_MODE: u8 = 0x03;
/// Physical address of the IDOS graphics buffer (returned by TSETGFX ioctl).
/// Zero means no graphics mode is active.
static mut GFX_BUFFER_PADDR: u32 = 0;
/// Virtual address where we mapped the graphics buffer (for writing dirty rect).
static mut GFX_BUFFER_VADDR: u32 = 0;
/// Size of the mapped graphics buffer in bytes.
static mut GFX_BUFFER_SIZE: u32 = 0;

/// VGA palette: 256 entries of (R, G, B), used for INT 10h AH=10h palette ops.
/// Initialized to the standard VGA 256-color palette.
static mut VGA_PALETTE: [u8; 768] = [0; 768];

/// Enter graphics mode: request a graphics buffer from the console via ioctl,
/// map it into our address space, and map shadow RAM at 0xA0000 for the v86 program.
fn enter_graphics_mode(width: u16, height: u16, bpp: u8) {
    use idos_api::io::termios::{GraphicsMode, TSETGFX};

    let pixel_bytes = width as u32 * height as u32 * ((bpp as u32 + 7) / 8);

    // Shadow RAM at 0xA0000 is already mapped by setup_dos_memory.

    let mut gfx_mode = GraphicsMode {
        width,
        height,
        bpp_flags: bpp as u32,
        framebuffer: 0,
    };

    let _ = ioctl_sync(
        STDIN,
        TSETGFX,
        &mut gfx_mode as *mut GraphicsMode as u32,
        core::mem::size_of::<GraphicsMode>() as u32,
    );

    let paddr = gfx_mode.framebuffer;
    if paddr == 0 {
        return;
    }

    let buf_size = 8 + pixel_bytes;
    let buf_pages = (buf_size + 0xfff) & !0xfff;

    // Map the graphics buffer into our address space so we can write the dirty
    // rect and copy pixel data into it.
    let vaddr = map_memory(None, buf_pages, Some(paddr)).unwrap_or(0);

    unsafe {
        GFX_BUFFER_PADDR = paddr;
        GFX_BUFFER_VADDR = vaddr;
        GFX_BUFFER_SIZE = pixel_bytes;
    }

    // Read the kernel's default palette into our local copy
    load_idos_palette();
}

/// Exit graphics mode: unmap the graphics buffer and tell the console to
/// return to text mode.
fn exit_graphics_mode() {
    use idos_api::io::termios::TSETTEXT;

    let _ = ioctl_sync(STDIN, TSETTEXT, 0, 0);

    unsafe {
        GFX_BUFFER_PADDR = 0;
        GFX_BUFFER_VADDR = 0;
        GFX_BUFFER_SIZE = 0;
    }
}

/// Copy the shadow VGA framebuffer at 0xA0000 into the IDOS graphics buffer
/// and mark it dirty so the compositor redraws.
fn sync_graphics_buffer() {
    unsafe {
        let vaddr = core::ptr::read_volatile(&GFX_BUFFER_VADDR);
        let size = core::ptr::read_volatile(&GFX_BUFFER_SIZE);
        if vaddr == 0 || size == 0 {
            return;
        }
        let src = 0xA0000 as *const u8;
        let dst = (vaddr + 8) as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, size as usize);

        // Write dirty rect header: full screen
        let header = vaddr as *mut u16;
        core::ptr::write_volatile(header, 0); // x
        core::ptr::write_volatile(header.add(1), 0); // y
        core::ptr::write_volatile(header.add(2), 0xFFFF); // w (full)
        core::ptr::write_volatile(header.add(3), 0xFFFF); // h (full)
    }
}

/// Read the console's current palette into VGA_PALETTE.
fn load_idos_palette() {
    use idos_api::io::termios::TGETPAL;
    unsafe {
        let _ = ioctl_sync(STDIN, TGETPAL, VGA_PALETTE.as_mut_ptr() as u32, 768);
    }
}

/// Push VGA_PALETTE to the console.
fn push_idos_palette() {
    use idos_api::io::termios::TSETPAL;
    unsafe {
        let _ = ioctl_sync(STDIN, TSETPAL, VGA_PALETTE.as_ptr() as u32, 768);
    }
}

/// Initialize the CWD from the executable's path (strip the filename).
fn init_cwd(exec_path: &[u8]) {
    // Find the last backslash to get the directory portion
    let mut last_slash = 0;
    for i in 0..exec_path.len() {
        if exec_path[i] == b'\\' {
            last_slash = i;
        }
    }
    // Include the trailing backslash
    let dir_len = last_slash + 1;
    unsafe {
        DOS_CWD[..dir_len].copy_from_slice(&exec_path[..dir_len]);
        DOS_CWD_LEN = dir_len;
    }
}

/// Resolve a DOS path to a full IDOS path.
/// - Absolute paths (contain ':') are returned as-is (uppercased).
/// - Relative paths are prefixed with the current working directory.
/// Returns (buffer, length).
fn resolve_dos_path(dos_path: &[u8], buf: &mut [u8; 256]) -> usize {
    // Check if path is absolute (has a colon, like "C:\FILE.TXT")
    let has_drive = dos_path.iter().any(|&b| b == b':');
    if has_drive {
        let len = dos_path.len().min(256);
        for i in 0..len {
            buf[i] = dos_path[i].to_ascii_uppercase();
        }
        return len;
    }

    // Relative path — prepend CWD
    unsafe {
        let cwd_len = DOS_CWD_LEN;
        buf[..cwd_len].copy_from_slice(&DOS_CWD[..cwd_len]);
        let path_len = dos_path.len().min(256 - cwd_len);
        for i in 0..path_len {
            buf[cwd_len + i] = dos_path[i].to_ascii_uppercase();
        }
        cwd_len + path_len
    }
}

#[no_mangle]
pub extern "C" fn dos_loader_start(load_info_addr: u32) -> ! {
    // 1. Read load info page
    let header = unsafe { &*(load_info_addr as *const LoadInfoHeader) };

    if header.magic != LOAD_INFO_MAGIC {
        idos_api::syscall::exec::terminate(0xff);
    }

    // 2. Extract executable path
    let exec_path = unsafe {
        let path_ptr = (load_info_addr + header.exec_path_offset) as *const u8;
        core::slice::from_raw_parts(path_ptr, header.exec_path_len as usize)
    };
    let exec_path_str = unsafe { core::str::from_utf8_unchecked(exec_path) };

    init_cwd(exec_path);

    // 3. Open the executable and read the first 2 bytes to detect format
    let file_handle = create_file_handle();
    if open_sync(file_handle, exec_path_str, 0).is_err() {
        idos_api::syscall::exec::terminate(0xfe);
    }

    let mut magic: [u8; 2] = [0; 2];
    let _ = read_sync(file_handle, &mut magic, 0);

    let is_mz = (magic == [b'M', b'Z']) || (magic == [b'Z', b'M']);

    if is_mz {
        load_mz_exe(file_handle);
    } else {
        load_com(file_handle);
    }
}

/// Address of a default IRET stub in low memory, just past the BIOS data area.
const IRET_STUB: u32 = 0x500;
const IRET_STUB_SEGMENT: u16 = 0x0050;
const IRET_STUB_OFFSET: u16 = 0x0000;

/// DPMI entry point stub: INT 0xFE at 0x501, followed by RETF at 0x503.
/// The v86 program calls this via FAR CALL; INT 0xFE triggers the DPMI switch.
const DPMI_ENTRY_STUB: u32 = 0x501;
const DPMI_ENTRY_SEGMENT: u16 = 0x0050;
const DPMI_ENTRY_OFFSET: u16 = 0x0001;
/// Interrupt number used by the DPMI entry stub.
const DPMI_ENTRY_INT: u8 = 0xFE;

/// Map the DOS conventional memory region: zero page (IVT/BDA) through DOS_MEM_TOP.
/// Initializes the IVT with default IRET handlers.
fn setup_dos_memory() {
    // Map the zero page for the IVT (interrupt vector table) and BIOS data area
    let _ = map_memory(Some(0), 0x1000, None);
    // Map memory from PSP_BASE up to the top of conventional DOS memory
    let dos_region_size = DOS_MEM_TOP - PSP_BASE;
    let pages = (dos_region_size + 0xfff) / 0x1000;
    let _ = map_memory(Some(PSP_BASE), pages * 0x1000, None);

    // Map the video memory area (0xA0000-0xBFFFF) as shadow RAM.
    // Programs write here thinking it's VGA memory; we intercept and composite.
    let _ = map_memory(Some(0x000A_0000), 0x2_0000, None);

    // Identity-map BIOS ROM area (0xC0000-0xFFFFF) so programs can read
    // video BIOS, system BIOS, and other ROM data.
    let _ = map_memory(Some(0x000C_0000), 0x4_0000, Some(0x000C_0000));

    // Place an IRET instruction at the stub address
    unsafe {
        core::ptr::write_volatile(IRET_STUB as *mut u8, 0xCF); // IRET
    }

    // Place the DPMI entry stub: INT 0xFE (CD FE) + RETF (CB)
    unsafe {
        let stub = DPMI_ENTRY_STUB as *mut u8;
        core::ptr::write_volatile(stub, 0xCD); // INT
        core::ptr::write_volatile(stub.add(1), DPMI_ENTRY_INT); // 0xFE
        core::ptr::write_volatile(stub.add(2), 0xCB); // RETF
    }

    // Point all 256 IVT entries to the IRET stub
    let ivt = 0 as *mut u16;
    for i in 0..256 {
        unsafe {
            core::ptr::write_volatile(ivt.add(i * 2), IRET_STUB_OFFSET);
            core::ptr::write_volatile(ivt.add(i * 2 + 1), IRET_STUB_SEGMENT);
        }
    }

    // Populate the BIOS Data Area (BDA) at 0x0040:0x0000 (linear 0x400)
    // Programs (especially Watcom's graph.lib) read these to determine
    // screen geometry. All-zero BDA causes divide-by-zero crashes.
    unsafe {
        let bda = 0x400 as *mut u8;
        // 0x449: current video mode (03h = 80x25 text)
        core::ptr::write_volatile(bda.add(0x49), 0x03);
        // 0x44A-0x44B: number of screen columns (80)
        core::ptr::write_volatile((bda.add(0x4A)) as *mut u16, 80);
        // 0x44C-0x44D: video page size in bytes (4000 = 80*25*2)
        core::ptr::write_volatile((bda.add(0x4C)) as *mut u16, 4000);
        // 0x44E-0x44F: current page offset (0)
        core::ptr::write_volatile((bda.add(0x4E)) as *mut u16, 0);
        // 0x450-0x45F: cursor positions for 8 pages (row, col pairs) — leave zero
        // 0x460-0x461: cursor shape (start/end scanlines)
        core::ptr::write_volatile(bda.add(0x60), 0x0D); // end scanline
        core::ptr::write_volatile(bda.add(0x61), 0x0C); // start scanline
        // 0x462: current display page (0)
        core::ptr::write_volatile(bda.add(0x62), 0);
        // 0x463-0x464: CRT controller base port (0x3D4 for color)
        core::ptr::write_volatile((bda.add(0x63)) as *mut u16, 0x3D4);
        // 0x484: rows on screen minus 1 (24 for 25-row display)
        core::ptr::write_volatile(bda.add(0x84), 24);
        // 0x485-0x486: character height in scanlines (16 for VGA)
        core::ptr::write_volatile((bda.add(0x85)) as *mut u16, 16);
        // 0x487: EGA/VGA misc info
        core::ptr::write_volatile(bda.add(0x87), 0x60);
        // 0x489: VGA mode set option control
        core::ptr::write_volatile(bda.add(0x89), 0x11);
    }
}

/// Update BDA fields when the video mode changes.
fn update_bda_for_mode(mode: u8) {
    unsafe {
        let bda = 0x400 as *mut u8;
        core::ptr::write_volatile(bda.add(0x49), mode);
        match mode {
            0x13 => {
                // 320x200x256
                core::ptr::write_volatile((bda.add(0x4A)) as *mut u16, 40); // columns
                core::ptr::write_volatile((bda.add(0x4C)) as *mut u16, 0); // page size (N/A)
                core::ptr::write_volatile(bda.add(0x84), 24); // rows - 1
            }
            _ => {
                // 80x25 text
                core::ptr::write_volatile((bda.add(0x4A)) as *mut u16, 80);
                core::ptr::write_volatile((bda.add(0x4C)) as *mut u16, 4000);
                core::ptr::write_volatile(bda.add(0x84), 24);
            }
        }
        // Reset cursor and page
        core::ptr::write_volatile((bda.add(0x4E)) as *mut u16, 0);
        core::ptr::write_volatile(bda.add(0x62), 0);
    }
}

/// Load a .COM file: read the entire file into memory at PSP_BASE + 0x100,
/// then enter the VM with CS:IP = PSP_SEGMENT:0x100.
fn load_com(file_handle: Handle) -> ! {
    let mut file_status = FileStatus::new();
    let _ = io_sync(
        file_handle,
        FILE_OP_STAT,
        &mut file_status as *mut FileStatus as u32,
        core::mem::size_of::<FileStatus>() as u32,
        0,
    );
    let file_size = file_status.byte_size;

    setup_dos_memory();

    setup_psp();
    read_file_into(file_handle, PSP_BASE + 0x100, file_size, 0);

    // Register the program's initial block in the arena — it owns all
    // conventional memory from PSP_SEGMENT to DOS_MEM_TOP. Programs use
    // AH=4A to shrink this block, freeing the rest for allocation.
    unsafe {
        DOS_ARENA_START = PSP_SEGMENT as u16;
        DOS_ARENA[0] = (PSP_SEGMENT as u16, DOS_MEM_TOP_SEGMENT - PSP_SEGMENT as u16);
    }

    let _ = close_sync(file_handle);

    // .COM: all segment registers = PSP_SEGMENT, IP = 0x100
    compat_start(VMRegisters {
        eax: 0x00,
        ebx: 0x00,
        ecx: 0x00,
        edx: 0x00,
        esi: 0x00,
        edi: 0x00,
        ebp: 0x00,
        eip: 0x100,
        esp: 0xfffe,
        eflags: 0x2,
        cs: PSP_SEGMENT,
        ss: PSP_SEGMENT,
        es: PSP_SEGMENT,
        ds: PSP_SEGMENT,
        fs: PSP_SEGMENT,
        gs: PSP_SEGMENT,
    });
}

/// Load an MZ EXE: parse the header, load the program image after the PSP,
/// apply segment relocations, and enter the VM with CS:IP and SS:SP from the header.
fn load_mz_exe(file_handle: Handle) -> ! {
    // Read the MZ header
    let mut mz = MzHeader::default();
    let mz_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            &mut mz as *mut MzHeader as *mut u8,
            core::mem::size_of::<MzHeader>(),
        )
    };
    let _ = read_sync(file_handle, mz_bytes, 0);

    // Calculate image size (total file size minus header)
    let header_size = mz.header_paragraphs as u32 * 16;
    let file_image_size = if mz.last_page_bytes == 0 {
        mz.total_pages as u32 * 512
    } else {
        (mz.total_pages as u32 - 1) * 512 + mz.last_page_bytes as u32
    } - header_size;

    setup_dos_memory();

    setup_psp();

    // Load the program image at PSP_BASE + 0x100 (after the 256-byte PSP)
    let load_addr = PSP_BASE + 0x100;
    read_file_into(file_handle, load_addr, file_image_size, header_size);

    // Apply relocations: each entry points to a 16-bit word that needs
    // the load segment added to it
    let reloc_count = mz.relocation_count as u32;
    let mut reloc_file_offset = mz.relocation_offset as u32;
    for _ in 0..reloc_count {
        let mut reloc = MzRelocation::default();
        let reloc_bytes = unsafe {
            core::slice::from_raw_parts_mut(
                &mut reloc as *mut MzRelocation as *mut u8,
                core::mem::size_of::<MzRelocation>(),
            )
        };
        let _ = read_sync(file_handle, reloc_bytes, reloc_file_offset);
        reloc_file_offset += 4;

        // The fixup address in flat memory
        let fixup_addr = load_addr + reloc.segment as u32 * 16 + reloc.offset as u32;
        let ptr = fixup_addr as *mut u16;
        unsafe {
            let prev = core::ptr::read_volatile(ptr);
            core::ptr::write_volatile(ptr, prev.wrapping_add(PROGRAM_SEGMENT as u16));
        }
    }

    // Register the program's initial block — it owns all conventional memory.
    // Programs use AH=4A to shrink this, freeing the rest for allocation.
    unsafe {
        DOS_ARENA_START = PSP_SEGMENT as u16;
        DOS_ARENA[0] = (PSP_SEGMENT as u16, DOS_MEM_TOP_SEGMENT - PSP_SEGMENT as u16);
    }

    let _ = close_sync(file_handle);

    // MZ EXE: CS and SS are relative to the load segment (PROGRAM_SEGMENT)
    compat_start(VMRegisters {
        eax: 0x00,
        ebx: 0x00,
        ecx: 0x00,
        edx: 0x00,
        esi: 0x00,
        edi: 0x00,
        ebp: 0x00,
        eip: mz.initial_ip as u32,
        esp: mz.initial_sp as u32,
        eflags: 0x2,
        cs: PROGRAM_SEGMENT + mz.initial_cs as u32,
        ss: PROGRAM_SEGMENT + mz.initial_ss as u32,
        es: PSP_SEGMENT,
        ds: PSP_SEGMENT,
        fs: PSP_SEGMENT,
        gs: PSP_SEGMENT,
    });
}

/// Helper: read `size` bytes from `file_handle` at `file_offset` into `dest_addr`.
fn read_file_into(file_handle: Handle, dest_addr: u32, size: u32, file_offset: u32) {
    let dest = unsafe { core::slice::from_raw_parts_mut(dest_addr as *mut u8, size as usize) };
    let mut read_offset: u32 = 0;
    while read_offset < size {
        let chunk = &mut dest[read_offset as usize..size as usize];
        match read_sync(file_handle, chunk, file_offset + read_offset) {
            Ok(bytes_read) => {
                read_offset += bytes_read;
            }
            Err(_) => {
                idos_api::syscall::exec::terminate(0xfd);
            }
        }
    }
}

fn compat_start(mut vm_regs: VMRegisters) -> ! {
    // BSS is not guaranteed zeroed — explicitly init graphics state
    unsafe {
        GFX_BUFFER_PADDR = 0;
        GFX_BUFFER_VADDR = 0;
        GFX_BUFFER_SIZE = 0;
        VGA_MODE = 0x03;
        DPMI_ACTIVE = false;
        DPMI_CS_SEL = 0;
        DPMI_DS_SEL = 0;
        DPMI_SS_SEL = 0;
        DPMI_ES_SEL = 0;
        DPMI_PM_STACK_BASE = 0;
        for i in 0..LDT_MAX_SLOTS {
            DPMI_LDT_SHADOW[i] = LdtDescriptorParams {
                base: 0,
                limit: 0,
                access: 0,
                flags: 0,
            };
        }
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            DOS_ARENA[i] = (0, 0);
        }
        DOS_ARENA_START = 0;
        for i in 0..DPMI_HIGH_MEM_MAX {
            DPMI_HIGH_MEM[i] = (0, 0);
        }
        TIMER_ACCUM = 0;
    }

    let mut termios = Termios::default();
    let _ = ioctl_sync(
        STDIN,
        idos_api::io::termios::TCGETS,
        &mut termios as *mut Termios as u32,
        core::mem::size_of::<Termios>() as u32,
    )
    .unwrap();

    unsafe {
        TERMIOS_ORIG = termios.clone();
    }
    termios.lflags &= !(idos_api::io::termios::ECHO | idos_api::io::termios::ICANON);
    let _ = ioctl_sync(
        STDIN,
        idos_api::io::termios::TCSETS,
        &termios as *const Termios as u32,
        core::mem::size_of::<Termios>() as u32,
    );

    let stdaux = create_file_handle();
    let _ = open_sync(stdaux, "DEV:\\COM1", 0);

    init_file_table(stdaux);

    let kbd = create_file_handle();
    let _ = open_sync(kbd, "DEV:\\KEYBOARD", 0);
    KBD_HANDLE.store(kbd.as_u32(), core::sync::atomic::Ordering::Relaxed);

    let log = create_file_handle();
    let _ = open_sync(log, "LOG:\\DOSLAYER", 0);
    LOG_HANDLE.store(log.as_u32(), core::sync::atomic::Ordering::Relaxed);

    loop {
        let irq_mask = unsafe {
            if VM86_IF {
                VM86_IRQ_MASK
            } else {
                0
            }
        };
        let exit_reason = idos_api::syscall::exec::enter_8086(&mut vm_regs, irq_mask);

        match exit_reason {
            idos_api::compat::VM86_EXIT_GPF => unsafe {
                if !handle_fault(&mut vm_regs) {
                    break;
                }
            },
            _ if (exit_reason & 0xFF) == idos_api::compat::VM86_EXIT_DEBUG => {
                // Hardware interrupt delivery — TF was set by the kernel
                // Clear TF from the saved eflags so we don't keep trapping
                vm_regs.eflags &= !0x100;
                // Deliver pending virtual interrupts
                let pending = exit_reason >> 8;
                if pending != 0 {
                    dos_log(b"DEBUG exit, delivering IRQs\n");
                }
                deliver_pending_irqs(pending, &mut vm_regs);
            }
            _ => break,
        }
    }

    exit(0);
}

/// Toggle canonical mode (ICANON + ECHO) on stdin.
/// When enabled, the console line-buffers input with echo and editing.
fn set_stdin_canonical(enable: bool) {
    let mut termios = Termios::default();
    let _ = ioctl_sync(
        STDIN,
        idos_api::io::termios::TCGETS,
        &mut termios as *mut Termios as u32,
        core::mem::size_of::<Termios>() as u32,
    );
    if enable {
        termios.lflags |= idos_api::io::termios::ECHO | idos_api::io::termios::ICANON;
    } else {
        termios.lflags &= !(idos_api::io::termios::ECHO | idos_api::io::termios::ICANON);
    }
    let _ = ioctl_sync(
        STDIN,
        idos_api::io::termios::TCSETS,
        &termios as *const Termios as u32,
        core::mem::size_of::<Termios>() as u32,
    );
}

fn exit(code: u32) -> ! {
    // exit graphics mode if active
    unsafe {
        if GFX_BUFFER_PADDR != 0 {
            exit_graphics_mode();
        }
    }

    // reset termios
    unsafe {
        let _ = ioctl_sync(
            STDIN,
            idos_api::io::termios::TCSETS,
            &raw const TERMIOS_ORIG as *const Termios as u32,
            core::mem::size_of::<Termios>() as u32,
        );
    }

    idos_api::syscall::exec::terminate(code)
}

unsafe fn handle_fault(vm_regs: &mut VMRegisters) -> bool {
    let op_ptr = ((vm_regs.cs << 4) + vm_regs.eip) as *const u8;
    match *op_ptr {
        0x9c => {
            // PUSHF — push flags onto the v86 stack
            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_sub(2);
            let stack_addr = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            core::ptr::write_volatile(stack_addr as *mut u16, vm_regs.eflags as u16);
            vm_regs.eip += 1;
        }
        0x9d => {
            // POPF — pop flags from the v86 stack
            let stack_addr = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            let flags = core::ptr::read_volatile(stack_addr as *const u16) as u32;
            // Preserve VM flag and IOPL, update the rest
            vm_regs.eflags = (vm_regs.eflags & 0xFFF20000) | (flags & 0x0000FFFF);
            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_add(2);
            vm_regs.eip += 1;
        }
        0xcd => {
            // INT nn
            let irq = *op_ptr.add(1);
            handle_interrupt(irq, vm_regs);
            sync_graphics_buffer();
            vm_regs.eip += 2;
        }
        0xcf => {
            // IRET — pop IP, CS, FLAGS from v86 stack
            let stack_addr = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            let ip = core::ptr::read_volatile(stack_addr as *const u16) as u32;
            let cs = core::ptr::read_volatile((stack_addr + 2) as *const u16) as u32;
            let flags = core::ptr::read_volatile((stack_addr + 4) as *const u16) as u32;
            vm_regs.eip = ip;
            vm_regs.cs = cs;
            vm_regs.eflags = (vm_regs.eflags & 0xFFF20000) | (flags & 0x0000FFFF);
            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_add(6);
            return true; // don't advance EIP, we set it directly
        }
        0xf4 => {
            // HLT — stop execution
            return false;
        }
        0xfa => {
            // CLI
            VM86_IF = false;
            vm_regs.eip += 1;
        }
        0xfb => {
            // STI
            VM86_IF = true;
            vm_regs.eip += 1;
        }
        _ => {
            return false;
        }
    }

    true
}

/// BIOS keyboard services (INT 16h)
fn bios_keyboard(regs: &mut VMRegisters) {
    match regs.ah() {
        0x00 | 0x10 => {
            // AH=00/10: Blocking read — wait for a keypress
            // Returns AH=IBM scancode, AL=ASCII character
            // First check lookahead / any non-blocking keyboard data
            if let Some((scancode, ascii)) = read_next_key() {
                regs.set_ah(scancode);
                regs.set_al(ascii);
                return;
            }
            // Block on stdin (console), which supports blocking reads.
            let stdin = Handle::new(0);
            loop {
                let mut buf = [0u8; 1];
                if let Ok(1) = read_sync(stdin, &mut buf, 0) {
                    regs.set_ah(0);
                    regs.set_al(buf[0]);
                    return;
                }
            }
        }
        0x01 | 0x11 => {
            // AH=01/11: Check if key available (non-blocking)
            // If key available: ZF=0, AX=key data (key remains in buffer)
            // If no key: ZF=1
            if let Some((scancode, ascii)) = peek_next_key() {
                regs.set_ah(scancode);
                regs.set_al(ascii);
                // Clear ZF to indicate key available
                regs.eflags &= !0x40;
            } else {
                // Set ZF to indicate no key
                regs.eflags |= 0x40;
                // Yield so the keyboard driver can deliver data
                idos_api::syscall::exec::yield_coop();
            }
        }
        0x02 | 0x12 => {
            // AH=02/12: Get shift key status
            // Return 0 for now (no modifier keys pressed)
            regs.set_al(0);
        }
        _ => {}
    }
}

/// Read and consume the next key press from the keyboard device.
/// Returns (IBM_scancode, ASCII) or None if no key press is available.
fn read_next_key() -> Option<(u8, u8)> {
    // Check lookahead first (populated by peek_next_key)
    unsafe {
        if let Some(key) = KEY_LOOKAHEAD.take() {
            return Some(key);
        }
    }
    let kbd = Handle::new(KBD_HANDLE.load(core::sync::atomic::Ordering::Relaxed));
    let mut buf = [0u8; 2];
    loop {
        match idos_api::io::sync::read_sync(kbd, &mut buf, 0) {
            Ok(2) => {
                if buf[0] == 1 {
                    // Key press
                    if let Some(result) = keycode_to_bios(buf[1]) {
                        return Some(result);
                    }
                    // Modifier or unmapped key, skip
                }
                // Key release (buf[0] == 2), skip
            }
            _ => return None,
        }
    }
}

/// Peek at the next key press without consuming it.
/// Since the keyboard device doesn't support peek, we read into a small
/// lookahead buffer that read_next_key also checks.
fn peek_next_key() -> Option<(u8, u8)> {
    unsafe {
        if let Some(ref key) = KEY_LOOKAHEAD {
            return Some(*key);
        }
    }
    if let Some(key) = read_next_key() {
        unsafe {
            KEY_LOOKAHEAD = Some(key);
        }
        Some(key)
    } else {
        None
    }
}

static mut KEY_LOOKAHEAD: Option<(u8, u8)> = None;

/// DPMI state — true once the client has switched to protected mode.
static mut DPMI_ACTIVE: bool = false;
/// LDT selectors allocated for the DPMI client during entry.
static mut DPMI_CS_SEL: u32 = 0;
static mut DPMI_DS_SEL: u32 = 0;
static mut DPMI_SS_SEL: u32 = 0;
static mut DPMI_ES_SEL: u32 = 0;
/// Base address of the DPMI protected-mode stack (mmap'd region).
static mut DPMI_PM_STACK_BASE: u32 = 0;
/// Size of the DPMI protected-mode stack.
const DPMI_PM_STACK_SIZE: u32 = 0x4000; // 16 KiB

/// Simple conventional memory allocator.
/// Tracks allocated blocks as (segment, size_in_paragraphs) pairs.
/// Free space starts at DOS_ARENA_START and goes up to DOS_MEM_TOP_SEGMENT.
const DOS_ARENA_MAX_BLOCKS: usize = 32;
/// Each entry: (segment, size_paragraphs). segment=0 means free slot.
static mut DOS_ARENA: [(u16, u16); DOS_ARENA_MAX_BLOCKS] = [(0, 0); DOS_ARENA_MAX_BLOCKS];
/// Start of free conventional memory (paragraph/segment). Set after program load.
static mut DOS_ARENA_START: u16 = 0;

/// High memory block tracker for DPMI 0x501/0x502.
/// Each entry: (linear_address, size_bytes). address=0 means free slot.
const DPMI_HIGH_MEM_MAX: usize = 32;
static mut DPMI_HIGH_MEM: [(u32, u32); DPMI_HIGH_MEM_MAX] = [(0, 0); DPMI_HIGH_MEM_MAX];

/// Shadow copy of descriptor params for each LDT slot.
/// Needed because the kernel syscall only supports full replacement,
/// but DPMI lets clients modify base/limit/access independently.
const LDT_MAX_SLOTS: usize = 64;
static mut DPMI_LDT_SHADOW: [LdtDescriptorParams; LDT_MAX_SLOTS] = {
    const ZERO: LdtDescriptorParams = LdtDescriptorParams {
        base: 0,
        limit: 0,
        access: 0,
        flags: 0,
    };
    [ZERO; LDT_MAX_SLOTS]
};

/// Map an IDOS KeyCode byte to (IBM_scancode, ASCII).
/// KeyCode values match kernel/src/hardware/ps2/keycodes.rs KeyCode enum.
fn keycode_to_bios(keycode: u8) -> Option<(u8, u8)> {
    // KeyCode -> (IBM scancode, ASCII lowercase)
    // IBM scancodes from the standard scan code set 1 make codes
    let (scancode, ascii) = match keycode {
        0x08 => (0x0E, 0x08u8), // Backspace
        0x09 => (0x0F, 0x09),   // Tab
        0x0D => (0x1C, 0x0D),   // Enter
        0x1B => (0x01, 0x1B),   // Escape
        0x20 => (0x39, 0x20),   // Space

        // Numbers 0-9
        0x30 => (0x0B, b'0'),
        0x31 => (0x02, b'1'),
        0x32 => (0x03, b'2'),
        0x33 => (0x04, b'3'),
        0x34 => (0x05, b'4'),
        0x35 => (0x06, b'5'),
        0x36 => (0x07, b'6'),
        0x37 => (0x08, b'7'),
        0x38 => (0x09, b'8'),
        0x39 => (0x0A, b'9'),

        // Letters A-Z (lowercase ASCII)
        0x41 => (0x1E, b'a'),
        0x42 => (0x30, b'b'),
        0x43 => (0x2E, b'c'),
        0x44 => (0x20, b'd'),
        0x45 => (0x12, b'e'),
        0x46 => (0x21, b'f'),
        0x47 => (0x22, b'g'),
        0x48 => (0x23, b'h'),
        0x49 => (0x17, b'i'),
        0x4A => (0x24, b'j'),
        0x4B => (0x25, b'k'),
        0x4C => (0x26, b'l'),
        0x4D => (0x32, b'm'),
        0x4E => (0x31, b'n'),
        0x4F => (0x18, b'o'),
        0x50 => (0x19, b'p'),
        0x51 => (0x10, b'q'),
        0x52 => (0x13, b'r'),
        0x53 => (0x1F, b's'),
        0x54 => (0x14, b't'),
        0x55 => (0x16, b'u'),
        0x56 => (0x2F, b'v'),
        0x57 => (0x11, b'w'),
        0x58 => (0x2D, b'x'),
        0x59 => (0x15, b'y'),
        0x5A => (0x2C, b'z'),

        // Punctuation
        0x2C => (0x33, b','),
        0x2D => (0x0C, b'-'),
        0x2E => (0x34, b'.'),
        0x2F => (0x35, b'/'),
        0x3A => (0x27, b';'),
        0x3B => (0x28, b'\''),
        0x3D => (0x0D, b'='),
        0x5B => (0x1A, b'['),
        0x5C => (0x2B, b'\\'),
        0x5D => (0x1B, b']'),
        0x5F => (0x29, b'`'),

        // Arrow keys (extended, no ASCII)
        0x21 => (0x4B, 0x00), // Left
        0x22 => (0x48, 0x00), // Up
        0x23 => (0x4D, 0x00), // Right
        0x24 => (0x50, 0x00), // Down

        0x07 => (0x53, 0x00), // Delete

        // Modifiers and unmapped keys return None
        _ => return None,
    };
    Some((scancode, ascii))
}

/// Timer tick divider state. The kernel PIT runs at 100 Hz, but DOS programs
/// expect ~18.2 Hz. We accumulate fractional ticks: every time the accumulator
/// reaches the threshold, we deliver one DOS tick.
/// Using fixed-point: 100 Hz / 18.2 Hz ≈ 5.4945. We accumulate 182 per tick
/// and fire when it reaches 1000 (1000/182 ≈ 5.4945).
static mut TIMER_ACCUM: u32 = 0;
const TIMER_ACCUM_PER_TICK: u32 = 182;
const TIMER_ACCUM_THRESHOLD: u32 = 1000;

/// Deliver pending hardware interrupts to the v86 program.
/// For each pending IRQ, simulate a hardware interrupt: push FLAGS, CS, IP
/// onto the v86 stack and set CS:IP to the IVT vector.
fn deliver_pending_irqs(pending: u32, vm_regs: &mut VMRegisters) {
    // Map IRQ bits to interrupt vectors
    // Bit 0 = IRQ 0 (timer) → INT 1Ch (user timer tick)
    // Bit 1 = IRQ 1 (keyboard) → INT 9
    let irq_to_int: [(u32, u8); 2] = [
        (idos_api::compat::VM86_IRQ_TIMER, 0x1C),
        (idos_api::compat::VM86_IRQ_KEYBOARD, 0x09),
    ];

    for &(mask, int_num) in &irq_to_int {
        if pending & mask == 0 {
            continue;
        }
        // Rate-limit timer delivery to ~18.2 Hz
        if mask == idos_api::compat::VM86_IRQ_TIMER {
            unsafe {
                TIMER_ACCUM += TIMER_ACCUM_PER_TICK;
                if TIMER_ACCUM < TIMER_ACCUM_THRESHOLD {
                    continue;
                }
                TIMER_ACCUM -= TIMER_ACCUM_THRESHOLD;
            }
        }
        // Read the IVT entry for this interrupt
        let ivt_addr = (int_num as u32 * 4) as *const u16;
        let (vec_offset, vec_segment) = unsafe {
            (
                core::ptr::read_volatile(ivt_addr) as u32,
                core::ptr::read_volatile(ivt_addr.add(1)) as u32,
            )
        };
        // Skip if the vector points to our default IRET stub (no handler installed)
        if vec_segment == IRET_STUB_SEGMENT as u32 && vec_offset == IRET_STUB_OFFSET as u32 {
            continue;
        }
        // Simulate hardware interrupt: push FLAGS, CS, IP onto the v86 stack
        unsafe {
            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_sub(2);
            let sp = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            core::ptr::write_volatile(sp as *mut u16, vm_regs.eflags as u16);

            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_sub(2);
            let sp = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            core::ptr::write_volatile(sp as *mut u16, vm_regs.cs as u16);

            vm_regs.esp = (vm_regs.esp & 0xffff).wrapping_sub(2);
            let sp = (vm_regs.ss << 4) + (vm_regs.esp & 0xffff);
            core::ptr::write_volatile(sp as *mut u16, vm_regs.eip as u16);
        }
        // Set CS:IP to the handler
        vm_regs.cs = vec_segment;
        vm_regs.eip = vec_offset;
    }
}

fn handle_interrupt(irq: u8, vm_regs: &mut VMRegisters) {
    match irq {
        0x10 => {
            // BIOS video services
            bios_video(vm_regs);
        }
        0x16 => {
            // BIOS keyboard services
            bios_keyboard(vm_regs);
        }
        0x21 => {
            // DOS API
            dos_api(vm_regs);
        }
        0x2f => {
            // Multiplex interrupt
            multiplex_int(vm_regs);
        }
        DPMI_ENTRY_INT => {
            // DPMI entry point — switch to protected mode
            dpmi_enter(vm_regs);
        }

        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT ", irq, &mut buf);
            dos_log(&buf[..len]);
        }
    }
}

/// INT 0x2F — Multiplex interrupt.
/// AX=0x1687: DPMI detection / get entry point.
fn multiplex_int(regs: &mut VMRegisters) {
    let ax = (regs.eax & 0xffff) as u16;
    match ax {
        0x1687 => {
            // DPMI 0.9 host detection
            // AX=0 means DPMI is available
            regs.set_ax(0);
            // BX = flags (bit 0 = 32-bit programs supported)
            regs.ebx = (regs.ebx & 0xffff0000) | 0x0001;
            // CL = processor type (04 = 486+)
            regs.ecx = (regs.ecx & 0xffffff00) | 0x04;
            // DX = DPMI version (0.90)
            regs.set_dx(0x005A); // major=0, minor=90
            // SI = number of paragraphs needed for DPMI private data (PM stack)
            regs.esi = (regs.esi & 0xffff0000) | ((DPMI_PM_STACK_SIZE / 16) as u32);
            // ES:DI = entry point (real-mode far address)
            regs.es = DPMI_ENTRY_SEGMENT as u32;
            regs.edi = (regs.edi & 0xffff0000) | (DPMI_ENTRY_OFFSET as u32);
        }
        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT 2F AX=", (ax >> 8) as u8, &mut buf);
            dos_log(&buf[..len]);
        }
    }
}

/// DPMI entry — called when the v86 program invokes the DPMI entry point.
/// The real-mode registers tell us the client's 16-bit CS:IP (return address
/// on the v86 stack from the FAR CALL), DS, ES, SS:SP. We allocate flat LDT
/// descriptors, set up a PM stack, and enter the protected-mode dispatch loop.
///
/// Per the DPMI spec the client passes:
///   AX = 0 for 16-bit client, 1 for 32-bit client
///   ES = real-mode segment of DPMI host data area (we ignore this)
///
/// On entry to PM, the DPMI spec says:
///   CS:EIP = return address from the FAR CALL (where the client resumes in PM)
///   SS:ESP = PM stack
///   DS = selector with base = real-mode DS << 4, limit = 64K
///   ES = selector for PSP (base = PSP_BASE, limit = 256 bytes)
///   FS = GS = 0
///
/// DJGPP's CWSDPMI stub expects flat CS/DS/SS with base 0, limit 4 GiB after
/// it adjusts them itself via INT 31h. For the initial switch we give it
/// descriptors matching the real-mode segments so the return address works.
fn dpmi_enter(vm_regs: &mut VMRegisters) {
    dos_log(b"DPMI: entering protected mode\n");

    let is_32bit = (vm_regs.eax & 1) != 0;
    if !is_32bit {
        dos_log(b"DPMI: 16-bit clients not supported\n");
        // Signal failure: set carry flag
        vm_regs.eflags |= 1;
        return;
    }

    // Allocate LDT selectors for CS, DS, SS, ES
    let cs_sel = idos_api::syscall::ldt::ldt_allocate();
    let ds_sel = idos_api::syscall::ldt::ldt_allocate();
    let ss_sel = idos_api::syscall::ldt::ldt_allocate();
    let es_sel = idos_api::syscall::ldt::ldt_allocate();

    if cs_sel == 0xffff_ffff
        || ds_sel == 0xffff_ffff
        || ss_sel == 0xffff_ffff
        || es_sel == 0xffff_ffff
    {
        dos_log(b"DPMI: failed to allocate LDT selectors\n");
        vm_regs.eflags |= 1;
        return;
    }

    // The FAR CALL to the entry stub pushed CS:IP on the v86 stack.
    // The INT 0xFE inside the stub pushed FLAGS:CS:IP again.
    // After handle_fault advances EIP past the INT instruction, we'll return
    // to the RETF (0x503). But we actually want the return address from the
    // original FAR CALL, which is still on the v86 stack beneath the INT frame.
    //
    // The v86 stack currently has (from top):
    //   [handled by handle_fault's INT emulation — already consumed]
    //   FAR CALL return IP (2 bytes)
    //   FAR CALL return CS (2 bytes)
    //
    // Actually, the INT instruction faults to the kernel, which exits to us.
    // handle_fault sees INT 0xFE and dispatches here. It will advance EIP by 2
    // after we return. But we don't want to return to v86 — we want to enter PM.
    //
    // The FAR CALL pushed the return address on the v86 stack. Read it.
    let rm_sp = vm_regs.esp & 0xffff;
    let rm_ss = vm_regs.ss & 0xffff;
    let stack_lin = (rm_ss << 4) + rm_sp;
    let (ret_ip, ret_cs) = unsafe {
        let ip = core::ptr::read_volatile(stack_lin as *const u16) as u32;
        let cs = core::ptr::read_volatile((stack_lin + 2) as *const u16) as u32;
        (ip, cs)
    };

    // Linear address where the client expects to resume
    let _client_code_linear = (ret_cs << 4) + ret_ip;

    // Set up CS: base = real-mode CS << 4, limit = 64K, 32-bit code, DPL=3
    let cs_params = LdtDescriptorParams {
        base: ret_cs << 4,
        limit: 0xFFFF,
        access: 0xFA, // P=1 DPL=3 S=1 type=code,read (1111_1010)
        flags: 0x40,  // D=1 (32-bit), G=0 (byte granularity)
    };
    dpmi_ldt_write(cs_sel, &cs_params);

    // DS: base = real-mode DS << 4, limit = 64K, 32-bit data, DPL=3
    let ds_base = (vm_regs.ds & 0xffff) << 4;
    let ds_params = LdtDescriptorParams {
        base: ds_base,
        limit: 0xFFFF,
        access: 0xF2, // P=1 DPL=3 S=1 type=data,rw (1111_0010)
        flags: 0x40,  // B=1 (32-bit), G=0
    };
    dpmi_ldt_write(ds_sel, &ds_params);

    // SS: base 0, limit 4 GiB, 32-bit data, DPL=3 (flat for PM stack)
    let ss_params = LdtDescriptorParams {
        base: 0,
        limit: 0xFFFFF,
        access: 0xF2,
        flags: 0xC0, // G=1 (4K granularity), B=1 (32-bit)
    };
    dpmi_ldt_write(ss_sel, &ss_params);

    // ES: base = PSP, limit = 256 bytes, 32-bit data, DPL=3
    let es_params = LdtDescriptorParams {
        base: PSP_BASE,
        limit: 0xFF,
        access: 0xF2,
        flags: 0x40,
    };
    dpmi_ldt_write(es_sel, &es_params);

    // Allocate a PM stack
    let pm_stack_base = map_memory(None, DPMI_PM_STACK_SIZE, None).unwrap_or(0);
    if pm_stack_base == 0 {
        dos_log(b"DPMI: failed to allocate PM stack\n");
        vm_regs.eflags |= 1;
        return;
    }

    // Save selectors for later use
    unsafe {
        DPMI_CS_SEL = cs_sel;
        DPMI_DS_SEL = ds_sel;
        DPMI_SS_SEL = ss_sel;
        DPMI_ES_SEL = es_sel;
        DPMI_PM_STACK_BASE = pm_stack_base;
        DPMI_ACTIVE = true;
    }

    // Build PM register state
    let mut pm_regs = VMRegisters {
        eax: vm_regs.eax,
        ebx: vm_regs.ebx,
        ecx: vm_regs.ecx,
        edx: vm_regs.edx,
        esi: vm_regs.esi,
        edi: vm_regs.edi,
        ebp: vm_regs.ebp,
        eip: ret_ip, // offset within the CS segment
        cs: cs_sel,
        eflags: 0x200,                           // IF set
        esp: pm_stack_base + DPMI_PM_STACK_SIZE, // top of stack
        ss: ss_sel,
        es: es_sel,
        ds: ds_sel,
        fs: 0,
        gs: 0,
    };

    // Enter the PM dispatch loop (does not return to the v86 loop)
    dpmi_protected_mode_loop(&mut pm_regs);
}

/// Protected-mode dispatch loop for DPMI.
/// Runs enter_protected_mode in a loop, handling INT exits.
fn dpmi_protected_mode_loop(pm_regs: &mut VMRegisters) {
    loop {
        let exit_reason = idos_api::syscall::exec::enter_protected_mode(pm_regs);

        let reason_type = exit_reason & 0xFF;
        match reason_type {
            idos_api::compat::DPMI_EXIT_INT => {
                let int_num = ((exit_reason >> 16) & 0xFF) as u8;
                if !dpmi_handle_int(int_num, pm_regs) {
                    break;
                }
            }
            idos_api::compat::DPMI_EXIT_FAULT => {
                let err_code = exit_reason >> 16;
                let mut buf = [0u8; 32];
                let len = fmt_unsupported(b"DPMI: fault err=", (err_code & 0xFF) as u8, &mut buf);
                dos_log(&buf[..len]);
                break;
            }
            _ => {
                dos_log(b"DPMI: unknown exit reason\n");
                break;
            }
        }
    }

    // Fell out of PM loop — terminate
    exit(1);
}

/// Handle an interrupt from DPMI protected-mode code.
/// Returns true to continue execution, false to stop.
fn dpmi_handle_int(int_num: u8, regs: &mut VMRegisters) -> bool {
    match int_num {
        0x21 => {
            // DOS API — terminate (AH=4Ch) exits the PM loop
            if regs.ah() == 0x4C {
                return false;
            }
            // Reuse the same handlers as v86 mode; resolve_ptr()
            // takes care of pointer translation.
            dos_api(regs);
        }
        0x31 => {
            // DPMI services
            dpmi_int31(regs);
        }
        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"DPMI INT ", int_num, &mut buf);
            dos_log(&buf[..len]);
        }
    }
    true
}

/// Write descriptor params to the kernel LDT and update our shadow table.
fn dpmi_ldt_write(selector: u32, params: &LdtDescriptorParams) {
    let index = (selector >> 3) as usize;
    if index > 0 && index < LDT_MAX_SLOTS {
        unsafe {
            DPMI_LDT_SHADOW[index] = *params;
        }
    }
    idos_api::syscall::ldt::ldt_modify(selector, params);
}

/// Read the shadow copy of a descriptor's params.
fn dpmi_ldt_read(selector: u32) -> LdtDescriptorParams {
    let index = (selector >> 3) as usize;
    if index > 0 && index < LDT_MAX_SLOTS {
        unsafe { DPMI_LDT_SHADOW[index] }
    } else {
        LdtDescriptorParams {
            base: 0,
            limit: 0,
            access: 0,
            flags: 0,
        }
    }
}

/// Clear the shadow entry for a freed selector.
fn dpmi_ldt_clear(selector: u32) {
    let index = (selector >> 3) as usize;
    if index > 0 && index < LDT_MAX_SLOTS {
        unsafe {
            DPMI_LDT_SHADOW[index] = LdtDescriptorParams {
                base: 0,
                limit: 0,
                access: 0,
                flags: 0,
            };
        }
    }
}

/// INT 0x31 — DPMI API dispatch.
fn dpmi_int31(regs: &mut VMRegisters) {
    let ax = (regs.eax & 0xffff) as u16;

    match ax {
        // ---- Descriptor management (AH=00) ----
        0x0000 => {
            // Allocate LDT descriptor(s)
            // CX = number of descriptors to allocate
            // Returns: AX = base selector (CF=1 on error)
            let count = (regs.ecx & 0xffff) as u32;
            if count == 0 || count > 16 {
                regs.eflags |= 1;
                return;
            }
            // Allocate them; if any fail, free them all and return error.
            let mut sels = [0u32; 16];
            for i in 0..count as usize {
                let sel = idos_api::syscall::ldt::ldt_allocate();
                if sel == 0xffff_ffff {
                    // Free already-allocated
                    for j in 0..i {
                        idos_api::syscall::ldt::ldt_free(sels[j]);
                        dpmi_ldt_clear(sels[j]);
                    }
                    regs.eflags |= 1;
                    return;
                }
                sels[i] = sel;
                // Initialize shadow as present data segment, DPL=3
                let params = LdtDescriptorParams {
                    base: 0,
                    limit: 0,
                    access: 0xF2, // P=1 DPL=3 S=1 data r/w
                    flags: 0x40,  // D/B=1
                };
                dpmi_ldt_write(sel, &params);
            }
            // Return the first selector
            regs.set_ax(sels[0] as u16);
            regs.eflags &= !1; // clear CF
        }

        0x0001 => {
            // Free LDT descriptor
            // BX = selector
            let sel = regs.ebx & 0xffff;
            let result = idos_api::syscall::ldt::ldt_free(sel);
            if result == 0xffff_ffff {
                regs.eflags |= 1;
            } else {
                dpmi_ldt_clear(sel);
                regs.eflags &= !1;
            }
        }

        0x0007 => {
            // Set segment base address
            // BX = selector, CX:DX = 32-bit base address
            let sel = regs.ebx & 0xffff;
            let base = ((regs.ecx & 0xffff) << 16) | (regs.edx & 0xffff);
            let mut params = dpmi_ldt_read(sel);
            params.base = base;
            dpmi_ldt_write(sel, &params);
            regs.eflags &= !1;
        }

        0x0008 => {
            // Set segment limit
            // BX = selector, CX:DX = 32-bit limit
            let sel = regs.ebx & 0xffff;
            let limit = ((regs.ecx & 0xffff) << 16) | (regs.edx & 0xffff);
            let mut params = dpmi_ldt_read(sel);
            if limit > 0xFFFFF {
                // Need page granularity
                params.limit = limit >> 12;
                params.flags = (params.flags & 0x7F) | 0x80; // set G bit
            } else {
                params.limit = limit;
                params.flags = params.flags & 0x7F; // clear G bit
            }
            dpmi_ldt_write(sel, &params);
            regs.eflags &= !1;
        }

        0x0009 => {
            // Set descriptor access rights
            // BX = selector, CL = access byte, CH = flags (type/386 byte)
            let sel = regs.ebx & 0xffff;
            let access = regs.cl();
            let type_byte = regs.ch();
            let mut params = dpmi_ldt_read(sel);
            // Enforce DPL=3 — client can't escalate
            params.access = (access & 0x9F) | 0x60; // force DPL bits to 11
            params.flags = type_byte & 0xF0; // high nibble only (G, D/B, L, AVL)
            dpmi_ldt_write(sel, &params);
            regs.eflags &= !1;
        }

        0x000A => {
            // Create alias descriptor (data alias of a code segment)
            // BX = selector of code segment to alias
            // Returns: AX = new data selector
            let src_sel = regs.ebx & 0xffff;
            let src = dpmi_ldt_read(src_sel);
            // Allocate a new descriptor
            let new_sel = idos_api::syscall::ldt::ldt_allocate();
            if new_sel == 0xffff_ffff {
                regs.eflags |= 1;
                return;
            }
            // Copy base and limit, but make it a data segment
            let params = LdtDescriptorParams {
                base: src.base,
                limit: src.limit,
                access: (src.access & 0xF0) | 0x02, // keep P/DPL, type = data r/w
                flags: src.flags,
            };
            dpmi_ldt_write(new_sel, &params);
            regs.set_ax(new_sel as u16);
            regs.eflags &= !1;
        }

        // ---- DOS memory management (AH=01) ----
        0x0100 => {
            // Allocate DOS memory block
            // BX = paragraphs requested
            // Returns: AX = real-mode segment, DX = selector (CF=1 on error, BX = max avail)
            let paras = (regs.ebx & 0xffff) as u16;
            match dos_arena_alloc(paras) {
                Some(segment) => {
                    regs.set_ax(segment);
                    // Allocate an LDT selector for the block too
                    let sel = idos_api::syscall::ldt::ldt_allocate();
                    if sel != 0xffff_ffff {
                        let params = LdtDescriptorParams {
                            base: (segment as u32) << 4,
                            limit: (paras as u32) * 16 - 1,
                            access: 0xF2,
                            flags: 0x40,
                        };
                        dpmi_ldt_write(sel, &params);
                    }
                    regs.set_dx(sel as u16);
                    regs.eflags &= !1;
                }
                None => {
                    regs.ebx = (regs.ebx & 0xffff0000) | dos_arena_largest() as u32;
                    regs.eflags |= 1;
                }
            }
        }

        0x0101 => {
            // Free DOS memory block
            // DX = selector of block to free
            let sel = regs.edx & 0xffff;
            let params = dpmi_ldt_read(sel);
            let segment = (params.base >> 4) as u16;
            if dos_arena_free(segment) {
                idos_api::syscall::ldt::ldt_free(sel);
                dpmi_ldt_clear(sel);
                regs.eflags &= !1;
            } else {
                regs.eflags |= 1;
            }
        }

        0x0102 => {
            // Resize DOS memory block
            // BX = new size in paragraphs, DX = selector of block
            let new_paras = (regs.ebx & 0xffff) as u16;
            let sel = regs.edx & 0xffff;
            let params = dpmi_ldt_read(sel);
            let segment = (params.base >> 4) as u16;
            if dos_arena_resize(segment, new_paras) {
                // Update the descriptor limit
                let mut p = dpmi_ldt_read(sel);
                p.limit = (new_paras as u32) * 16 - 1;
                dpmi_ldt_write(sel, &p);
                regs.eflags &= !1;
            } else {
                regs.ebx = (regs.ebx & 0xffff0000) | dos_arena_largest() as u32;
                regs.eflags |= 1;
            }
        }

        // ---- Memory management (AH=05) ----
        0x0501 => {
            // Allocate memory block (above 1 MB)
            // BX:CX = size in bytes
            // Returns: BX:CX = linear address, SI:DI = handle
            let size = ((regs.ebx & 0xffff) << 16) | (regs.ecx & 0xffff);
            if size == 0 {
                regs.eflags |= 1;
                return;
            }
            // Round up to page size
            let alloc_size = (size + 0xFFF) & !0xFFF;
            match map_memory(None, alloc_size, None) {
                Ok(addr) => {
                    // Record in our tracking table
                    if let Some(handle) = dpmi_high_mem_record(addr, alloc_size) {
                        regs.ebx = (regs.ebx & 0xffff0000) | ((addr >> 16) & 0xffff);
                        regs.ecx = (regs.ecx & 0xffff0000) | (addr & 0xffff);
                        regs.esi = (regs.esi & 0xffff0000) | ((handle >> 16) & 0xffff);
                        regs.edi = (regs.edi & 0xffff0000) | (handle & 0xffff);
                        regs.eflags &= !1;
                    } else {
                        // Table full, unmap and fail
                        let _ = idos_api::syscall::memory::unmap_memory(addr, alloc_size);
                        regs.eflags |= 1;
                    }
                }
                Err(_) => {
                    regs.eflags |= 1;
                }
            }
        }

        0x0502 => {
            // Free memory block
            // SI:DI = handle (from 0x0501)
            let handle = ((regs.esi & 0xffff) << 16) | (regs.edi & 0xffff);
            if dpmi_high_mem_free(handle) {
                regs.eflags &= !1;
            } else {
                regs.eflags |= 1;
            }
        }

        // ---- Interrupt management (AH=02) ----
        0x0200 => {
            // Get real-mode interrupt vector
            // BL = interrupt number
            // Returns: CX:DX = segment:offset
            let int_num = regs.bl() as u32;
            let ivt_addr = (int_num * 4) as *const u16;
            unsafe {
                let offset = core::ptr::read_volatile(ivt_addr) as u32;
                let segment = core::ptr::read_volatile(ivt_addr.add(1)) as u32;
                regs.ecx = (regs.ecx & 0xffff0000) | segment;
                regs.edx = (regs.edx & 0xffff0000) | offset;
            }
            regs.eflags &= !1;
        }

        0x0201 => {
            // Set real-mode interrupt vector
            // BL = interrupt number, CX:DX = segment:offset
            let int_num = regs.bl() as u32;
            let segment = regs.ecx & 0xffff;
            let offset = regs.edx & 0xffff;
            let ivt_addr = (int_num * 4) as *mut u16;
            unsafe {
                core::ptr::write_volatile(ivt_addr, offset as u16);
                core::ptr::write_volatile(ivt_addr.add(1), segment as u16);
            }
            // Update IRQ mask if hooking a hardware interrupt
            let irq = match int_num {
                0x08..=0x0F => Some(int_num - 0x08),
                0x70..=0x77 => Some(int_num - 0x70 + 8),
                0x1C => Some(0),
                _ => None,
            };
            if let Some(irq_num) = irq {
                unsafe {
                    VM86_IRQ_MASK |= 1 << irq_num;
                }
            }
            regs.eflags &= !1;
        }

        // ---- Real-mode interrupt simulation (AH=03) ----
        0x0300 => {
            // Simulate real-mode interrupt
            // BL = interrupt number, ES:EDI = pointer to RealModeCallStruct
            let int_num = regs.bl();
            let struct_addr = resolve_ptr(regs.es, regs.edi);
            let rmcs = struct_addr as *mut DpmiRealModeCallStruct;

            // Copy the RMCS into a VMRegisters for our existing handlers
            let mut vm = unsafe {
                let s = &*rmcs;
                VMRegisters {
                    eax: s.eax,
                    ebx: s.ebx,
                    ecx: s.ecx,
                    edx: s.edx,
                    esi: s.esi,
                    edi: s.edi,
                    ebp: s.ebp,
                    eip: s.ip as u32,
                    cs: s.cs as u32,
                    eflags: s.flags as u32,
                    esp: s.sp as u32,
                    ss: s.ss as u32,
                    es: s.es as u32,
                    ds: s.ds as u32,
                    fs: s.fs as u32,
                    gs: s.gs as u32,
                }
            };

            // Temporarily clear DPMI_ACTIVE so handlers use real-mode
            // pointer resolution (seg << 4 + offset)
            unsafe {
                DPMI_ACTIVE = false;
            }
            handle_interrupt(int_num, &mut vm);
            unsafe {
                DPMI_ACTIVE = true;
            }

            // Copy results back to the RMCS
            unsafe {
                let s = &mut *rmcs;
                s.eax = vm.eax;
                s.ebx = vm.ebx;
                s.ecx = vm.ecx;
                s.edx = vm.edx;
                s.esi = vm.esi;
                s.edi = vm.edi;
                s.ebp = vm.ebp;
                s.flags = vm.eflags as u16;
                s.es = vm.es as u16;
                s.ds = vm.ds as u16;
                s.fs = vm.fs as u16;
                s.gs = vm.gs as u16;
            }

            regs.eflags &= !1;
        }

        _ => {
            dpmi_log_unsupported(ax);
            regs.eflags |= 1;
        }
    }
}

/// DPMI Real Mode Call Structure (50 bytes).
/// Layout matches the DPMI 0.9 spec for INT 31h AX=0300h.
#[repr(C, packed)]
struct DpmiRealModeCallStruct {
    edi: u32,
    esi: u32,
    ebp: u32,
    _reserved: u32,
    ebx: u32,
    edx: u32,
    ecx: u32,
    eax: u32,
    flags: u16,
    es: u16,
    ds: u16,
    fs: u16,
    gs: u16,
    ip: u16,
    cs: u16,
    sp: u16,
    ss: u16,
}

// ---- Conventional memory arena allocator ----

/// Allocate `paras` paragraphs from the conventional memory arena.
/// Returns the segment of the allocated block, or None.
fn dos_arena_alloc(paras: u16) -> Option<u16> {
    unsafe {
        // Find the lowest free address by scanning existing blocks
        let mut cursor = DOS_ARENA_START;
        // Sort blocks by segment to find gaps (simple approach: find first fit)
        // Collect occupied regions
        let mut occupied = [(0u16, 0u16); DOS_ARENA_MAX_BLOCKS];
        let mut n_occupied = 0;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 != 0 {
                occupied[n_occupied] = DOS_ARENA[i];
                n_occupied += 1;
            }
        }
        // Simple insertion sort by segment
        for i in 1..n_occupied {
            let key = occupied[i];
            let mut j = i;
            while j > 0 && occupied[j - 1].0 > key.0 {
                occupied[j] = occupied[j - 1];
                j -= 1;
            }
            occupied[j] = key;
        }
        // First-fit: walk through sorted blocks, look for gap
        cursor = DOS_ARENA_START;
        for i in 0..n_occupied {
            let blk_start = occupied[i].0;
            let blk_end = blk_start + occupied[i].1;
            if cursor + paras <= blk_start {
                // Found a gap before this block
                break;
            }
            if blk_end > cursor {
                cursor = blk_end;
            }
        }
        // Check if there's room before DOS_MEM_TOP
        if (cursor as u32 + paras as u32) > DOS_MEM_TOP_SEGMENT as u32 {
            return None;
        }
        // Find a free slot in the arena table
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == 0 {
                DOS_ARENA[i] = (cursor, paras);
                return Some(cursor);
            }
        }
        None // table full
    }
}

/// Free a conventional memory block by segment.
fn dos_arena_free(segment: u16) -> bool {
    unsafe {
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == segment {
                DOS_ARENA[i] = (0, 0);
                return true;
            }
        }
        false
    }
}

/// Resize a conventional memory block. Only grows/shrinks in place.
fn dos_arena_resize(segment: u16, new_paras: u16) -> bool {
    unsafe {
        // Find the block
        let mut idx = DOS_ARENA_MAX_BLOCKS;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == segment {
                idx = i;
                break;
            }
        }
        if idx == DOS_ARENA_MAX_BLOCKS {
            return false;
        }
        let blk_end = segment as u32 + new_paras as u32;
        if blk_end > DOS_MEM_TOP_SEGMENT as u32 {
            return false;
        }
        // Check no other block overlaps the new range
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if i == idx || DOS_ARENA[i].0 == 0 {
                continue;
            }
            let other_start = DOS_ARENA[i].0 as u32;
            let other_end = other_start + DOS_ARENA[i].1 as u32;
            // Overlap check
            if (segment as u32) < other_end && blk_end > other_start {
                return false;
            }
        }
        DOS_ARENA[idx].1 = new_paras;
        true
    }
}

/// Return the largest free contiguous block in paragraphs.
fn dos_arena_largest() -> u16 {
    unsafe {
        let mut occupied = [(0u16, 0u16); DOS_ARENA_MAX_BLOCKS];
        let mut n = 0;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 != 0 {
                occupied[n] = DOS_ARENA[i];
                n += 1;
            }
        }
        // Sort by segment
        for i in 1..n {
            let key = occupied[i];
            let mut j = i;
            while j > 0 && occupied[j - 1].0 > key.0 {
                occupied[j] = occupied[j - 1];
                j -= 1;
            }
            occupied[j] = key;
        }
        let mut cursor = DOS_ARENA_START;
        let mut largest: u16 = 0;
        for i in 0..n {
            let gap = occupied[i].0.saturating_sub(cursor);
            if gap > largest {
                largest = gap;
            }
            let end = occupied[i].0 + occupied[i].1;
            if end > cursor {
                cursor = end;
            }
        }
        // Gap after last block
        let gap = DOS_MEM_TOP_SEGMENT.saturating_sub(cursor);
        if gap > largest {
            largest = gap;
        }
        largest
    }
}

// ---- High memory block tracker ----

/// Record a high-memory allocation. Returns a handle (1-based index) or None.
fn dpmi_high_mem_record(addr: u32, size: u32) -> Option<u32> {
    unsafe {
        for i in 0..DPMI_HIGH_MEM_MAX {
            if DPMI_HIGH_MEM[i].0 == 0 {
                DPMI_HIGH_MEM[i] = (addr, size);
                return Some((i + 1) as u32); // 1-based handle
            }
        }
        None
    }
}

/// Free a high-memory block by handle. Unmaps the memory.
fn dpmi_high_mem_free(handle: u32) -> bool {
    if handle == 0 {
        return false;
    }
    let idx = (handle - 1) as usize;
    unsafe {
        if idx >= DPMI_HIGH_MEM_MAX || DPMI_HIGH_MEM[idx].0 == 0 {
            return false;
        }
        let (addr, size) = DPMI_HIGH_MEM[idx];
        let _ = idos_api::syscall::memory::unmap_memory(addr, size);
        DPMI_HIGH_MEM[idx] = (0, 0);
        true
    }
}

/// Log an unsupported DPMI INT 31h function.
fn dpmi_log_unsupported(ax: u16) {
    let hi = (ax >> 8) as u8;
    let lo = (ax & 0xff) as u8;
    let hex = b"0123456789ABCDEF";
    let mut buf = [0u8; 32];
    let prefix = b"DPMI INT 31 AX=";
    let mut i = 0;
    for &b in prefix {
        buf[i] = b;
        i += 1;
    }
    buf[i] = hex[(hi >> 4) as usize];
    i += 1;
    buf[i] = hex[(hi & 0xf) as usize];
    i += 1;
    buf[i] = hex[(lo >> 4) as usize];
    i += 1;
    buf[i] = hex[(lo & 0xf) as usize];
    i += 1;
    buf[i] = b'\n';
    i += 1;
    dos_log(&buf[..i]);
}

/// BIOS video services (INT 10h)
fn bios_video(regs: &mut VMRegisters) {
    let stdout = idos_api::io::handle::Handle::new(1);
    match regs.ah() {
        0x00 => {
            // AH=00: Set video mode
            let mode = regs.al() & 0x7F; // bit 7 = don't clear screen
            unsafe {
                VGA_MODE = mode;
            }
            match mode {
                0x03 => {
                    // 80x25 text mode — exit graphics if active
                    unsafe {
                        if GFX_BUFFER_PADDR != 0 {
                            exit_graphics_mode();
                        }
                    }
                    update_bda_for_mode(0x03);
                    let _ = write_sync(stdout, b"\x1B[2J\x1B[H", 0);
                }
                0x13 => {
                    // 320x200x256 (mode 13h)
                    enter_graphics_mode(320, 200, 8);
                    update_bda_for_mode(0x13);
                }
                _ => {}
            }
        }
        0x02 => {
            // AH=02: Set cursor position
            // BH=page, DH=row (0-based), DL=col (0-based)
            let row = regs.dh() as u32 + 1; // ANSI is 1-based
            let col = regs.dl() as u32 + 1;
            let mut buf = [0u8; 16];
            let len = write_ansi_cursor(&mut buf, row, col);
            let _ = write_sync(stdout, &buf[..len], 0);
        }
        0x06 => {
            // AH=06: Scroll window up
            // AL=lines (0=clear), BH=attribute, CH/CL=top-left, DH/DL=bottom-right
            if regs.al() == 0 {
                let _ = write_sync(stdout, b"\x1B[2J\x1B[H", 0);
            }
        }
        0x09 => {
            // AH=09: Write character and attribute at cursor
            // AL=char, BH=page, BL=attribute, CX=count
            let ch = regs.al();
            let count = (regs.ecx & 0xffff) as usize;
            let buf = [ch];
            for _ in 0..count {
                let _ = write_sync(stdout, &buf, 0);
            }
        }
        0x0E => {
            // AH=0E: Teletype output — write character, advance cursor
            let ch = regs.al();
            let buf = [ch];
            let _ = write_sync(stdout, &buf, 0);
        }
        0x0F => {
            // AH=0F: Get video mode
            let mode = unsafe { VGA_MODE };
            let cols: u8 = if mode == 0x13 { 40 } else { 80 };
            regs.set_ah(cols);
            regs.set_al(mode);
            regs.ebx = regs.ebx & 0xffff00ff; // BH=0 (page 0)
        }
        0x10 => {
            // AH=10: Palette / DAC functions
            bios_palette(regs);
        }
        0x12 => {
            // AH=12h: Alternate select — VGA capability queries
            let bl = regs.bl();
            match bl {
                0x10 => {
                    // BL=10h: Get EGA info — return VGA-compatible values
                    regs.ebx = (regs.ebx & 0xffff0000) | 0x0003; // BH=0 (color), BL=3 (256K)
                    regs.ecx = (regs.ecx & 0xffff0000) | 0x0009; // CH=0 (feature bits), CL=9 (EGA switches)
                }
                0x30 => {
                    // BL=30h: Set vertical resolution / text rows
                    // AL=12h means success (VGA present)
                    regs.set_al(0x12);
                }
                _ => {
                    // Return AL=12h to indicate VGA is present
                    regs.set_al(0x12);
                }
            }
        }
        0x1A => {
            // AH=1Ah: Display combination code
            // AL=00: Get display combination
            if regs.al() == 0x00 {
                // AL=1Ah means function supported (VGA)
                regs.set_al(0x1A);
                // BL=active display code: 08h = VGA color
                // BH=inactive display: 00h = none
                regs.ebx = (regs.ebx & 0xffff0000) | 0x0008;
            }
        }
        0x4F => {
            // AH=4Fh: VESA BIOS Extensions — not supported.
            // VBE convention: AX=004Fh means supported, anything else = not supported.
            regs.set_ax(0x0100); // AH=01 (failed), AL=00 (not supported)
        }
        0x5F | 0x6F => {
            // AH=5Fh/6Fh: Vendor SVGA extensions — not supported.
            regs.set_ax(0x0000);
        }
        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT 10h AH=", regs.ah(), &mut buf);
            dos_log(&buf[..len]);
        }
    }
}

/// INT 10h AH=10h — VGA palette/DAC subfunctions
fn bios_palette(regs: &mut VMRegisters) {
    match regs.al() {
        0x10 => {
            // AL=10h: Set individual DAC register
            // BX=register number, DH=red, CH=green, CL=blue (6-bit VGA values)
            let idx = (regs.ebx & 0xffff) as usize;
            if idx < 256 {
                unsafe {
                    // VGA DAC values are 6-bit (0-63), scale to 8-bit
                    VGA_PALETTE[idx * 3] = (regs.dh() as u16 * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 1] = (regs.ch() as u16 * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 2] = (regs.cl() as u16 * 255 / 63) as u8;
                }
                push_idos_palette();
            }
        }
        0x12 => {
            // AL=12h: Set block of DAC registers
            // BX=first register, CX=count, ES:DX=table of R,G,B bytes (6-bit)
            let first = (regs.ebx & 0xffff) as usize;
            let count = (regs.ecx & 0xffff) as usize;
            let dx = regs.edx & 0xffff;
            let table_addr = (regs.es << 4) + dx;
            let table = table_addr as *const u8;
            for i in 0..count {
                let idx = first + i;
                if idx >= 256 {
                    break;
                }
                unsafe {
                    let r = core::ptr::read_volatile(table.add(i * 3)) as u16;
                    let g = core::ptr::read_volatile(table.add(i * 3 + 1)) as u16;
                    let b = core::ptr::read_volatile(table.add(i * 3 + 2)) as u16;
                    VGA_PALETTE[idx * 3] = (r * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 1] = (g * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 2] = (b * 255 / 63) as u8;
                }
            }
            push_idos_palette();
        }
        0x15 => {
            // AL=15h: Read individual DAC register
            // BX=register number → DH=red, CH=green, CL=blue (6-bit)
            let idx = (regs.ebx & 0xffff) as usize;
            if idx < 256 {
                unsafe {
                    let r = (VGA_PALETTE[idx * 3] as u16 * 63 / 255) as u8;
                    let g = (VGA_PALETTE[idx * 3 + 1] as u16 * 63 / 255) as u8;
                    let b = (VGA_PALETTE[idx * 3 + 2] as u16 * 63 / 255) as u8;
                    regs.edx = (regs.edx & 0xffff00ff) | ((r as u32) << 8);
                    regs.ecx = (regs.ecx & 0xffff0000) | ((g as u32) << 8) | b as u32;
                }
            }
        }
        0x17 => {
            // AL=17h: Read block of DAC registers
            // BX=first register, CX=count, ES:DX=buffer for R,G,B (6-bit)
            let first = (regs.ebx & 0xffff) as usize;
            let count = (regs.ecx & 0xffff) as usize;
            let dx = regs.edx & 0xffff;
            let table_addr = (regs.es << 4) + dx;
            let table = table_addr as *mut u8;
            for i in 0..count {
                let idx = first + i;
                if idx >= 256 {
                    break;
                }
                unsafe {
                    let r = (VGA_PALETTE[idx * 3] as u16 * 63 / 255) as u8;
                    let g = (VGA_PALETTE[idx * 3 + 1] as u16 * 63 / 255) as u8;
                    let b = (VGA_PALETTE[idx * 3 + 2] as u16 * 63 / 255) as u8;
                    core::ptr::write_volatile(table.add(i * 3), r);
                    core::ptr::write_volatile(table.add(i * 3 + 1), g);
                    core::ptr::write_volatile(table.add(i * 3 + 2), b);
                }
            }
        }
        _ => {}
    }
}

/// Write ANSI cursor positioning escape sequence into buf. Returns bytes written.
fn write_ansi_cursor(buf: &mut [u8; 16], row: u32, col: u32) -> usize {
    // Format: ESC [ row ; col H
    buf[0] = 0x1B;
    buf[1] = b'[';
    let mut pos = 2;
    pos += write_u32(&mut buf[pos..], row);
    buf[pos] = b';';
    pos += 1;
    pos += write_u32(&mut buf[pos..], col);
    buf[pos] = b'H';
    pos + 1
}

/// Write a u32 as decimal digits into a byte slice. Returns number of bytes written.
fn write_u32(buf: &mut [u8], mut val: u32) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut len = 0;
    while val > 0 {
        tmp[len] = b'0' + (val % 10) as u8;
        val /= 10;
        len += 1;
    }
    for i in 0..len {
        buf[i] = tmp[len - 1 - i];
    }
    len
}

fn dos_api(vm_regs: &mut VMRegisters) {
    match vm_regs.ah() {
        0x00 => terminate(vm_regs),
        0x01 => read_stdin_with_echo(vm_regs),
        0x02 => output_char_to_stdout(vm_regs),
        0x04 => write_char_stdaux(vm_regs),
        0x09 => print_string(vm_regs),
        0x0E => set_current_drive(vm_regs),
        0x19 => get_current_drive(vm_regs),
        0x25 => set_interrupt_vector(vm_regs),
        0x2A => get_date(vm_regs),
        0x2C => get_time(vm_regs),
        0x30 => get_dos_version(vm_regs),
        0x35 => get_interrupt_vector(vm_regs),
        0x36 => get_disk_free_space(vm_regs),
        0x3C => create_file(vm_regs),
        0x3D => open_file(vm_regs),
        0x3E => close_file(vm_regs),
        0x3F => read_file(vm_regs),
        0x40 => write_file(vm_regs),
        0x42 => seek_file(vm_regs),
        0x41 => delete_file(vm_regs),
        0x44 => ioctl(vm_regs),
        0x47 => get_current_directory(vm_regs),
        0x48 => allocate_memory(vm_regs),
        0x49 => free_memory(vm_regs),
        0x4A => resize_memory(vm_regs),
        0x4C => terminate_with_code(vm_regs),
        0x56 => rename_file(vm_regs),
        0x63 => get_dbcs_table(vm_regs),
        0x66 => get_global_code_page(vm_regs),
        0x68 => commit_file(vm_regs),
        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT 21h AH=", vm_regs.ah(), &mut buf);
            dos_log(&buf[..len]);
            vm_regs.eflags |= 1;
        }
    }
}

/// AH=0x00 - Terminate the current program
/// Restores the interrupt vectors 0x22, 0x23, 0x24. Frees memory allocated to
/// the current program, but does not close FCBs.
/// Input:
///     CS points to the PSP
/// Output:
///     If a termination vector exists, set CS and IP to that vector
pub fn terminate(regs: &mut VMRegisters) {
    // TODO: Check PSP for parent segment
    // if has parent segment
    //   set cs to termination vector segment
    //   set eip to termination vector offset

    exit(1);
}

/// AH=0x01 - Read from STDIN and echo to STDOUT
/// Input:
///     None
/// Output:
///     AL = character from STDIN
pub fn read_stdin_with_echo(regs: &mut VMRegisters) {
    let stdin = idos_api::io::handle::Handle::new(0);
    let stdout = idos_api::io::handle::Handle::new(1);

    let mut buffer: [u8; 1] = [0; 1];

    match idos_api::io::sync::read_sync(stdin, &mut buffer, 0) {
        Ok(len) if len == 1 => {
            let _ = idos_api::io::sync::write_sync(stdout, &mut buffer, 0);
        }
        _ => (),
    }

    regs.set_al(buffer[0]);
}

/// AH=0x02 - Output single character to STDOUT
/// Input:
///     DL = character to output
/// Output:
///     None
pub fn output_char_to_stdout(regs: &mut VMRegisters) {
    let char = regs.dl();
    let buffer: [u8; 1] = [char];
    let stdout = idos_api::io::handle::Handle::new(1);
    let _ = idos_api::io::sync::write_sync(stdout, &buffer, 0);
}

/// AH=0x03 - Blocking character read from STDAUX (COM)
/// Input:
///     None
/// Output:
///     AL = character from STDAUX
pub fn read_char_stdaux(_regs: &mut VMRegisters) {}

/// AH=0x04 - Write character to STDAUX
/// Input:
///     DL = character to output
/// Output:
///     None
pub fn write_char_stdaux(regs: &mut VMRegisters) {
    let char = regs.dl();
    let buffer: [u8; 1] = [char];
    let stdaux = idos_api::io::handle::Handle::new(2);
    let _ = idos_api::io::sync::write_sync(stdaux, &buffer, 0);
}

/// AH=0x09 - Print a dollar-terminated string to STDOUT
/// Input:
///     DS:DX points to the string
/// Output:
///     None
pub fn print_string(regs: &mut VMRegisters) {
    let start_address = resolve_ptr(regs.ds, regs.edx);
    let start_ptr = start_address as *const u8;
    let search_len = 256usize;
    let mut string_len = 0;
    while string_len < search_len {
        unsafe {
            if core::ptr::read_volatile(start_ptr.add(string_len)) == b'$' {
                break;
            }
        }
        string_len += 1;
    }
    let string_slice = unsafe { core::slice::from_raw_parts(start_ptr, string_len) };
    let stdout = idos_api::io::handle::Handle::new(1);
    let _ = idos_api::io::sync::write_sync(stdout, string_slice, 0);
}

/// AH=0x25 - Set interrupt vector
/// Input: AL=interrupt number, DS:DX=new handler address
fn set_interrupt_vector(regs: &mut VMRegisters) {
    let int_num = regs.al() as u32;
    let offset = regs.edx & 0xffff;
    let segment = regs.ds;
    // Write to the IVT at address 0000:(int_num * 4)
    let ivt_addr = (int_num * 4) as *mut u16;
    unsafe {
        core::ptr::write_volatile(ivt_addr, offset as u16);
        core::ptr::write_volatile(ivt_addr.add(1), segment as u16);
    }

    // If the program is hooking a hardware interrupt, record it in the IRQ mask.
    // INT 8-15 map to IRQ 0-7, INT 70-77 map to IRQ 8-15.
    let irq = match int_num {
        0x08..=0x0F => Some(int_num - 0x08),
        0x70..=0x77 => Some(int_num - 0x70 + 8),
        // INT 1Ch is the user timer hook, chained from INT 8 (IRQ 0)
        0x1C => Some(0),
        _ => None,
    };
    if let Some(irq_num) = irq {
        unsafe { VM86_IRQ_MASK |= 1 << irq_num; }
    }
}

/// AH=0x2A - Get system date
/// Output: CX=year, DH=month, DL=day, AL=day of week
fn get_date(regs: &mut VMRegisters) {
    let ts = idos_api::syscall::time::get_system_time();
    let dt = idos_api::time::DateTime::from_timestamp(ts);
    regs.set_cx(dt.date.year);
    regs.set_ah(0); // preserve AH=2A? No, DOS returns AL=day of week
    regs.edx = (regs.edx & 0xffff0000) | ((dt.date.month as u32) << 8) | dt.date.day as u32;
    // Day of week: 0=Sunday. Simple approximation using Zeller-like formula.
    let y = dt.date.year as u32;
    let m = dt.date.month as u32;
    let d = dt.date.day as u32;
    let (y2, m2) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let dow = (d + 13 * (m2 + 1) / 5 + y2 + y2 / 4 - y2 / 100 + y2 / 400) % 7;
    // Zeller gives 0=Saturday, convert to DOS 0=Sunday
    let dos_dow = if dow == 0 { 6 } else { dow - 1 };
    regs.set_al(dos_dow as u8);
}

/// AH=0x2C - Get system time
/// Output: CH=hour, CL=minutes, DH=seconds, DL=hundredths
fn get_time(regs: &mut VMRegisters) {
    let ts = idos_api::syscall::time::get_system_time();
    let dt = idos_api::time::DateTime::from_timestamp(ts);
    let mono_ms = idos_api::syscall::time::get_monotonic_ms();
    let hundredths = ((mono_ms / 10) % 100) as u32;
    regs.ecx = (regs.ecx & 0xffff0000) | ((dt.time.hours as u32) << 8) | dt.time.minutes as u32;
    regs.edx = (regs.edx & 0xffff0000) | ((dt.time.seconds as u32) << 8) | hundredths;
}

/// AH=0x0E - Set current drive
/// Input: DL=drive number (0=A, 1=B, ...)
/// Output: AL=number of drives
fn set_current_drive(regs: &mut VMRegisters) {
    // We only support one drive, so this is mostly a no-op.
    // The drive letter in the CWD stays as initialized.
    regs.set_al(1); // 1 logical drive
}

/// AH=0x19 - Get current drive
/// Output: AL=current drive (0=A, 1=B, ...)
fn get_current_drive(regs: &mut VMRegisters) {
    let drive_letter = unsafe {
        if DOS_CWD_LEN > 0 {
            DOS_CWD[0]
        } else {
            b'A'
        }
    };
    let drive_num = if drive_letter >= b'A' && drive_letter <= b'Z' {
        drive_letter - b'A'
    } else {
        0
    };
    regs.set_al(drive_num);
}

/// AH=0x30 - Get DOS version
/// Returns AL=major, AH=minor, BH=OEM ID, BL:CX=serial
fn get_dos_version(regs: &mut VMRegisters) {
    regs.set_al(5); // DOS 5.0
    regs.set_ah(0);
    regs.ebx = 0; // OEM=IBM, serial=0
    regs.ecx = 0;
}

/// AH=0x35 - Get interrupt vector
/// Input: AL=interrupt number
/// Output: ES:BX=current handler address
fn get_interrupt_vector(regs: &mut VMRegisters) {
    let int_num = regs.al() as u32;
    let ivt_addr = (int_num * 4) as *const u16;
    unsafe {
        let offset = core::ptr::read_volatile(ivt_addr) as u32;
        let segment = core::ptr::read_volatile(ivt_addr.add(1)) as u32;
        regs.ebx = offset;
        regs.es = segment;
    }
}

/// AH=0x36 - Get disk free space
/// Input: DL=drive (0=default, 1=A, ...)
/// Output: AX=sectors/cluster, BX=free clusters, CX=bytes/sector, DX=total clusters
fn get_disk_free_space(regs: &mut VMRegisters) {
    // Return plausible values for a FAT12 floppy
    regs.set_ax(1); // 1 sector per cluster
    regs.ebx = (regs.ebx & 0xffff0000) | 100; // 100 free clusters
    regs.set_cx(512); // 512 bytes per sector
    regs.set_dx(2880); // 2880 total clusters (1.44MB floppy)
}

/// AH=0x47 - Get current directory
/// Input: DL=drive (0=default, 1=A, ...), DS:SI=64-byte buffer
/// Output: CF=0 on success, buffer filled with path (no drive, no leading \)
fn get_current_directory(regs: &mut VMRegisters) {
    let buf_addr = resolve_ptr(regs.ds, regs.esi);
    let buf = buf_addr as *mut u8;

    unsafe {
        // CWD is like "A:\" — skip the "X:\" prefix to get the subdirectory part
        let skip = if DOS_CWD_LEN >= 3 && DOS_CWD[1] == b':' && DOS_CWD[2] == b'\\' {
            3
        } else {
            0
        };
        let dir_len = if DOS_CWD_LEN > skip {
            DOS_CWD_LEN - skip
        } else {
            0
        };
        // Strip trailing backslash if present
        let copy_len = if dir_len > 0 && DOS_CWD[skip + dir_len - 1] == b'\\' {
            dir_len - 1
        } else {
            dir_len
        };
        for i in 0..copy_len.min(63) {
            core::ptr::write_volatile(buf.add(i), DOS_CWD[skip + i]);
        }
        core::ptr::write_volatile(buf.add(copy_len.min(63)), 0); // NUL terminate
    }

    regs.eflags &= !1; // clear CF
}

/// Read a NUL-terminated DOS path from v86 memory at DS:DX.
/// Returns the path as a byte slice (up to 128 bytes).
fn read_dos_path(regs: &VMRegisters) -> &'static [u8] {
    let addr = resolve_ptr(regs.ds, regs.edx);
    let ptr = addr as *const u8;
    let mut len = 0;
    while len < 128 {
        let b = unsafe { core::ptr::read_volatile(ptr.add(len)) };
        if b == 0 {
            break;
        }
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(ptr, len) }
}

/// Open a file via IDOS, allocating a DOS fd. Used by both 3C and 3D.
fn dos_open_common(regs: &mut VMRegisters, create: bool) {
    let dos_fd = match alloc_dos_fd() {
        Some(fd) => fd,
        None => {
            regs.eflags |= 1;
            regs.set_ax(0x04); // too many open files
            return;
        }
    };

    let path_bytes = read_dos_path(regs);
    let mut resolved = [0u8; 256];
    let resolved_len = resolve_dos_path(path_bytes, &mut resolved);
    let path_str = unsafe { core::str::from_utf8_unchecked(&resolved[..resolved_len]) };

    let idos_handle = create_file_handle();
    let flags = if create { OPEN_FLAG_CREATE } else { 0 };
    match open_sync(idos_handle, path_str, flags) {
        Ok(_) => {
            unsafe {
                DOS_FDS[dos_fd as usize] = DosFileDescriptor {
                    handle: idos_handle,
                    cursor: 0,
                    flags: FD_OPEN,
                };
            }
            regs.set_ax(dos_fd);
            regs.eflags &= !1; // clear CF
        }
        Err(_) => {
            regs.eflags |= 1; // set CF
            regs.set_ax(0x02); // file not found
        }
    }
}

/// AH=0x3C - Create file
/// Input: CX=attributes, DS:DX=ASCIIZ filename
/// Output: CF=0 AX=handle on success, CF=1 AX=error on failure
fn create_file(regs: &mut VMRegisters) {
    dos_open_common(regs, true);
}

/// AH=0x3D - Open file
/// Input: AL=access mode, DS:DX=ASCIIZ filename
/// Output: CF=0 AX=handle on success, CF=1 AX=error on failure
fn open_file(regs: &mut VMRegisters) {
    dos_open_common(regs, false);
}

/// AH=0x3E - Close file
/// Input: BX=file handle
/// Output: CF=0 on success
fn close_file(regs: &mut VMRegisters) {
    let dos_handle = (regs.ebx & 0xffff) as u16;
    match get_dos_fd(dos_handle) {
        Some(fd) => {
            if !fd.is_device() {
                let _ = close_sync(fd.handle);
            }
            unsafe {
                DOS_FDS[dos_handle as usize].flags = 0;
            }
            regs.eflags &= !1;
        }
        None => {
            regs.eflags |= 1;
            regs.set_ax(0x06); // invalid handle
        }
    }
}

/// AH=0x3F - Read from file or device
/// Input: BX=file handle, CX=byte count, DS:DX=buffer
/// Output: CF=0 AX=bytes read on success
fn read_file(regs: &mut VMRegisters) {
    let dos_handle = (regs.ebx & 0xffff) as u16;
    let count = if unsafe { DPMI_ACTIVE } {
        regs.ecx as usize
    } else {
        (regs.ecx & 0xffff) as usize
    };
    let buffer_addr = resolve_ptr(regs.ds, regs.edx);
    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_addr as *mut u8, count) };

    // When reading from stdin, flush graphics and temporarily enable
    // canonical mode so the console buffers a full line.
    let is_stdin = dos_handle == 0;
    if is_stdin {
        set_stdin_canonical(true);
    }

    match get_dos_fd_mut(dos_handle) {
        Some(fd) => {
            match read_sync(fd.handle, buffer, fd.cursor) {
                Ok(bytes_read) => {
                    if !fd.is_device() {
                        fd.cursor += bytes_read;
                    }
                    regs.set_ax(bytes_read as u16);
                    regs.eflags &= !1;
                }
                Err(_) => {
                    regs.set_ax(0);
                    regs.eflags &= !1; // DOS returns 0 bytes on EOF, not error
                }
            }
        }
        None => {
            regs.eflags |= 1;
            regs.set_ax(0x06); // invalid handle
        }
    }

    if is_stdin {
        set_stdin_canonical(false);
    }
}

/// AH=0x40 - Write to file or device
/// Input: BX=file handle, CX=byte count, DS:DX=buffer
/// Output: CF=0 AX=bytes written on success
fn write_file(regs: &mut VMRegisters) {
    let dos_handle = (regs.ebx & 0xffff) as u16;
    let count = if unsafe { DPMI_ACTIVE } {
        regs.ecx as usize
    } else {
        (regs.ecx & 0xffff) as usize
    };
    let buffer_addr = resolve_ptr(regs.ds, regs.edx);
    let buffer = unsafe { core::slice::from_raw_parts(buffer_addr as *const u8, count) };

    match get_dos_fd_mut(dos_handle) {
        Some(fd) => {
            match write_sync(fd.handle, buffer, fd.cursor) {
                Ok(bytes_written) => {
                    if !fd.is_device() {
                        fd.cursor += bytes_written;
                    }
                    regs.set_ax(bytes_written as u16);
                    regs.eflags &= !1;
                }
                Err(_) => {
                    regs.eflags |= 1;
                    regs.set_ax(0x05); // access denied
                }
            }
        }
        None => {
            regs.eflags |= 1;
            regs.set_ax(0x06); // invalid handle
        }
    }
}

/// AH=0x42 - Seek (LSEEK)
/// Input: AL=origin (0=SET,1=CUR,2=END), BX=handle, CX:DX=offset
/// Output: CF=0 DX:AX=new position on success
fn seek_file(regs: &mut VMRegisters) {
    let dos_handle = (regs.ebx & 0xffff) as u16;
    let origin = regs.al();
    let offset = ((regs.ecx & 0xffff) << 16 | (regs.edx & 0xffff)) as i32;

    match get_dos_fd_mut(dos_handle) {
        Some(fd) => {
            if fd.is_device() {
                // Devices: return position 0
                regs.set_ax(0);
                regs.edx = regs.edx & 0xffff0000;
                regs.eflags &= !1;
                return;
            }

            let new_pos: i64 = match origin {
                0 => offset as i64,                    // SEEK_SET
                1 => fd.cursor as i64 + offset as i64, // SEEK_CUR
                2 => {
                    // SEEK_END: need file size
                    let mut file_status = FileStatus::new();
                    let _ = io_sync(
                        fd.handle,
                        FILE_OP_STAT,
                        &mut file_status as *mut FileStatus as u32,
                        core::mem::size_of::<FileStatus>() as u32,
                        0,
                    );
                    file_status.byte_size as i64 + offset as i64
                }
                _ => {
                    regs.eflags |= 1;
                    regs.set_ax(0x01); // invalid function
                    return;
                }
            };

            if new_pos < 0 {
                regs.eflags |= 1;
                regs.set_ax(0x19); // seek error
                return;
            }

            fd.cursor = new_pos as u32;
            regs.set_ax(fd.cursor as u16);
            regs.edx = (regs.edx & 0xffff0000) | (fd.cursor >> 16);
            regs.eflags &= !1;
        }
        None => {
            regs.eflags |= 1;
            regs.set_ax(0x06); // invalid handle
        }
    }
}

/// AH=0x44 - IOCTL
/// AL=subfunction, BX=handle
fn ioctl(regs: &mut VMRegisters) {
    let subfunc = regs.al();
    match subfunc {
        0x00 => {
            // Get device information
            let dos_handle = (regs.ebx & 0xffff) as u16;
            let info: u16 = match get_dos_fd(dos_handle) {
                Some(fd) if fd.is_device() => 0x80D3, // device flags
                Some(_) => 0x0000,                    // disk file
                None => {
                    regs.eflags |= 1;
                    regs.set_ax(0x06);
                    return;
                }
            };
            regs.edx = (regs.edx & 0xffff0000) | info as u32;
            regs.eflags &= !1;
        }
        _ => {
            regs.eflags |= 1;
            regs.set_ax(0x01); // invalid function
        }
    }
}

/// AH=0x41 - Delete file
/// Input: DS:DX=ASCIIZ filename
/// Output: CF=0 on success, CF=1 AX=error on failure
fn delete_file(regs: &mut VMRegisters) {
    let path_bytes = read_dos_path(regs);
    let mut resolved = [0u8; 256];
    let resolved_len = resolve_dos_path(path_bytes, &mut resolved);
    let path_str = unsafe { core::str::from_utf8_unchecked(&resolved[..resolved_len]) };

    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_UNLINK,
        path_str.as_ptr() as u32,
        path_str.len() as u32,
        0,
    );
    let _ = close_sync(handle);
    match result {
        Ok(_) => {
            regs.eflags &= !1;
        }
        Err(_) => {
            regs.eflags |= 1;
            regs.set_ax(0x02); // file not found
        }
    }
}

/// AH=0x48 - Allocate memory block
/// Input: BX=paragraphs requested
/// Output: CF=0 AX=segment on success, CF=1 BX=max available on failure
fn allocate_memory(regs: &mut VMRegisters) {
    let paras = (regs.ebx & 0xffff) as u16;
    match dos_arena_alloc(paras) {
        Some(segment) => {
            regs.set_ax(segment);
            regs.eflags &= !1;
        }
        None => {
            regs.ebx = (regs.ebx & 0xffff0000) | dos_arena_largest() as u32;
            regs.set_ax(0x08); // insufficient memory
            regs.eflags |= 1;
        }
    }
}

/// AH=0x49 - Free memory block
/// Input: ES=segment of block
/// Output: CF=0 on success
fn free_memory(regs: &mut VMRegisters) {
    let segment = (regs.es & 0xffff) as u16;
    dos_arena_free(segment); // best-effort, always succeed
    regs.eflags &= !1;
}

/// AH=0x4A - Resize memory block
/// Input: BX=new size in paragraphs, ES=segment of block
/// Output: CF=0 on success, CF=1 BX=max available on failure
fn resize_memory(regs: &mut VMRegisters) {
    let new_paras = (regs.ebx & 0xffff) as u16;
    let segment = (regs.es & 0xffff) as u16;
    // Try to resize in the arena. If the block isn't tracked (e.g. the
    // program's initial allocation before we had an arena), just succeed —
    // the memory is already mapped.
    dos_arena_resize(segment, new_paras);
    regs.eflags &= !1;
}

/// AH=0x4C - Terminate with return code
/// Input: AL=return code
fn terminate_with_code(regs: &mut VMRegisters) {
    let code = regs.al() as u32;
    exit(code);
}

/// AH=0x56 - Rename file
/// Input: DS:DX=ASCIIZ old name, ES:DI=ASCIIZ new name
/// Output: CF=0 on success, CF=1 AX=error on failure
fn rename_file(regs: &mut VMRegisters) {
    // Read old name from DS:DX
    let old_path_bytes = read_dos_path(regs);
    let mut old_resolved = [0u8; 256];
    let old_len = resolve_dos_path(old_path_bytes, &mut old_resolved);

    // Read new name from ES:DI
    let new_addr = resolve_ptr(regs.es, regs.edi);
    let new_ptr = new_addr as *const u8;
    let mut new_raw_len = 0;
    while new_raw_len < 128 {
        let b = unsafe { core::ptr::read_volatile(new_ptr.add(new_raw_len)) };
        if b == 0 {
            break;
        }
        new_raw_len += 1;
    }
    let new_path_bytes = unsafe { core::slice::from_raw_parts(new_ptr, new_raw_len) };
    let mut new_resolved = [0u8; 256];
    let new_len = resolve_dos_path(new_path_bytes, &mut new_resolved);

    // IDOS rename: concatenate both paths, pack lengths
    let total_len = old_len + new_len;
    let mut combined = [0u8; 512];
    combined[..old_len].copy_from_slice(&old_resolved[..old_len]);
    combined[old_len..total_len].copy_from_slice(&new_resolved[..new_len]);
    let packed_lens = (old_len as u32) | ((new_len as u32) << 16);

    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_RENAME,
        combined.as_ptr() as u32,
        total_len as u32,
        packed_lens,
    );
    let _ = close_sync(handle);
    match result {
        Ok(_) => {
            regs.eflags &= !1;
        }
        Err(_) => {
            regs.eflags |= 1;
            regs.set_ax(0x02); // file not found
        }
    }
}

/// AH=0x63 - Get DBCS lead byte table
/// Output: DS:SI=pointer to DBCS table
fn get_dbcs_table(regs: &mut VMRegisters) {
    // Return a pointer to a table that's just a terminator (0000).
    // We'll use two zero bytes somewhere safe in the PSP reserved area.
    // PSP offset 0x3C is reserved and we zeroed it, so point there.
    regs.ds = PSP_SEGMENT;
    regs.esi = 0x3C;
}

/// AH=0x66 - Get/set global code page
/// AL=01: get, AL=02: set
fn get_global_code_page(regs: &mut VMRegisters) {
    let subfunc = regs.al();
    match subfunc {
        0x01 => {
            // Get: BX=active code page, DX=system code page
            regs.ebx = (regs.ebx & 0xffff0000) | 437; // US English
            regs.edx = (regs.edx & 0xffff0000) | 437;
            regs.eflags &= !1;
        }
        _ => {
            regs.eflags &= !1; // just succeed
        }
    }
}

/// AH=0x68 - Commit/flush file
/// Input: BX=handle
fn commit_file(regs: &mut VMRegisters) {
    // Stub: always succeed
    regs.eflags &= !1;
}
