//! DOS INT 0x21 handler and related data structures.
//!
//! This module implements the DOS API dispatch (INT 0x21) and all individual
//! function handlers, plus the DOS file descriptor table, PSP setup, CWD
//! tracking, and path resolution.

use crate::dpmi::DPMI_ACTIVE;
use crate::memory::{dos_arena_alloc, dos_arena_free, dos_arena_largest, dos_arena_resize};

use idos_api::{
    compat::VMRegisters,
    io::{
        file::FileStatus,
        sync::{close_sync, io_sync, ioctl_sync, open_sync, read_sync, write_sync},
        Handle, FILE_OP_RENAME, FILE_OP_STAT, FILE_OP_UNLINK, OPEN_FLAG_CREATE,
    },
    syscall::io::create_file_handle,
};

// --- Constants ---

/// DOS segment base address (flat). Segment number is PSP_BASE / 16.
pub(crate) const PSP_BASE: u32 = 0x8000;
/// PSP segment number for 8086 VM registers
pub(crate) const PSP_SEGMENT: u32 = PSP_BASE / 16; // 0x800
/// The program image loads 0x10 paragraphs (256 bytes) past the PSP segment
pub(crate) const PROGRAM_SEGMENT: u32 = PSP_SEGMENT + 0x10;
/// Top of conventional memory available to DOS programs (640KB boundary)
pub(crate) const DOS_MEM_TOP: u32 = 0xA000_0;
/// Top of memory as a segment
pub(crate) const DOS_MEM_TOP_SEGMENT: u16 = (DOS_MEM_TOP / 16) as u16;

const MAX_DOS_FILES: usize = 20;
/// Flag: this descriptor refers to a character device (stdin/stdout/etc.)
const FD_DEVICE: u8 = 0x80;
/// Flag: this descriptor is currently open
const FD_OPEN: u8 = 0x01;

// --- Types ---

/// Program Segment Prefix — the 256-byte header DOS places before every program.
#[repr(C, packed)]
pub(crate) struct Psp {
    /// 0x00: INT 20h instruction (CD 20)
    pub int20: [u8; 2],
    /// 0x02: Top of memory segment
    pub mem_top_segment: u16,
    /// 0x04: Reserved
    pub _reserved1: u8,
    /// 0x05: Far call to DOS dispatcher (5 bytes)
    pub dos_far_call: [u8; 5],
    /// 0x0A: Terminate address (IP:CS)
    pub terminate_vector: u32,
    /// 0x0E: Ctrl-Break handler (IP:CS)
    pub break_vector: u32,
    /// 0x12: Critical error handler (IP:CS)
    pub error_vector: u32,
    /// 0x16: Parent PSP segment
    pub parent_psp: u16,
    /// 0x18: Job File Table (20 entries)
    pub jft: [u8; 20],
    /// 0x2C: Environment segment
    pub env_segment: u16,
    /// 0x2E: SS:SP on last INT 21h
    pub last_stack: u32,
    /// 0x32: JFT size
    pub jft_size: u16,
    /// 0x34: JFT far pointer
    pub jft_pointer: u32,
    /// 0x38: Previous PSP far pointer
    pub prev_psp: u32,
    /// 0x3C: Reserved
    pub _reserved2: [u8; 20],
    /// 0x50: INT 21h / RETF trampoline
    pub int21_retf: [u8; 3],
    /// 0x53: Reserved
    pub _reserved3: [u8; 45],
    /// 0x80: Command tail length
    pub cmdtail_len: u8,
    /// 0x81: Command tail (127 bytes, CR-terminated)
    pub cmdtail: [u8; 127],
}

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

// --- Statics ---

static mut DOS_FDS: [DosFileDescriptor; MAX_DOS_FILES] =
    [DosFileDescriptor::empty(); MAX_DOS_FILES];

/// Current working directory — full IDOS path ending in '\', e.g. "A:\"
static mut DOS_CWD: [u8; 256] = [0; 256];
static mut DOS_CWD_LEN: usize = 0;

// --- File table functions ---

/// Initialize the DOS file descriptor table with the standard handles.
/// Called from compat_start after IDOS handles are opened.
pub(crate) fn init_file_table(stdaux_handle: Handle) {
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

// --- PSP ---

pub(crate) fn setup_psp() {
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

// --- CWD ---

/// Initialize the CWD from the executable's path (strip the filename).
pub(crate) fn init_cwd(exec_path: &[u8]) {
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

/// Read a NUL-terminated DOS path from v86 memory at DS:DX.
/// Returns the path as a byte slice (up to 128 bytes).
fn read_dos_path(regs: &VMRegisters) -> &'static [u8] {
    let addr = super::resolve_ptr(regs.ds, regs.edx);
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

// --- DOS API dispatch ---

/// Main INT 0x21 dispatch.
pub(crate) fn dos_api(vm_regs: &mut VMRegisters) {
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
            let len = super::fmt_unsupported(b"INT 21h AH=", vm_regs.ah(), &mut buf);
            super::dos_log(&buf[..len]);
            vm_regs.eflags |= 1;
        }
    }
}

// --- Individual INT 0x21 handlers ---

/// AH=0x00 - Terminate the current program
/// Restores the interrupt vectors 0x22, 0x23, 0x24. Frees memory allocated to
/// the current program, but does not close FCBs.
/// Input:
///     CS points to the PSP
/// Output:
///     If a termination vector exists, set CS and IP to that vector
fn terminate(_regs: &mut VMRegisters) {
    // TODO: Check PSP for parent segment
    // if has parent segment
    //   set cs to termination vector segment
    //   set eip to termination vector offset

    super::exit(1);
}

/// AH=0x01 - Read from STDIN and echo to STDOUT
/// Input:
///     None
/// Output:
///     AL = character from STDIN
fn read_stdin_with_echo(regs: &mut VMRegisters) {
    let stdin = Handle::new(0);
    let stdout = Handle::new(1);

    let mut buffer: [u8; 1] = [0; 1];

    match read_sync(stdin, &mut buffer, 0) {
        Ok(len) if len == 1 => {
            let _ = write_sync(stdout, &mut buffer, 0);
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
fn output_char_to_stdout(regs: &mut VMRegisters) {
    let char = regs.dl();
    let buffer: [u8; 1] = [char];
    let stdout = Handle::new(1);
    let _ = write_sync(stdout, &buffer, 0);
}

/// AH=0x03 - Blocking character read from STDAUX (COM)
/// Input:
///     None
/// Output:
///     AL = character from STDAUX
fn read_char_stdaux(_regs: &mut VMRegisters) {}

/// AH=0x04 - Write character to STDAUX
/// Input:
///     DL = character to output
/// Output:
///     None
fn write_char_stdaux(regs: &mut VMRegisters) {
    let char = regs.dl();
    let buffer: [u8; 1] = [char];
    let stdaux = Handle::new(2);
    let _ = write_sync(stdaux, &buffer, 0);
}

/// AH=0x09 - Print a dollar-terminated string to STDOUT
/// Input:
///     DS:DX points to the string
/// Output:
///     None
fn print_string(regs: &mut VMRegisters) {
    let start_address = super::resolve_ptr(regs.ds, regs.edx);
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
    let stdout = Handle::new(1);
    let _ = write_sync(stdout, string_slice, 0);
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
        unsafe {
            super::VM86_IRQ_MASK |= 1 << irq_num;
        }
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
    let buf_addr = super::resolve_ptr(regs.ds, regs.esi);
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
    let buffer_addr = super::resolve_ptr(regs.ds, regs.edx);
    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_addr as *mut u8, count) };

    // When reading from stdin, flush graphics and temporarily enable
    // canonical mode so the console buffers a full line.
    let is_stdin = dos_handle == 0;
    if is_stdin {
        super::set_stdin_canonical(true);
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
        super::set_stdin_canonical(false);
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
    let buffer_addr = super::resolve_ptr(regs.ds, regs.edx);
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
    super::exit(code);
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
    let new_addr = super::resolve_ptr(regs.es, regs.edi);
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
