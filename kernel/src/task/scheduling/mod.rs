//! Scheduling of tasks has been enhanced to support multi-core CPUs.
//! Each CPU has its own run queue and handles scheduling of eligible tasks.
//! In addition, all cores can pick up async tasklets, which are also added to
//! their queues.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use alloc::collections::VecDeque;
use spin::Mutex;

use crate::{
    arch::gdt::GdtEntry,
    hardware::lapic::LocalAPIC,
    memory::{
        address::{PhysicalAddress, VirtualAddress},
        physical::allocate_frame,
    },
};

use super::{
    id::{AtomicTaskID, TaskID},
    map::get_task,
    paging::{current_pagedir_map, current_pagedir_map_explicit, PermissionFlags},
    stack::KERNEL_STACKS_BOTTOM,
    switching::switch_to,
};

/// This struct is instantiated once per CPU core, and manages data necessary to
/// run and switch tasks on that core.
#[repr(C)]
pub struct CPUScheduler {
    linear_address: VirtualAddress,
    cpu_index: usize,
    current_task: AtomicTaskID,
    idle_task: TaskID,
    pub work_queue: Mutex<VecDeque<WorkItem>>,

    gdt: [GdtEntry; 8],

    pub has_lapic: bool,
    current_ticks: AtomicU8,
}

impl CPUScheduler {
    pub fn new(cpu_index: usize, idle_task: TaskID, linear_address: VirtualAddress) -> Self {
        let mut gdt = unsafe { crate::arch::gdt::GDT.clone() };
        gdt[5].set_base(linear_address.as_u32());

        Self {
            linear_address,
            cpu_index,
            current_task: AtomicTaskID::new(0),
            idle_task,
            work_queue: Mutex::new(VecDeque::new()),
            gdt,

            has_lapic: false,
            current_ticks: AtomicU8::new(0),
        }
    }

    pub fn load_gdt(&mut self) {
        crate::arch::gdt::init_tss(&mut self.gdt[7]);
        let mut gdtr = crate::arch::gdt::GdtDescriptor::new();
        gdtr.point_to(&self.gdt);
        gdtr.load();
        crate::arch::gdt::ltr(0x38);
    }

    pub fn get_current_task(&self) -> TaskID {
        self.current_task.load(Ordering::SeqCst)
    }

    pub fn get_idle_task(&self) -> TaskID {
        self.idle_task
    }

    pub fn get_next_work_item(&self) -> Option<WorkItem> {
        self.work_queue.lock().pop_front()
    }

    pub fn reenqueue_work_item(&self, item: WorkItem) {
        self.work_queue.lock().push_back(item);
    }

    /// Called every timer tick (10ms). Returns true if the current task's
    /// time slice has expired.
    pub fn tick(&self) -> bool {
        let prev = self.current_ticks.fetch_add(1, Ordering::Relaxed);
        if prev >= 1 {
            // 2 ticks = 20ms time slice (~50Hz)
            self.current_ticks.store(0, Ordering::Relaxed);
            return true;
        }
        false
    }
}

pub enum WorkItem {
    Task(TaskID),
    Tasklet(Tasklet),
}

pub struct Tasklet {}

pub fn create_cpu_scheduler(
    cpu_index: usize,
    idle_task: TaskID,
    has_lapic: bool,
) -> VirtualAddress {
    // Create an area of memory for the CPU's scheduler struct
    // This memory is only referenced by the scheduler itself, so it can be
    // directly allocated and should never be freed.
    let mapped_to = VirtualAddress::new((KERNEL_STACKS_BOTTOM - 0x2000 * (cpu_index + 1)) as u32);
    let frame = allocate_frame().unwrap();
    current_pagedir_map(frame, mapped_to, PermissionFlags::empty());

    unsafe {
        let scheduler_ptr = mapped_to.as_ptr_mut::<CPUScheduler>();
        scheduler_ptr.write(CPUScheduler::new(cpu_index, idle_task, mapped_to));

        let scheduler = &mut *scheduler_ptr;
        scheduler.has_lapic = has_lapic;
        scheduler.load_gdt();
    }

    if has_lapic {
        // map the CPU's LAPIC to the page beyond the scheduler struct
        let lapic_mapping = mapped_to + 0x1000;

        let mut lapic_phys: u32;
        unsafe {
            let msr: u32 = 0x1b;
            core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lapic_phys, out("edx") _);
        }
        lapic_phys &= 0xfffff000;
        current_pagedir_map_explicit(
            PhysicalAddress::new(lapic_phys),
            lapic_mapping,
            PermissionFlags::empty(),
        );
    }

    mapped_to
}

/// Get the CPUScheduler instance for the current CPU
pub fn get_cpu_scheduler() -> &'static mut CPUScheduler {
    // GS shouldn't be set here, but it's getting overridden by userspace
    // programs. We should probably set it whenever entering the kernel, or
    // when switching tasks.
    unsafe {
        let raw_addr: u32;
        asm!(
            "mov gs, {}",
            "mov {}, gs:[0]",
            in(reg) 0x28,
            out(reg) raw_addr,
        );
        let addr = VirtualAddress::new(raw_addr);

        &mut *addr.as_ptr_mut::<CPUScheduler>()
    }
}

pub fn get_lapic() -> LocalAPIC {
    unsafe {
        let raw_addr: u32;
        asm!(
            "mov gs, {}",
            "mov {}, gs:[0]",
            in(reg) 0x28,
            out(reg) raw_addr,
        );
        let addr = VirtualAddress::new(raw_addr + 0x1000);

        LocalAPIC::new(addr)
    }
}

/// Put a task back on any work queue, making it eligible for execution again.
pub fn reenqueue_task(id: TaskID) {
    //crate::kprintln!("SCHEDULER: re-enqueue task {:?}", id);
    let scheduler = get_cpu_scheduler();
    scheduler.reenqueue_work_item(WorkItem::Task(id));
}

/// If the current task is still running, put it on the back of the CPU's work
/// queue. Then, pop the first item off of the work queue. If there is no
/// runnable task, switch to the current CPU's idle task.
pub fn switch() {
    let scheduler = get_cpu_scheduler();
    let current_id = scheduler.current_task.load(Ordering::SeqCst);

    if current_id != scheduler.idle_task {
        // if the current task isn't the idle task, and it is still running,
        // re-enqueue it
        let current_task = get_task(current_id);
        if let Some(task) = current_task {
            if task.read().can_resume() {
                scheduler.reenqueue_work_item(WorkItem::Task(current_id));
            }
        }
    }

    let switch_to_id = loop {
        // Pop the first item off the queue, if one exists.
        // It's not guaranteed that enqueued tasks are runnable, since their
        // state may have changed. If the popped task is not runnable, discard
        // the ID and fetch another one.
        match scheduler.get_next_work_item() {
            Some(WorkItem::Task(id)) => {
                if let Some(task_lock) = get_task(id) {
                    if task_lock.read().can_resume() {
                        break id;
                    }
                }
            }
            Some(WorkItem::Tasklet(_)) => panic!("Tasklet isn't supported"),
            None => {
                // if there's nothing in the queue, switch to the idle task
                break scheduler.idle_task;
            }
        }
    };

    if current_id == switch_to_id {
        return;
    }

    scheduler.current_task.swap(switch_to_id, Ordering::SeqCst);

    switch_to(switch_to_id);
}
