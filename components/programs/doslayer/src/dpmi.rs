//! DPMI (DOS Protected Mode Interface) support for the DOS compatibility layer.
//!
//! Implements DPMI 0.9 host services: protected-mode entry, LDT descriptor
//! management, DOS and high memory allocation, real-mode interrupt simulation,
//! and the INT 31h API dispatch.

use idos_api::{
    compat::{LdtDescriptorParams, VMRegisters},
    syscall::memory::map_memory,
};

use super::{dos_log, exit, fmt_unsupported, handle_interrupt, resolve_ptr};
use super::{IRET_STUB_OFFSET, IRET_STUB_SEGMENT, VM86_IRQ_MASK};
use crate::dos::{dos_api, PSP_BASE};
use crate::memory::{
    dos_arena_alloc, dos_arena_free, dos_arena_largest, dos_arena_resize, dpmi_high_mem_free,
    dpmi_high_mem_record,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of the DPMI protected-mode stack.
const DPMI_PM_STACK_SIZE: u32 = 0x4000; // 16 KiB

/// Maximum number of LDT slots tracked in the shadow table.
const LDT_MAX_SLOTS: usize = 64;

/// DPMI entry point stub segment (real-mode far address returned by INT 2Fh AX=1687h).
pub(crate) const DPMI_ENTRY_SEGMENT: u16 = 0x0050;
/// DPMI entry point stub offset.
pub(crate) const DPMI_ENTRY_OFFSET: u16 = 0x0001;

// ---------------------------------------------------------------------------
// Statics
// ---------------------------------------------------------------------------

/// DPMI state — true once the client has switched to protected mode.
pub(crate) static mut DPMI_ACTIVE: bool = false;

/// LDT selectors allocated for the DPMI client during entry.
static mut DPMI_CS_SEL: u32 = 0;
static mut DPMI_DS_SEL: u32 = 0;
static mut DPMI_SS_SEL: u32 = 0;
static mut DPMI_ES_SEL: u32 = 0;

/// Base address of the DPMI protected-mode stack (mmap'd region).
static mut DPMI_PM_STACK_BASE: u32 = 0;

/// Shadow copy of descriptor params for each LDT slot.
/// Needed because the kernel syscall only supports full replacement,
/// but DPMI lets clients modify base/limit/access independently.
static mut DPMI_LDT_SHADOW: [LdtDescriptorParams; LDT_MAX_SLOTS] = {
    const ZERO: LdtDescriptorParams = LdtDescriptorParams {
        base: 0,
        limit: 0,
        access: 0,
        flags: 0,
    };
    [ZERO; LDT_MAX_SLOTS]
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Zero out all DPMI statics. Called once from `compat_start`.
pub(crate) fn init() {
    unsafe {
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
    }
}

// ---------------------------------------------------------------------------
// DPMI entry and protected-mode loop
// ---------------------------------------------------------------------------

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
pub(crate) fn dpmi_enter(vm_regs: &mut VMRegisters) {
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

// ---------------------------------------------------------------------------
// Interrupt handling from protected mode
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// LDT shadow table operations
// ---------------------------------------------------------------------------

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
pub(crate) fn dpmi_ldt_read(selector: u32) -> LdtDescriptorParams {
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

// ---------------------------------------------------------------------------
// INT 0x31 — DPMI API dispatch
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// INT 0x2F — Multiplex interrupt (DPMI detection)
// ---------------------------------------------------------------------------

/// INT 0x2F — Multiplex interrupt.
/// AX=0x1687: DPMI detection / get entry point.
pub(crate) fn multiplex_int(regs: &mut VMRegisters) {
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
