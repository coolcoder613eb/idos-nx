//! Async IO-compatible device driver for COM ports
//!
//! The COM driver handles incoming data from the port, as well as data written
//! by user programs that should be output on the port.

use core::sync::atomic::{AtomicU32, Ordering};

use alloc::collections::{BTreeMap, VecDeque};
use idos_api::io::{
    driver::DriverCommand,
    error::{IoError, IoResult},
    AsyncOp, ASYNC_OP_READ,
};
use idos_api::ipc::Message;

use crate::task::actions::{
    handle::{open_interrupt_handle, open_message_queue},
    io::{driver_io_complete, send_io_op, write_sync},
    lifecycle::create_kernel_task,
    sync::{block_on_wake_set, create_wake_set},
};

use super::serial::with_port;

/// Main event loop of the COM driver
pub fn run_driver() -> ! {
    let messages_handle = open_message_queue();
    let mut incoming_message = Message::empty();

    let irq_handle = open_interrupt_handle(4);
    let mut interrupt_ready: [u8; 1] = [0];

    let wake_set = create_wake_set();

    let mut driver_impl = ComDeviceDriver::new(0);

    let mut interrupt_read = AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
    let _ = send_io_op(irq_handle, &interrupt_read, Some(wake_set));
    let mut message_read = AsyncOp::new(
        ASYNC_OP_READ,
        &mut incoming_message as *mut Message as u32,
        core::mem::size_of::<Message>() as u32,
        0,
    );
    let _ = send_io_op(messages_handle, &message_read, Some(wake_set));
    loop {
        if interrupt_read.is_complete() {
            let _ = write_sync(irq_handle, &[1], 0);

            match driver_impl.init_read() {
                Some((request_id, result)) => driver_io_complete(request_id, Ok(result)),
                None => (),
            }

            interrupt_read = AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
            let _ = send_io_op(irq_handle, &interrupt_read, Some(wake_set));
        } else if message_read.is_complete() {
            let request_id = incoming_message.unique_id;
            match driver_impl.handle_request(incoming_message) {
                Some(result) => driver_io_complete(request_id, result),
                None => (),
            }

            message_read = AsyncOp::new(
                ASYNC_OP_READ,
                &mut incoming_message as *mut Message as u32,
                core::mem::size_of::<Message>() as u32,
                0,
            );
            let _ = send_io_op(messages_handle, &message_read, Some(wake_set));
        } else {
            block_on_wake_set(wake_set, None);
        }
    }
}

pub fn install() {
    let task_id = create_kernel_task(run_driver, Some("COMDEV"));

    crate::io::filesystem::install_task_dev("COM1", task_id, 0);
}

struct ComDeviceDriver {
    port_index: usize,
    next_instance: AtomicU32,
    open_instances: BTreeMap<u32, OpenFile>,

    read_list: VecDeque<PendingRead>,
}

struct OpenFile {}

struct PendingRead {
    request_id: u32,
    buffer_ptr: *mut u8,
    buffer_len: usize,
    written: usize,
}

impl ComDeviceDriver {
    pub fn new(port_index: usize) -> Self {
        Self {
            port_index,
            next_instance: AtomicU32::new(1),
            open_instances: BTreeMap::new(),
            read_list: VecDeque::new(),
        }
    }

    pub fn handle_request(&mut self, message: Message) -> Option<IoResult> {
        match DriverCommand::from_u32(message.message_type) {
            DriverCommand::OpenRaw => {
                let instance = self.next_instance.fetch_add(1, Ordering::SeqCst);
                self.open_instances.insert(instance, OpenFile {});
                Some(Ok(instance))
            }
            DriverCommand::Read => {
                let buffer_ptr = message.args[1] as *mut u8;
                let buffer_len = message.args[2] as usize;
                self.read_list.push_back(PendingRead {
                    request_id: message.unique_id,
                    buffer_ptr,
                    buffer_len,
                    written: 0,
                });
                if self.read_list.len() == 1 {
                    self.init_read().map(|(_, result)| Ok(result))
                } else {
                    None
                }
            }
            DriverCommand::Write => {
                let buffer_ptr = message.args[1] as *const u8;
                let buffer_len = message.args[2] as usize;
                let data = unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };
                with_port(self.port_index, |port| port.push(data));
                Some(Ok(buffer_len as u32))
            }
            _ => Some(Err(IoError::UnsupportedOperation)),
        }
    }

    fn init_read(&mut self) -> Option<(u32, u32)> {
        let first = match self.read_list.get_mut(0) {
            Some(pending) => pending,
            None => return None,
        };
        while first.written < first.buffer_len {
            let byte = with_port(self.port_index, |port| port.read_byte()).flatten();
            match byte {
                Some(byte) => {
                    unsafe {
                        let ptr = first.buffer_ptr.add(first.written);
                        core::ptr::write_volatile(ptr, byte);
                    }
                    first.written += 1;
                }
                None => break,
            }
        }
        if first.written < first.buffer_len {
            return None;
        }
        let completed = self.read_list.pop_front().unwrap();
        Some((completed.request_id, completed.written as u32))
    }
}
