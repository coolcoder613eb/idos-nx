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
    compat::VMRegisters,
    io::{
        file::FileStatus,
        sync::{close_sync, io_sync, ioctl_sync, open_sync, read_sync},
        termios::Termios,
        Handle, FILE_OP_STAT,
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
    magic: [u8; 2],           // 'M','Z' or 'Z','M'
    last_page_bytes: u16,     // bytes used in last 512-byte page
    total_pages: u16,         // total number of 512-byte pages
    relocation_count: u16,    // number of relocation entries
    header_paragraphs: u16,   // header size in 16-byte paragraphs
    min_extra_paragraphs: u16,
    max_extra_paragraphs: u16,
    initial_ss: u16,          // initial SS relative to load segment
    initial_sp: u16,          // initial SP
    checksum: u16,
    initial_ip: u16,          // initial IP
    initial_cs: u16,          // initial CS relative to load segment
    relocation_offset: u16,   // offset of relocation table in file
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

    // Allocate DOS memory at PSP_BASE and load the .COM binary at offset 0x100
    let total_size = 0x100 + file_size; // PSP + program
    let pages = (total_size + 0xfff) / 0x1000;
    let _ = map_memory(Some(PSP_BASE), pages * 0x1000, None);

    read_file_into(file_handle, PSP_BASE + 0x100, file_size, 0);

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

    // Total memory needed: PSP (256 bytes) + image + extra paragraphs for BSS/stack
    let extra_bytes = mz.min_extra_paragraphs as u32 * 16;
    let total_size = 0x100 + file_image_size + extra_bytes;
    let pages = (total_size + 0xfff) / 0x1000;
    let _ = map_memory(Some(PSP_BASE), pages * 0x1000, None);

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

    loop {
        let exit_reason = idos_api::syscall::exec::enter_8086(&mut vm_regs, 0);

        match exit_reason {
            idos_api::compat::VM86_EXIT_GPF => unsafe {
                if !handle_fault(&mut vm_regs) {
                    break;
                }
            },
            idos_api::compat::VM86_EXIT_DEBUG => {
                // Hardware interrupt delivery — TF was set by the kernel
                // Clear TF from the saved eflags so we don't keep trapping
                vm_regs.eflags &= !0x100;
                // TODO: deliver pending virtual interrupts
            }
            _ => break,
        }
    }

    exit(0);
}

fn exit(code: u32) -> ! {
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
    let mut op_ptr = ((vm_regs.cs << 4) + vm_regs.eip) as *const u8;
    // TODO: check prefix
    match *op_ptr {
        0x9c => { // PUSHF
        }
        0x9d => { // POPF
        }
        0xcd => {
            // INT
            let irq = *op_ptr.add(1);
            handle_interrupt(irq, vm_regs);
            vm_regs.eip += 2;
            return true;
        }
        0xcf => { // IRET
        }
        0xf4 => { // HLT
        }
        0xfa => { // CLI
        }
        0xfb => { // STI
        }
        _ => (),
    }

    false
}

fn handle_interrupt(irq: u8, vm_regs: &mut VMRegisters) {
    match irq {
        // So many interrupts to implement here...
        0x21 => {
            // DOS API
            dos_api(vm_regs);
        }

        // TODO: jump to the value in the IVT, or fail if there is no irq
        _ => (),
    }
}

fn dos_api(vm_regs: &mut VMRegisters) {
    match vm_regs.ah() {
        0x00 => terminate(vm_regs),
        0x01 => read_stdin_with_echo(vm_regs),
        0x02 => output_char_to_stdout(vm_regs),
        0x04 => write_char_stdaux(vm_regs),
        0x09 => print_string(vm_regs),
        _ => {
            panic!("Unsupported API")
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
    let dx = regs.edx & 0xffff;
    let start_address = (regs.ds << 4) + dx;
    let start_ptr = start_address as *const u8;
    let search_len = 256.min(0x10000 - dx) as usize;
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
