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

pub mod bios;
pub mod dos;
pub mod dpmi;
pub mod graphics;
pub mod memory;
pub mod panic;

use core::arch::global_asm;

use idos_api::{
    compat::VMRegisters,
    io::{
        file::FileStatus,
        sync::{close_sync, ioctl_sync, open_sync, read_sync, write_sync},
        termios::Termios,
        Handle, FILE_OP_STAT,
    },
    syscall::{io::create_file_handle, memory::map_memory},
};

use dos::{
    DOS_MEM_TOP, DOS_MEM_TOP_SEGMENT, PROGRAM_SEGMENT, PSP_BASE, PSP_SEGMENT,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

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

/// Address of a default IRET stub in low memory, just past the BIOS data area.
const IRET_STUB: u32 = 0x500;
pub(crate) const IRET_STUB_SEGMENT: u16 = 0x0050;
pub(crate) const IRET_STUB_OFFSET: u16 = 0x0000;

/// DPMI entry point stub: INT 0xFE at 0x501, followed by RETF at 0x503.
/// The v86 program calls this via FAR CALL; INT 0xFE triggers the DPMI switch.
const DPMI_ENTRY_STUB: u32 = 0x501;
/// Interrupt number used by the DPMI entry stub.
const DPMI_ENTRY_INT: u8 = 0xFE;

// ---------------------------------------------------------------------------
// Shared statics
// ---------------------------------------------------------------------------

static mut TERMIOS_ORIG: Termios = Termios::default();
pub(crate) static STDIN: Handle = Handle::new(0);
static LOG_HANDLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// IRQ mask passed to enter_8086, built up as the DOS program sets interrupt vectors
pub(crate) static mut VM86_IRQ_MASK: u32 = 0;
/// Virtual interrupt flag — tracks whether the DOS program has done CLI/STI
static mut VM86_IF: bool = true;

/// Timer tick divider state. The kernel PIT runs at 100 Hz, but DOS programs
/// expect ~18.2 Hz. We accumulate fractional ticks: every time the accumulator
/// reaches the threshold, we deliver one DOS tick.
/// Using fixed-point: 100 Hz / 18.2 Hz ≈ 5.4945. We accumulate 182 per tick
/// and fire when it reaches 1000 (1000/182 ≈ 5.4945).
static mut TIMER_ACCUM: u32 = 0;
const TIMER_ACCUM_PER_TICK: u32 = 182;
const TIMER_ACCUM_THRESHOLD: u32 = 1000;

// ---------------------------------------------------------------------------
// Shared utility functions
// ---------------------------------------------------------------------------

pub(crate) fn dos_log(msg: &[u8]) {
    let h = LOG_HANDLE.load(core::sync::atomic::Ordering::Relaxed);
    if h != 0 {
        let _ = write_sync(Handle::new(h), msg, 0);
    }
}

/// Resolve a seg:offset pointer to a linear address.
/// In v86 mode: (seg << 4) + (offset & 0xffff) (real-mode addressing).
/// In DPMI mode: descriptor base + offset (flat addressing via LDT shadow).
pub(crate) fn resolve_ptr(seg: u32, offset: u32) -> u32 {
    unsafe {
        if dpmi::DPMI_ACTIVE {
            let base = dpmi::dpmi_ldt_read(seg).base;
            base + offset
        } else {
            (seg << 4) + (offset & 0xffff)
        }
    }
}

/// Format "PREFIX XX\n" where XX is a hex byte.
pub(crate) fn fmt_unsupported(prefix: &[u8], value: u8, buf: &mut [u8; 32]) -> usize {
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

/// Toggle canonical mode (ICANON + ECHO) on stdin.
/// When enabled, the console line-buffers input with echo and editing.
pub(crate) fn set_stdin_canonical(enable: bool) {
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

pub(crate) fn exit(code: u32) -> ! {
    // exit graphics mode if active
    unsafe {
        if graphics::GFX_BUFFER_PADDR != 0 {
            graphics::exit_graphics_mode();
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

// ---------------------------------------------------------------------------
// Entry point and loaders
// ---------------------------------------------------------------------------

global_asm!(
    r#"
.global _start

_start:
    push ebx
    call dos_loader_start
"#
);

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

    dos::init_cwd(exec_path);

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

/// Load a .COM file: read the entire file into memory at PSP_BASE + 0x100,
/// then enter the VM with CS:IP = PSP_SEGMENT:0x100.
fn load_com(file_handle: Handle) -> ! {
    let mut file_status = FileStatus::new();
    let _ = idos_api::io::sync::io_sync(
        file_handle,
        FILE_OP_STAT,
        &mut file_status as *mut FileStatus as u32,
        core::mem::size_of::<FileStatus>() as u32,
        0,
    );
    let file_size = file_status.byte_size;

    setup_dos_memory();

    dos::setup_psp();
    read_file_into(file_handle, PSP_BASE + 0x100, file_size, 0);

    // Register the program's initial block in the arena
    memory::dos_arena_set_start(PSP_SEGMENT as u16);
    memory::dos_arena_record(PSP_SEGMENT as u16, DOS_MEM_TOP_SEGMENT - PSP_SEGMENT as u16);

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

    dos::setup_psp();

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

    // Register the program's initial block in the arena
    memory::dos_arena_set_start(PSP_SEGMENT as u16);
    memory::dos_arena_record(PSP_SEGMENT as u16, DOS_MEM_TOP_SEGMENT - PSP_SEGMENT as u16);

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

// ---------------------------------------------------------------------------
// v86 main loop
// ---------------------------------------------------------------------------

fn compat_start(mut vm_regs: VMRegisters) -> ! {
    // BSS is not guaranteed zeroed — explicitly init all module state
    graphics::init();
    dpmi::init();
    memory::init();
    unsafe {
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

    dos::init_file_table(stdaux);

    let kbd = create_file_handle();
    let _ = open_sync(kbd, "DEV:\\KEYBOARD", 0);
    bios::KBD_HANDLE.store(kbd.as_u32(), core::sync::atomic::Ordering::Relaxed);

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
                deliver_pending_irqs(pending, &mut vm_regs);
            }
            _ => break,
        }
    }

    exit(0);
}

// ---------------------------------------------------------------------------
// v86 fault handling and interrupt dispatch
// ---------------------------------------------------------------------------

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
            graphics::sync_graphics_buffer();
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

pub(crate) fn handle_interrupt(irq: u8, vm_regs: &mut VMRegisters) {
    match irq {
        0x10 => {
            // BIOS video services
            bios::bios_video(vm_regs);
        }
        0x16 => {
            // BIOS keyboard services
            bios::bios_keyboard(vm_regs);
        }
        0x21 => {
            // DOS API
            dos::dos_api(vm_regs);
        }
        0x2f => {
            // Multiplex interrupt
            dpmi::multiplex_int(vm_regs);
        }
        DPMI_ENTRY_INT => {
            // DPMI entry point — switch to protected mode
            dpmi::dpmi_enter(vm_regs);
        }

        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT ", irq, &mut buf);
            dos_log(&buf[..len]);
        }
    }
}

// ---------------------------------------------------------------------------
// Timer interrupt delivery
// ---------------------------------------------------------------------------

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
