use crate::interrupts::syscall::FullSavedRegisters;
use crate::io::async_io::{AsyncIOTable, IOType};
use crate::io::handle::HandleTable;
use crate::memory::address::PhysicalAddress;
use crate::sync::wake_set::WakeSet;
use crate::time::system::{get_system_time, Timestamp};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use idos_api::io::error::IoResult;
use idos_api::ipc::Message;

use super::args::ExecArgs;
use super::id::TaskID;
use super::memory::MappedMemory;
use super::messaging::{MessagePacket, MessageQueue};
use super::stack::free_stack;

/// Stored FPU/SSE state for a task, saved and restored by FXSAVE/FXRSTOR.
/// Must be 16-byte aligned per the x86 FXSAVE requirement.
#[repr(C, align(16))]
pub struct FxState {
    pub data: [u8; 512],
}

impl FxState {
    /// Create a properly initialized FPU state, equivalent to what FINIT +
    /// LDMXCSR would produce: all exceptions masked, 64-bit precision,
    /// round-to-nearest.
    pub fn new() -> Self {
        let mut state = Self { data: [0u8; 512] };
        // FCW: 0x037F — all x87 exceptions masked, extended (64-bit) precision
        state.data[0] = 0x7F;
        state.data[1] = 0x03;
        // MXCSR: 0x1F80 — all SSE exceptions masked
        state.data[24] = 0x80;
        state.data[25] = 0x1F;
        state
    }

    pub unsafe fn save(&mut self) {
        core::arch::asm!(
            "fxsave [{}]",
            in(reg) self.data.as_mut_ptr(),
            options(nostack),
        );
    }

    pub unsafe fn restore(&self) {
        core::arch::asm!(
            "fxrstor [{}]",
            in(reg) self.data.as_ptr(),
            options(nostack),
        );
    }
}

pub struct Task {
    /// The unique identifier for this Task
    pub id: TaskID,
    /// The ID of the parent Task
    pub parent_id: TaskID,
    /// Represents the current execution state of the task
    pub state: RunState,
    /// Timestamp when the Task was created
    pub created_at: Timestamp,

    /// A Box pointing to the kernel stack for this task. This stack will be
    /// used when the task is executing kernel-mode code.
    /// The stack Box is wrapped in an Option so that we can replace it with
    /// None before the Task struct is dropped. If any code attempts to drop
    /// the stack Box, it will panic because it was not created by the global
    /// allocator.
    pub kernel_stack: Option<Box<[u8]>>,
    /// Stores the kernel stack pointer when the task is swapped out. When the
    /// task is resumed by the scheduler, this address will be placed in $esp.
    /// Registers will be popped off the stack to resume the execution state
    /// of the task.
    pub stack_pointer: usize,
    /// Physical address of the task's page directory
    pub page_directory: PhysicalAddress,
    /// Stores all of the memory mappings for the Task
    pub memory_mapping: MappedMemory<0xc0000000>,

    /// Store Messages that have been sent to this task
    pub message_queue: MessageQueue,
    /// Store Wake Sets that have been allocated to this task
    pub wake_sets: HandleTable<Arc<WakeSet>>,

    /// The name of the executable file running in the thread
    pub filename: String,
    /// The arguments passed to the executable
    pub args: ExecArgs,

    /// Store references to all open handles
    pub open_handles: HandleTable<u32>,
    /// Stores the actual active async IO objects
    pub async_io_table: AsyncIOTable,

    /// Stores the last received result from a file mapping request
    pub last_map_result: Option<IoResult>,

    /// Storage for the task's registers when it enters VM86 mode
    pub vm86_registers: Option<FullSavedRegisters>,
    /// IRQ bitmask for virtual interrupt delivery in VM86 mode
    pub vm86_irq_mask: u32,

    /// FPU/SSE register state, saved and restored on every context switch
    pub fpu_state: FxState,
}

impl Task {
    pub fn new(id: TaskID, parent_id: TaskID, stack: Box<[u8]>) -> Self {
        let stack_pointer = (stack.as_ptr() as usize) + stack.len() - core::mem::size_of::<u32>();
        Self {
            id,
            parent_id,
            state: RunState::Uninitialized,
            created_at: get_system_time().to_timestamp(),
            kernel_stack: Some(stack),
            stack_pointer,
            page_directory: PhysicalAddress::new(0),
            memory_mapping: MappedMemory::new(),
            message_queue: MessageQueue::new(),
            wake_sets: HandleTable::new(),
            filename: String::new(),
            args: ExecArgs::new(),
            open_handles: HandleTable::new(),
            async_io_table: AsyncIOTable::new(),
            last_map_result: None,
            vm86_registers: None,
            vm86_irq_mask: 0,
            fpu_state: FxState::new(),
        }
    }

    pub fn create_initial_task() -> Self {
        let id = TaskID::new(0);
        let stack = super::stack::create_initial_stack();
        let mut task = Self::new(id, id, stack);
        task.state = RunState::Running;
        task.filename = String::from("IDLE");
        task
    }

    pub fn get_kernel_stack(&self) -> &Box<[u8]> {
        match &self.kernel_stack {
            Some(stack) => stack,
            None => panic!("Task does not have a stack"),
        }
    }

    pub fn get_kernel_stack_mut(&mut self) -> &mut Box<[u8]> {
        match &mut self.kernel_stack {
            Some(stack) => stack,
            None => panic!("Task does not have a stack"),
        }
    }

    pub fn get_stack_top(&self) -> usize {
        let stack = self.get_kernel_stack();
        (stack.as_ptr() as usize) + stack.len()
    }

    pub fn reset_stack_pointer(&mut self) {
        self.stack_pointer = self.get_stack_top();
    }

    /// Push a u8 value onto the kernel stack
    pub fn stack_push_u8(&mut self, value: u8) {
        self.stack_pointer -= 1;
        let esp = self.stack_pointer;
        let stack = self.get_kernel_stack_mut();
        let stack_start = stack.as_ptr() as usize;
        let offset = esp - stack_start;
        stack[offset] = value;
    }

    pub fn stack_push_u32(&mut self, value: u32) {
        self.stack_pointer -= 4;
        let esp = self.stack_pointer;
        let stack = self.get_kernel_stack_mut();
        let stack_start = stack.as_ptr() as usize;
        let offset = esp - stack_start;
        stack[offset + 0] = ((value & 0x000000ff) >> 0) as u8;
        stack[offset + 1] = ((value & 0x0000ff00) >> 8) as u8;
        stack[offset + 2] = ((value & 0x00ff0000) >> 16) as u8;
        stack[offset + 3] = ((value & 0xff000000) >> 24) as u8;
    }

    pub fn initialize_registers(&mut self) {
        self.stack_push_u32(0);
        self.stack_push_u32(0);
        self.stack_push_u32(0);
        self.stack_push_u32(0);
        self.stack_push_u32(0);
        self.stack_push_u32(0);
        self.stack_push_u32(0);
    }

    pub fn set_entry_point(&mut self, f: fn() -> !) {
        self.initialize_registers();
        self.stack_push_u32(f as *const () as u32);
    }

    /// Determine if the scheduler can re-enter this task
    pub fn can_resume(&self) -> bool {
        match self.state {
            RunState::Initialized => true,
            RunState::Running => true,
            _ => false,
        }
    }

    pub fn make_runnable(&mut self) {
        if let RunState::Uninitialized = self.state {
            self.state = RunState::Initialized;
        }
    }

    pub fn set_filename(&mut self, name: &String) {
        self.filename.clone_from(name);
    }

    /// End all execution of the task, and mark its resources as available for
    /// cleanup
    pub fn terminate(&mut self) {
        self.state = RunState::Terminated;
    }

    pub fn is_terminated(&self) -> bool {
        match self.state {
            RunState::Terminated => true,
            _ => false,
        }
    }

    /// Notify the task that time has passed, in case it is currently in a
    /// blocked state. If the block has a timeout that has now expired, the
    /// task is resumed and the method returns true. In all other cases it
    /// returns false.
    pub fn update_timeout(&mut self, ms: u32) -> bool {
        match self.state {
            RunState::Blocked(Some(t), block_type) => {
                if t <= ms {
                    self.state = RunState::Running;
                    return true;
                } else {
                    self.state = RunState::Blocked(Some(t - ms), block_type);
                }
            }
            _ => (),
        }
        false
    }

    pub fn sleep(&mut self, timeout_ms: u32) {
        if let RunState::Running = self.state {
            self.state = RunState::Blocked(Some(timeout_ms), BlockType::Sleep);
        } else {
            panic!("Cannot sleep a non-running task");
        }
    }

    pub fn read_message(&mut self, current_ticks: u32) -> (Option<MessagePacket>, bool) {
        self.message_queue.read(current_ticks)
    }

    /// Place a Message in this task's queue. If the task is currently blocked
    /// on reading the message queue, it will resume running.
    /// Each message is accompanied by an expiration time (in system ticks),
    /// after which point the message is considered invalid.
    pub fn receive_message(
        &mut self,
        current_ticks: u32,
        from: TaskID,
        message: Message,
        expiration_ticks: u32,
    ) {
        self.message_queue
            .add(from, message, current_ticks, expiration_ticks);
    }

    pub fn get_message_io_provider(&self) -> Option<(u32, Arc<IOType>)> {
        self.async_io_table.get_message_io()
    }

    pub fn async_io_complete(&self, io_index: u32) -> Option<Arc<IOType>> {
        self.async_io_table
            .get(io_index)
            .map(|entry| entry.io_type.clone())
    }

    pub fn futex_wait(&mut self, timeout: Option<u32>) {
        self.state = RunState::Blocked(timeout, BlockType::Futex);
    }

    pub fn futex_wake(&mut self) {
        match self.state {
            RunState::Blocked(_, BlockType::Futex) => {
                self.state = RunState::Running;
            }
            _ => (),
        }
    }

    pub fn begin_file_mapping_request(&mut self) {
        if let RunState::Running = self.state {
            self.state = RunState::Blocked(None, BlockType::FileMapping);
            self.last_map_result = None;
        }
    }

    pub fn resolve_file_mapping_request(&mut self, result: IoResult) -> bool {
        if let RunState::Blocked(_, BlockType::FileMapping) = self.state {
            self.state = RunState::Running;
            self.last_map_result = Some(result);
            true
        } else {
            false
        }
    }

    pub fn push_arg(&mut self, arg: &[u8]) {
        self.args.add(arg);
    }

    pub fn push_args<I, A>(&mut self, args: I)
    where
        I: IntoIterator<Item = A>,
        A: AsRef<[u8]>,
    {
        for arg in args {
            self.args.add(arg.as_ref());
        }
    }

    pub fn has_executable(&self) -> bool {
        false
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        let stack = self.kernel_stack.take();
        if let Some(b) = stack {
            free_stack(b);
        }
    }
}

/// RunState represents the current state of the task, and determines how the
/// task scheduler treats it. It is mostly used to represent the ways that an
/// existing task may not be actively running.
///
/// A task is initially created with an Uninitialized state. Until an
/// executable program is attached, or the task is explicitly marked as ready,
/// the kernel assumes there is no code to run, so the task is ignored.
///
/// When a task is Running, the kernel assumes that it can be safely executed.
/// The scheduler will consider this task as a candidate for the next one to
/// run.
///
/// When a program crashes, exits, or is killed by a soft interrupt, it moves
/// to a Terminated state. This allows the task data to remain in memory until
/// the kernel is able to notify its parent and clean up the resources
/// associated with the terminated task. A kernel-level task regularly walks
/// the task map and handles any terminated tasks.
///
/// A task becomes Blocked when it wants to pause execution and yield the CPU
/// to other tasks. This may be waiting for a fixed amount of time (sleeping)
/// or blocking until hardware or another task is ready. The Blocked state
/// contains information on what conditions will allow the task to resume
/// execution, as well as an optional timeout. This allows every blocking
/// operation to resume even if the condition is never met, so that tasks
/// can avoid blocking indefinitely.
///
/// State alones doesn't make a task runnable. The scheduler must place the task
/// on one of the run queues. Tasks start as Uninitialized, and are not placed
/// on a run queue until they are marked as Initialized. The first time an
/// Initialized task is picked up by the scheduler, it is switched to Running.
/// As long as it remains Running, it will be placed on another run queue when
/// it yields or its time slice is up.
/// A task can become invalid for scheduling if it terminates or blocks. In
/// those cases it is not removed from a run queue, but if the scheduler
/// encounters a non-runnable task it will not re-enqueue it.
/// When a blocked task resumes, it is placed on an available run queue, just
/// like a newly initialized task.
#[derive(Copy, Clone)]
pub enum RunState {
    /// The Task has been created, but is not ready to be executed
    Uninitialized,
    /// The Task is executable, but has not run yet. This requires some special
    /// code to safely switch into from another running task
    Initialized,
    /// The Task can be safely run by the scheduler
    Running,
    /// The Task has ended, but still needs to be cleaned up
    Terminated,
    /// The Task is blocked on some condition, with an optional timeout
    Blocked(Option<u32>, BlockType),
}

impl core::fmt::Display for RunState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Uninitialized => f.write_str("Uninit"),
            Self::Initialized => f.write_str("Init"),
            Self::Running => f.write_str("Run"),
            Self::Terminated => f.write_str("Term"),
            Self::Blocked(_, BlockType::Sleep) => f.write_str("Sleep"),
            Self::Blocked(_, BlockType::Futex) => f.write_str("FutexWait"),
            Self::Blocked(_, BlockType::FileMapping) => f.write_str("FileMapWait"),
        }
    }
}

/// A task may block on a variety of hardware or software conditions. The
/// BlockType describes why the task is blocked, and how it can be resumed.
#[derive(Copy, Clone)]
pub enum BlockType {
    /// The Task is sleeping for a fixed period of time, stored in the timeout
    Sleep,

    /// The task is blocked on a futex
    Futex,

    /// The task is blocked on an async file-mapping operation
    FileMapping,
}
