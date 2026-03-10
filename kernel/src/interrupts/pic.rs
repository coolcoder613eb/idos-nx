use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU32, Ordering};
use spin::RwLock;

use super::stack::{SavedState, StackFrame};
use crate::io::async_io::IOType;
use crate::task::scheduling::{get_cpu_scheduler, get_lapic};
use crate::{
    hardware::pic::PIC,
    task::{id::TaskID, map::get_task},
};

global_asm!(
    r#"
.global pic_irq_0, pic_irq_1, pic_irq_3, pic_irq_4, pic_irq_5, pic_irq_6, pic_irq_7, pic_irq_8, pic_irq_9, pic_irq_a, pic_irq_b, pic_irq_c, pic_irq_d, pic_irq_e, pic_irq_f

pic_irq_0:
    push 0x0
    jmp pic_irq_core

pic_irq_1:
    push 0x1
    jmp pic_irq_core

pic_irq_3:
    push 0x3
    jmp pic_irq_core

pic_irq_4:
    push 0x4
    jmp pic_irq_core

pic_irq_5:
    push 0x5
    jmp pic_irq_core

pic_irq_6:
    push 0x6
    jmp pic_irq_core

pic_irq_7:
    push 0x7
    jmp pic_irq_core

pic_irq_8:
    push 0x8
    jmp pic_irq_core

pic_irq_9:
    push 0x9
    jmp pic_irq_core

pic_irq_a:
    push 0xa
    jmp pic_irq_core

pic_irq_b:
    push 0xb
    jmp pic_irq_core

pic_irq_c:
    push 0xc
    jmp pic_irq_core

pic_irq_d:
    push 0xd
    jmp pic_irq_core

pic_irq_e:
    push 0xe
    jmp pic_irq_core

pic_irq_f:
    push 0xf
    jmp pic_irq_core

# called once the serviced IRQ number has been pushed onto the stack,
pic_irq_core:
    push eax
    push ecx
    push edx
    push ebx
    push ebp
    push esi
    push edi

    # Push pointers for _handle_pic_interrupt(&StackFrame, irq, &SavedRegisters)
    mov ebx, esp
    push ebx                    # arg3: &SavedRegisters (= ESP after pushing 7 regs)
    push dword ptr [ebx + 7*4]  # arg2: irq number (by value)
    lea eax, [ebx + 7*4 + 4]
    push eax                    # arg1: &StackFrame

    call _handle_pic_interrupt
    add esp, 12

    pop edi
    pop esi
    pop ebp
    pop ebx
    pop edx
    pop ecx
    pop eax
    add esp, 4 # clear the irq number

    iretd
"#
);

/// Handle interrupts that come from the PIC
#[no_mangle]
pub extern "C" fn _handle_pic_interrupt(frame: &StackFrame, irq: u32, _registers: &SavedState) {
    let pic = PIC::new();

    if irq == 0 {
        // IRQ 0 is not installable, and is hard-coded to the kernel's PIT
        // interrupt handler
        handle_pit_interrupt();

        // CPU time accounting: attribute this tick based on what we interrupted
        let is_user = frame.cs & 3 != 0 || frame.eflags & 0x20000 != 0;
        let scheduler = get_cpu_scheduler();
        let is_idle = scheduler.get_current_task() == scheduler.get_idle_task();
        crate::time::system::record_cpu_tick(is_user, is_idle);

        if scheduler.has_lapic {
            get_lapic().broadcast_ipi(0xf0);
        }

        let should_preempt = scheduler.tick();

        // Virtual interrupt delivery to v86 tasks: if we interrupted a v86
        // task that has the timer IRQ enabled, mark it pending and inject
        // the trap flag so the next instruction triggers #DB, exiting to
        // doslayer for delivery.
        let is_vm86 = frame.eflags & 0x20000 != 0;
        if is_vm86 {
            let task_lock = crate::task::switching::get_current_task();
            let mut task = task_lock.write();
            if task.vm86_irq_mask & idos_api::compat::VM86_IRQ_TIMER != 0 {
                task.vm86_pending_irqs |= idos_api::compat::VM86_IRQ_TIMER;
                // Set TF (bit 8) in the real eflags on the interrupt frame.
                // frame is now a reference to the actual stack, so this works.
                frame.set_eflags(frame.eflags | 0x100);
            }
        }

        pic.end_of_interrupt(0);

        // Preempt if the time slice expired and we interrupted userspace
        // (ring 3 or VM86 mode).
        if should_preempt && (frame.cs & 3 != 0 || is_vm86) {
            crate::task::actions::yield_coop();
        }

        return;
    }

    // need to check 7 and 15 for spurious interrupts
    if irq == 7 {
        let serviced = pic.get_interrupts_in_service();
        if serviced & 0x80 == 0 {
            return;
        }
    }
    if irq == 15 {
        let serviced = pic.get_interrupts_in_service();
        if serviced & 0x8000 == 0 {
            pic.end_of_interrupt(2);
            return;
        }
    }

    try_installed_handler(irq);

    if irq != 1 && irq != 12 {
        // don't spam the console for keyboard and mouse interrupts
        crate::kprintln!("!!! INT {}", irq);
    }

    let should_notify = has_listeners(irq as u8);
    if should_notify {
        pic.mask_interrupt(irq as u8);
        notify_interrupt_listeners(irq as u8);
    }

    pic.end_of_interrupt(irq as u8);
}

/// The PIT triggers at 100Hz, and is used to update the internal clock and the
/// task scheduler.
pub fn handle_pit_interrupt() {
    crate::time::system::tick();
    crate::task::switching::update_timeouts(crate::time::system::MS_PER_TICK);
}

const EMPTY_LISTENERS: RwLock<BTreeMap<TaskID, u32>> = RwLock::new(BTreeMap::new());
static INTERRUPT_LISTENERS: [RwLock<BTreeMap<TaskID, u32>>; 16] = [EMPTY_LISTENERS; 16];
static ACTIVE_INTERRUPTS_LOW: AtomicU32 = AtomicU32::new(0);
static ACTIVE_INTERRUPTS_HIGH: AtomicU32 = AtomicU32::new(0);

pub fn add_interrupt_listener(irq: u8, task: TaskID, io_index: u32) -> bool {
    if irq > 15 {
        return false;
    }
    match INTERRUPT_LISTENERS[irq as usize]
        .write()
        .try_insert(task, io_index)
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

pub fn has_listeners(irq: u8) -> bool {
    if irq > 15 {
        return false;
    }
    let is_empty = match INTERRUPT_LISTENERS[irq as usize].try_read() {
        Some(map) => map.is_empty(),
        None => true,
    };
    !is_empty
}

pub fn notify_interrupt_listeners(irq: u8) {
    if irq > 15 {
        return;
    }
    let active = if irq > 7 {
        &ACTIVE_INTERRUPTS_HIGH
    } else {
        &ACTIVE_INTERRUPTS_LOW
    };
    let mask = 1 << ((irq & 7) as usize);
    let prev_mask = active.fetch_or(mask, Ordering::SeqCst);
    if prev_mask & mask == 0 {
        // the flag was newly raised
        let task_list: Vec<(TaskID, u32)> = INTERRUPT_LISTENERS[irq as usize]
            .read()
            .iter()
            .map(|(id, index)| (*id, *index))
            .collect();
        for (id, io_index) in task_list.iter() {
            let task_lock = match get_task(*id) {
                Some(lock) => lock,
                None => continue,
            };
            let io_provider = task_lock
                .read()
                .async_io_table
                .get(*io_index)
                .map(|entry| entry.io_type.clone());
            match io_provider {
                Some(iotype) => match *iotype {
                    IOType::Interrupt(ref provider) => {
                        provider.interrupt_fired();
                    }
                    _ => continue,
                },
                None => continue,
            }
        }
    }
}

pub fn is_interrupt_active(irq: u8) -> bool {
    if irq > 15 {
        return false;
    }
    let active = if irq > 7 {
        &ACTIVE_INTERRUPTS_HIGH
    } else {
        &ACTIVE_INTERRUPTS_LOW
    };
    let mask = 1 << ((irq & 7) as usize);
    let value = active.load(Ordering::SeqCst);
    value & mask != 0
}

pub fn acknowledge_interrupt(irq: u8) {
    if irq > 15 {
        return;
    }
    let active = if irq > 7 {
        &ACTIVE_INTERRUPTS_HIGH
    } else {
        &ACTIVE_INTERRUPTS_LOW
    };
    let mask = !(1 << ((irq & 7) as usize));
    active.fetch_and(mask, Ordering::SeqCst);
    PIC::new().unmask_interrupt(irq);
}

#[derive(Copy, Clone)]
pub enum InstallableHandlerType {
    Empty,
    Kernel(fn(u32) -> ()),
    KernelTask(fn(u32) -> (), TaskID),
}

pub type InstallableHandler = RwLock<InstallableHandlerType>;

const UNINSTALLED_HANDLER: InstallableHandler = RwLock::new(InstallableHandlerType::Empty);

static INSTALLED_HANDLERS: [InstallableHandler; 16] = [UNINSTALLED_HANDLER; 16];

pub fn install_interrupt_handler(irq: u32, f: fn(u32) -> (), task: Option<TaskID>) {
    match INSTALLED_HANDLERS[irq as usize].try_write() {
        Some(mut inner) => {
            let handler_type = match task {
                Some(id) => InstallableHandlerType::KernelTask(f, id),
                None => InstallableHandlerType::Kernel(f),
            };
            *inner = handler_type;
        }
        None => (),
    }
}

pub fn try_installed_handler(irq: u32) {
    let handler = match INSTALLED_HANDLERS[irq as usize].try_read() {
        Some(inner) => *inner,
        None => return,
    };
    match handler {
        InstallableHandlerType::Empty => return,
        InstallableHandlerType::Kernel(f) => f(irq),
        InstallableHandlerType::KernelTask(f, id) => {
            // TODO: make this more durable, store registers, handle all cases
            // where an interrupt can happen, etc.
            // Ultimately, expanding this is necessary to support usermode
            // interrupt handlers
            crate::kprintln!("Temporarily switch memory to {:?}", id);
            let task_lock = match get_task(id) {
                Some(lock) => lock,
                None => return,
            };
            let cr3 = task_lock.read().page_directory.as_u32();
            let prev_cr3: u32;
            unsafe {
                asm!(
                    "mov {prev}, cr3",
                    "mov cr3, {next}",
                    prev = out(reg) prev_cr3,
                    next = in(reg) cr3,
                );
            }
            f(irq);
            unsafe {
                asm!(
                    "mov cr3, {prev}",
                    prev = in(reg) prev_cr3,
                );
            }
            crate::kprintln!("Int memory switch done");
        }
    }
}
