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

use crate::io::handle::Handle;
use crate::task::actions::{
    handle::{open_interrupt_handle, open_message_queue},
    io::{driver_io_complete, send_io_op, write_sync},
    lifecycle::create_kernel_task,
    sync::{block_on_wake_set, create_wake_set},
};

use super::serial::{port_exists, with_port};

/// IRQ 4 serves COM1 and COM3, IRQ 3 serves COM2 and COM4
const IRQ4_PORTS: [usize; 2] = [0, 2];
const IRQ3_PORTS: [usize; 2] = [1, 3];

fn needs_irq3() -> bool {
    port_exists(1) || port_exists(3)
}

/// Main event loop of the COM driver
pub fn run_driver() -> ! {
    let messages_handle = open_message_queue();
    let mut incoming_message = Message::empty();

    let wake_set = create_wake_set();
    let mut driver_impl = ComDeviceDriver::new();

    // IRQ 4 (COM1/COM3) — always present since COM1 is required
    let irq4_handle = open_interrupt_handle(4);
    let mut irq4_buf: [u8; 1] = [0];
    let mut irq4_op = AsyncOp::new(ASYNC_OP_READ, irq4_buf.as_mut_ptr() as u32, 1, 0);
    let _ = send_io_op(irq4_handle, &irq4_op, Some(wake_set));

    // IRQ 3 (COM2/COM4) — only if those ports exist
    let irq3_handle = if needs_irq3() {
        Some(open_interrupt_handle(3))
    } else {
        None
    };
    let mut irq3_buf: [u8; 1] = [0];
    let mut irq3_op = AsyncOp::new(ASYNC_OP_READ, irq3_buf.as_mut_ptr() as u32, 1, 0);
    if let Some(h) = irq3_handle {
        let _ = send_io_op(h, &irq3_op, Some(wake_set));
    }

    let mut message_read = AsyncOp::new(
        ASYNC_OP_READ,
        &mut incoming_message as *mut Message as u32,
        core::mem::size_of::<Message>() as u32,
        0,
    );
    let _ = send_io_op(messages_handle, &message_read, Some(wake_set));

    loop {
        let mut handled = false;

        if irq4_op.is_complete() {
            let _ = write_sync(irq4_handle, &[1], 0);
            driver_impl.try_complete_reads(&IRQ4_PORTS);
            irq4_op = AsyncOp::new(ASYNC_OP_READ, irq4_buf.as_mut_ptr() as u32, 1, 0);
            let _ = send_io_op(irq4_handle, &irq4_op, Some(wake_set));
            handled = true;
        }

        if let Some(h) = irq3_handle {
            if irq3_op.is_complete() {
                let _ = write_sync(h, &[1], 0);
                driver_impl.try_complete_reads(&IRQ3_PORTS);
                irq3_op = AsyncOp::new(ASYNC_OP_READ, irq3_buf.as_mut_ptr() as u32, 1, 0);
                let _ = send_io_op(h, &irq3_op, Some(wake_set));
                handled = true;
            }
        }

        if message_read.is_complete() {
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
            handled = true;
        }

        if !handled {
            block_on_wake_set(wake_set, None);
        }
    }
}

const DEVICE_NAMES: [&str; 4] = ["COM1", "COM2", "COM3", "COM4"];

pub fn install() {
    let task_id = create_kernel_task(run_driver, Some("COMDEV"));

    for i in 0..4 {
        if super::serial::port_exists(i) {
            crate::io::filesystem::install_task_dev(DEVICE_NAMES[i], task_id, i as u32);
        }
    }
}

struct ComDeviceDriver {
    next_instance: AtomicU32,
    open_instances: BTreeMap<u32, OpenFile>,

    read_list: VecDeque<PendingRead>,
}

struct OpenFile {
    port_index: usize,
}

struct PendingRead {
    port_index: usize,
    request_id: u32,
    buffer_ptr: *mut u8,
    buffer_len: usize,
    written: usize,
}

impl ComDeviceDriver {
    pub fn new() -> Self {
        Self {
            next_instance: AtomicU32::new(1),
            open_instances: BTreeMap::new(),
            read_list: VecDeque::new(),
        }
    }

    pub fn handle_request(&mut self, message: Message) -> Option<IoResult> {
        match DriverCommand::from_u32(message.message_type) {
            DriverCommand::OpenRaw => {
                let port_index = message.args[0] as usize;
                let instance = self.next_instance.fetch_add(1, Ordering::SeqCst);
                self.open_instances.insert(instance, OpenFile { port_index });
                Some(Ok(instance))
            }
            DriverCommand::Read => {
                let instance = message.args[0];
                let port_index = match self.open_instances.get(&instance) {
                    Some(file) => file.port_index,
                    None => return Some(Err(IoError::FileHandleInvalid)),
                };
                let buffer_ptr = message.args[1] as *mut u8;
                let buffer_len = message.args[2] as usize;
                self.read_list.push_back(PendingRead {
                    port_index,
                    request_id: message.unique_id,
                    buffer_ptr,
                    buffer_len,
                    written: 0,
                });
                // Try to satisfy immediately if data is already buffered
                self.try_complete_reads(&[port_index]);
                None
            }
            DriverCommand::Write => {
                let instance = message.args[0];
                let port_index = match self.open_instances.get(&instance) {
                    Some(file) => file.port_index,
                    None => return Some(Err(IoError::FileHandleInvalid)),
                };
                let buffer_ptr = message.args[1] as *const u8;
                let buffer_len = message.args[2] as usize;
                let data = unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };
                with_port(port_index, |port| port.push(data));
                Some(Ok(buffer_len as u32))
            }
            _ => Some(Err(IoError::UnsupportedOperation)),
        }
    }

    fn try_complete_reads(&mut self, ports: &[usize]) {
        let mut i = 0;
        while i < self.read_list.len() {
            let pending = &mut self.read_list[i];
            if !ports.contains(&pending.port_index) {
                i += 1;
                continue;
            }
            while pending.written < pending.buffer_len {
                let byte = with_port(pending.port_index, |port| port.read_byte()).flatten();
                match byte {
                    Some(byte) => {
                        unsafe {
                            let ptr = pending.buffer_ptr.add(pending.written);
                            core::ptr::write_volatile(ptr, byte);
                        }
                        pending.written += 1;
                    }
                    None => break,
                }
            }
            // Complete the read if we got any data, even if the buffer
            // isn't full. This allows streaming — callers get partial
            // reads as data arrives rather than blocking until full.
            if pending.written > 0 {
                let completed = self.read_list.remove(i).unwrap();
                driver_io_complete(completed.request_id, Ok(completed.written as u32));
            } else {
                i += 1;
            }
        }
    }
}
