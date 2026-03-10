use idos_api::compat::VMRegisters;

use crate::{interrupts::syscall::FullSavedRegisters, task::switching::get_current_task};

use core::arch::asm;

pub fn enter_vm86_mode(
    registers: &FullSavedRegisters,
    vm_regs_ptr: *mut VMRegisters,
    irq_mask: u32,
) {
    let task_lock = get_current_task();

    {
        let mut task = task_lock.write();
        task.vm86_registers = Some(registers.clone());
        task.vm86_irq_mask = irq_mask;
    }

    let vm_regs = unsafe { &mut *vm_regs_ptr };
    vm_regs.eflags |= 0x20200; // VM flag (bit 17) + IF (bit 9)

    let vm_regs_copy = vm_regs.clone();

    crate::kprintln!(
        "Enter 8086 Mode @ {:X}:{:X} IRQ({:X})",
        vm_regs.cs,
        vm_regs.eip,
        irq_mask
    );

    unsafe {
        asm!(
            "mov esp, eax",
            "pop eax",
            "pop ebx",
            "pop ecx",
            "pop edx",
            "pop esi",
            "pop edi",
            "pop ebp",
            "iretd",
            in("eax") &vm_regs_copy as *const VMRegisters as u32
        )
    }
}

/// Enter DPMI protected mode. The caller's registers are saved so that when
/// the DPMI code faults (INT instruction, GPF, etc.), we can restore the
/// caller's context and return the exit reason.
///
/// The IRETD frame for ring 3 protected mode is:
///   [ESP] → EIP, CS, EFLAGS, ESP(ring3), SS(ring3)
/// We also need to load DS/ES/FS/GS with the DPMI selectors before IRETD,
/// since the CPU only restores CS and SS from the interrupt frame.
pub fn enter_protected_mode(registers: &FullSavedRegisters, vm_regs_ptr: *mut VMRegisters) {
    let task_lock = get_current_task();

    {
        let mut task = task_lock.write();
        task.dpmi_registers = Some(registers.clone());
    }

    let vm_regs = unsafe { &mut *vm_regs_ptr };

    // Ensure IF is set so hardware interrupts still fire
    vm_regs.eflags |= 0x200;

    crate::kprintln!(
        "Enter Protected Mode @ {:X}:{:X} (SS {:X}:{:X})",
        vm_regs.cs,
        vm_regs.eip,
        vm_regs.ss,
        vm_regs.esp
    );

    // We use the same trick as enter_vm86: point ESP at the VMRegisters struct
    // and pop everything off. VMRegisters layout:
    //   [+0]  eax, [+4] ebx, [+8] ecx, [+12] edx, [+16] esi, [+20] edi, [+24] ebp
    //   [+28] eip, [+32] cs, [+36] eflags, [+40] esp, [+44] ss
    //   [+48] es, [+52] ds, [+56] fs, [+60] gs
    //
    // We need to build a stack that pops GP regs, loads segments, then IRETs.
    // Strategy: use EAX as the struct pointer, read everything via SS-relative
    // addressing (SS is still kernel selector at this point).

    let regs_ptr = vm_regs as *const VMRegisters as u32;
    unsafe {
        asm!(
            // Build IRETD frame on the kernel stack (push in reverse: SS, ESP, EFLAGS, CS, EIP)
            "push dword ptr ss:[eax + 44]",  // SS
            "push dword ptr ss:[eax + 40]",  // ESP
            "push dword ptr ss:[eax + 36]",  // EFLAGS
            "push dword ptr ss:[eax + 32]",  // CS
            "push dword ptr ss:[eax + 28]",  // EIP

            // Load segment registers from the struct (DS last since we need it for access)
            "mov gs, ss:[eax + 60]",
            "mov fs, ss:[eax + 56]",
            "mov es, ss:[eax + 48]",

            // Load GP registers (use SS: prefix since DS is about to change)
            "mov ebx, ss:[eax + 4]",
            "mov ecx, ss:[eax + 8]",
            "mov edx, ss:[eax + 12]",
            "mov esi, ss:[eax + 16]",
            "mov edi, ss:[eax + 20]",
            "mov ebp, ss:[eax + 24]",

            // Load DS last (after this, can't use DS-relative addressing)
            "mov ds, ss:[eax + 52]",

            // Load EAX last (it was our pointer register)
            "mov eax, ss:[eax + 0]",

            "iretd",

            in("eax") regs_ptr,
            options(noreturn)
        )
    }
}
