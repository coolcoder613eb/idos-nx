#![no_std]
#![no_main]

extern crate idos_api;
extern crate idos_sdk;

mod controller;
mod driver;

use core::sync::atomic::AtomicU32;

use controller::E1000Controller;
use driver::EthernetDriver;
use idos_api::{
    io::{
        read_message_op,
        sync::{read_sync, write_sync, close_sync},
        Handle, Message, AsyncOp, ASYNC_OP_READ,
    },
    io::driver::DriverCommand,
    io::error::{IoError, IoResult},
    syscall::{
        io::{
            append_io_op, block_on_wake_set, create_message_queue_handle, create_wake_set,
            open_irq_handle, register_dev, register_network_device,
        },
        memory::map_memory,
    },
    syscall::pci::PciDeviceQuery,
};

struct EthernetDevice {
    driver: EthernetDriver,
    next_instance: AtomicU32,
    pending_read: Option<(*mut u8, usize, u32)>,
}

impl EthernetDevice {
    fn new(driver: EthernetDriver) -> Self {
        Self {
            driver,
            next_instance: AtomicU32::new(1),
            pending_read: None,
        }
    }

    fn handle_request(&mut self, message: Message) -> Option<IoResult> {
        match DriverCommand::from_u32(message.message_type) {
            DriverCommand::OpenRaw => Some(self.open()),
            DriverCommand::Close => Some(self.close()),
            DriverCommand::Read => {
                let buffer_ptr = message.args[1] as *mut u8;
                let buffer_len = message.args[2] as usize;
                if let Some(response) = self.read(buffer_ptr, buffer_len) {
                    return Some(response);
                }
                self.pending_read
                    .replace((buffer_ptr, buffer_len, message.unique_id));
                None
            }
            DriverCommand::Write => {
                let buffer_ptr = message.args[1] as *const u8;
                let buffer_len = message.args[2] as usize;
                Some(self.write(buffer_ptr, buffer_len))
            }
            _ => Some(Err(IoError::UnsupportedOperation)),
        }
    }

    fn open(&mut self) -> IoResult {
        use core::sync::atomic::Ordering;
        let instance = self.next_instance.fetch_add(1, Ordering::SeqCst);
        Ok(instance)
    }

    fn close(&mut self) -> IoResult {
        Ok(1)
    }

    fn read(&mut self, buffer_ptr: *mut u8, buffer_len: usize) -> Option<IoResult> {
        let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_ptr, buffer_len) };
        let rx_buffer = self.driver.get_next_rx_buffer()?;
        let read_len = rx_buffer.len().min(buffer.len());
        buffer[..read_len].copy_from_slice(&rx_buffer[..read_len]);
        self.driver.mark_current_rx_read();
        Some(Ok(read_len as u32))
    }

    fn write(&mut self, buffer_ptr: *const u8, buffer_len: usize) -> IoResult {
        let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };
        Ok(self.driver.tx(buffer) as u32)
    }
}

fn send_response(request_id: u32, result: IoResult) {
    idos_api::syscall::io::driver_io_complete(request_id, result);
}

#[no_mangle]
pub extern "C" fn main() {
    let args_reader = Handle::new(0);
    let response_writer = Handle::new(1);

    // Read PciDeviceQuery from args pipe
    let mut query = PciDeviceQuery::new(0, 0);
    let query_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            &mut query as *mut PciDeviceQuery as *mut u8,
            core::mem::size_of::<PciDeviceQuery>(),
        )
    };
    let _ = read_sync(args_reader, query_bytes, 0);

    let bar0 = query.bar[0];
    let irq = query.irq;

    // Map MMIO region
    let mmio_vaddr = map_memory(None, 0x10000, Some(bar0)).unwrap();

    let controller = E1000Controller::new(mmio_vaddr);
    let mac = controller.get_mac_address();

    let eth = EthernetDriver::new(controller);
    let mut driver_impl = EthernetDevice::new(eth);

    // Register as device driver
    register_dev("ETH");

    // Register with network stack
    register_network_device("DEV:\\ETH", &mac);

    // Open IRQ handle
    let interrupt_handle = open_irq_handle(irq);
    let messages_handle = create_message_queue_handle();
    let wake_set = create_wake_set();

    let mut incoming_message = Message::empty();
    let mut interrupt_ready: [u8; 1] = [0; 1];

    let mut message_read = read_message_op(&mut incoming_message);
    append_io_op(messages_handle, &message_read, Some(wake_set));
    let mut interrupt_read = AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
    append_io_op(interrupt_handle, &interrupt_read, Some(wake_set));

    // Signal ready
    let _ = write_sync(response_writer, &[0], 0);
    let _ = close_sync(response_writer);

    loop {
        if interrupt_read.is_complete() {
            let cause = driver_impl.driver.get_interrupt_cause();
            if cause != 0 {
                if driver_impl.driver.get_next_rx_buffer().is_some() {
                    if let Some((buffer_ptr, buffer_len, unique_id)) =
                        driver_impl.pending_read.take()
                    {
                        if let Some(response) = driver_impl.read(buffer_ptr, buffer_len) {
                            send_response(unique_id, response);
                        } else {
                            driver_impl.pending_read = Some((buffer_ptr, buffer_len, unique_id));
                        }
                    }
                }
            }

            let _ = write_sync(interrupt_handle, &[], 0);

            interrupt_read =
                AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
            append_io_op(interrupt_handle, &interrupt_read, Some(wake_set));
        } else if message_read.is_complete() {
            match driver_impl.handle_request(incoming_message) {
                Some(response) => send_response(incoming_message.unique_id, response),
                None => (),
            }

            message_read = read_message_op(&mut incoming_message);
            append_io_op(messages_handle, &message_read, Some(wake_set));
        } else {
            block_on_wake_set(wake_set, None);
        }
    }
}
