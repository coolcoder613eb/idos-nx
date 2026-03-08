#![no_std]
#![no_main]

extern crate alloc;
extern crate idos_sdk;

mod controller;
mod dma;
mod geometry;
mod port;

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

use controller::{Command, ControllerError, DriveSelect, DriveType, FloppyController};
use dma::DmaChannelRegisters;
use geometry::ChsGeometry;
use idos_api::{
    io::{
        driver::DriverCommand,
        error::{IoError, IoResult},
        read_message_op,
        sync::{close_sync, write_sync},
        AsyncOp, Handle, Message, ASYNC_OP_READ,
    },
    syscall::{
        exec::yield_coop,
        io::{
            append_io_op, block_on_wake_set, create_message_queue_handle, create_wake_set,
            driver_io_complete, open_irq_handle, register_dev,
        },
        memory::map_dma_memory,
    },
};
use idos_sdk::log::SysLogger;

/// Polls the IRQ handle until an interrupt fires, then acknowledges it.
/// Used both during init and during normal I/O operations.
fn wait_irq(interrupt_handle: Handle, irq_buf: &mut [u8; 1], wake_set: Handle) {
    let irq_read = AsyncOp::new(ASYNC_OP_READ, irq_buf.as_mut_ptr() as u32, 1, 0);
    append_io_op(interrupt_handle, &irq_read, Some(wake_set));
    loop {
        if irq_read.is_complete() {
            let _ = write_sync(interrupt_handle, &[], 0);
            return;
        }
        block_on_wake_set(wake_set, Some(100));
    }
}

struct FloppyDeviceDriver {
    controller: FloppyController,
    dma_vaddr: u32,
    dma_paddr: u32,
    attached: [DriveType; 2],
    selected_drive: Option<DriveSelect>,

    interrupt_handle: Handle,
    irq_wake_set: Handle,
    irq_buf: [u8; 1],

    next_instance: AtomicU32,
    open_instances: BTreeMap<u32, OpenFile>,
}

impl FloppyDeviceDriver {
    fn new(interrupt_handle: Handle) -> Self {
        let (dma_vaddr, dma_paddr) = map_dma_memory(0x1000).unwrap();
        let irq_wake_set = create_wake_set();

        Self {
            controller: FloppyController::new(),
            dma_vaddr,
            dma_paddr,
            attached: [DriveType::None, DriveType::None],
            selected_drive: None,

            interrupt_handle,
            irq_wake_set,
            irq_buf: [0],

            next_instance: AtomicU32::new(1),
            open_instances: BTreeMap::new(),
        }
    }

    fn wait_irq(&mut self) {
        wait_irq(self.interrupt_handle, &mut self.irq_buf, self.irq_wake_set);
    }

    fn set_device(&mut self, index: usize, drive_type: DriveType) {
        self.attached[index] = drive_type;
    }

    fn init(&mut self) -> Result<(), ControllerError> {
        let mut response = [0];

        self.controller.send_command(Command::Version, &[])?;
        self.controller.get_response(&mut response)?;
        if response[0] != 0x90 {
            return Err(ControllerError::UnsupportedController);
        }
        self.controller
            .send_command(Command::Configure, &[0, 0x57, 0])?;
        self.controller.send_command(Command::Lock, &[])?;
        self.controller.get_response(&mut response)?;
        if response[0] != 0x10 {
            return Err(ControllerError::InvalidResponse);
        }

        self.reset()?;

        match self.attached[0] {
            DriveType::None => (),
            _ => {
                self.controller.ensure_motor_on(DriveSelect::Primary);
                self.recalibrate(DriveSelect::Primary)?;
            }
        }
        match self.attached[1] {
            DriveType::None => (),
            _ => {
                self.controller.ensure_motor_on(DriveSelect::Secondary);
                self.recalibrate(DriveSelect::Secondary)?;
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), ControllerError> {
        self.controller.dor_write(0);
        yield_coop();
        self.controller.dor_write(0x0c);
        self.wait_irq();

        let mut sense = [0, 0];
        for _ in 0..4 {
            self.controller
                .send_command(Command::SenseInterrupt, &[])?;
            self.controller.get_response(&mut sense)?;
        }

        self.controller.ccr_write(0);
        self.controller
            .send_command(Command::Specify, &[8 << 4, 5 << 1])?;

        Ok(())
    }

    fn select_drive(&mut self, drive: DriveSelect) {
        if self.selected_drive == Some(drive) {
            return;
        }
        let dor = self.controller.dor_read();
        let flag = match drive {
            DriveSelect::Primary => 0,
            DriveSelect::Secondary => 1,
        };
        self.controller.dor_write((dor & 0xfc) | flag);
        self.selected_drive = Some(drive);
    }

    fn recalibrate(&mut self, drive: DriveSelect) -> Result<(), ControllerError> {
        self.select_drive(drive);
        for _ in 0..2 {
            self.controller
                .send_command(Command::Recalibrate, &[0])?;
            self.wait_irq();

            let mut st0 = [0, 0];
            self.controller
                .send_command(Command::SenseInterrupt, &[])?;
            self.controller.get_response(&mut st0)?;

            if st0[0] & 0x20 == 0x20 {
                break;
            }
        }
        Ok(())
    }

    fn send_command(&mut self, command: Command, params: &[u8]) -> Result<(), ControllerError> {
        if self.controller.get_status() & 0xc0 != 0x80 {
            self.reset()?;
        }
        self.controller.send_command(command, params)
    }

    fn dma_transfer(
        &mut self,
        command: Command,
        drive: DriveSelect,
        chs: ChsGeometry,
    ) -> Result<(), ControllerError> {
        self.select_drive(drive);
        let drive_number = match drive {
            DriveSelect::Primary => 0,
            DriveSelect::Secondary => 1,
        };
        self.send_command(
            command,
            &[
                (chs.head << 2) as u8 | drive_number,
                chs.cylinder as u8,
                chs.head as u8,
                chs.sector as u8,
                2,
                18,
                0x1b,
                0xff,
            ],
        )?;

        self.wait_irq();
        let mut response = [0, 0, 0, 0, 0, 0, 0];
        self.controller.get_response(&mut response)?;
        Ok(())
    }

    fn get_dma_buffer(&self) -> &mut [u8] {
        unsafe {
            let buffer_ptr = self.dma_vaddr as *mut u8;
            core::slice::from_raw_parts_mut(buffer_ptr, 0x1000)
        }
    }

    fn dma_prepare(&self, sector_count: usize, dma_mode: u8) {
        let dma_channel = DmaChannelRegisters::for_channel(2);
        dma_channel.set_address(self.dma_paddr);
        dma_channel.set_count((sector_count * geometry::SECTOR_SIZE) as u32 - 1);
        dma_channel.set_mode(dma_mode);
    }

    fn open(&mut self, sub_driver: u32) -> IoResult {
        match self.attached.get(sub_driver as usize) {
            None => return Err(IoError::NotFound),
            _ => (),
        }
        let drive = match sub_driver {
            1 => DriveSelect::Secondary,
            _ => DriveSelect::Primary,
        };
        let file = OpenFile { drive };
        let instance = self.next_instance.fetch_add(1, Ordering::SeqCst);
        self.open_instances.insert(instance, file);
        Ok(instance)
    }

    fn read_blocks(&mut self, instance: u32, buffer: &mut [u8], offset: u32) -> IoResult {
        if buffer.is_empty() {
            return Ok(0);
        }

        let drive_select = match self.open_instances.get(&instance) {
            Some(file) => file.drive,
            None => return Err(IoError::FileHandleInvalid),
        };

        let mut buf_offset = 0usize;
        let mut position = offset as usize;
        let total = buffer.len();

        while buf_offset < total {
            let first_sector = position / geometry::SECTOR_SIZE;
            let read_offset = position % geometry::SECTOR_SIZE;
            let remaining = total - buf_offset;
            let last_sector = (position + remaining - 1) / geometry::SECTOR_SIZE;
            let mut sector_count = last_sector - first_sector + 1;

            let max_sectors = 0x1000 / geometry::SECTOR_SIZE;
            if sector_count > max_sectors {
                sector_count = max_sectors;
            }

            let chs = ChsGeometry::from_lba(first_sector);
            let sectors_left_on_track = geometry::SECTORS_PER_TRACK + 1 - chs.sector;
            if sector_count > sectors_left_on_track {
                sector_count = sectors_left_on_track;
            }

            self.dma_prepare(sector_count, 0x56);
            self.dma_transfer(Command::ReadData, drive_select, chs)
                .map_err(|_| IoError::FileSystemError)?;

            let dma_buffer = self.get_dma_buffer();
            let available = sector_count * geometry::SECTOR_SIZE - read_offset;
            let copy_len = remaining.min(available);

            buffer[buf_offset..buf_offset + copy_len]
                .copy_from_slice(&dma_buffer[read_offset..read_offset + copy_len]);

            buf_offset += copy_len;
            position += copy_len;
        }

        Ok(total as u32)
    }

    fn write_blocks(&mut self, instance: u32, buffer: &[u8], offset: u32) -> IoResult {
        if buffer.is_empty() {
            return Ok(0);
        }

        let drive_select = match self.open_instances.get(&instance) {
            Some(file) => file.drive,
            None => return Err(IoError::FileHandleInvalid),
        };

        let mut buf_offset = 0usize;
        let mut position = offset as usize;
        let total = buffer.len();

        while buf_offset < total {
            let first_sector = position / geometry::SECTOR_SIZE;
            let write_offset = position % geometry::SECTOR_SIZE;
            let remaining = total - buf_offset;
            let last_sector = (position + remaining - 1) / geometry::SECTOR_SIZE;
            let mut sector_count = last_sector - first_sector + 1;

            let max_sectors = 0x1000 / geometry::SECTOR_SIZE;
            if sector_count > max_sectors {
                sector_count = max_sectors;
            }

            let chs = ChsGeometry::from_lba(first_sector);
            let sectors_left_on_track = geometry::SECTORS_PER_TRACK + 1 - chs.sector;
            if sector_count > sectors_left_on_track {
                sector_count = sectors_left_on_track;
            }

            let available = sector_count * geometry::SECTOR_SIZE - write_offset;
            let copy_len = remaining.min(available);

            // If writing a partial sector, read-modify-write
            if write_offset != 0 || copy_len % geometry::SECTOR_SIZE != 0 {
                self.dma_prepare(sector_count, 0x56);
                self.dma_transfer(Command::ReadData, drive_select, chs)
                    .map_err(|_| IoError::FileSystemError)?;
            }

            let dma_buffer = self.get_dma_buffer();
            dma_buffer[write_offset..write_offset + copy_len]
                .copy_from_slice(&buffer[buf_offset..buf_offset + copy_len]);

            self.dma_prepare(sector_count, 0x5A);
            self.dma_transfer(Command::WriteData, drive_select, chs)
                .map_err(|_| IoError::FileSystemError)?;

            buf_offset += copy_len;
            position += copy_len;
        }

        Ok(total as u32)
    }

    fn close(&mut self, instance: u32) -> IoResult {
        self.open_instances
            .remove(&instance)
            .map(|_| 1)
            .ok_or(IoError::FileHandleInvalid)
    }
}

struct OpenFile {
    drive: DriveSelect,
}

#[no_mangle]
pub extern "C" fn main() {
    // Handle 0 = response pipe writer (transferred by kernel)
    let response_writer = Handle::new(0);

    let mut log = SysLogger::new("FDDEV");

    // argv[0] = path, argv[1] = IRQ number
    let mut args = idos_sdk::env::args();
    let _path = args.next();
    let irq_str = args.next().unwrap_or("6");
    let irq = parse_u8(irq_str);

    let interrupt_handle = open_irq_handle(irq);
    let mut driver = FloppyDeviceDriver::new(interrupt_handle);

    // Detect drives from CMOS
    let mut fd_count: u8 = 0;
    let drives = DriveType::read_cmos();
    for (i, drive_type) in drives.iter().enumerate() {
        if let DriveType::None = drive_type {
            continue;
        }
        driver.set_device(i, DriveType::from_cmos_value(match drive_type {
            DriveType::None => 0,
            DriveType::Capacity360K => 1,
            DriveType::Capacity720K => 2,
            DriveType::Capacity1200K => 3,
            DriveType::Capacity1440K => 4,
            DriveType::Capacity2880K => 5,
        }));
        fd_count += 1;
        let dev_name_bytes: [u8; 3] = [b'F', b'D', b'0' + fd_count];
        let dev_name = unsafe { core::str::from_utf8_unchecked(&dev_name_bytes) };
        log.log_fmt(format_args!("Registered DEV:\\{}", dev_name));
        register_dev(dev_name);
    }

    // Initialize controller (needs working IRQs for reset + recalibrate)
    match driver.init() {
        Ok(_) => log.log("Floppy controller initialized"),
        Err(_) => log.log("Failed to init floppy controller"),
    }

    // Signal ready with drive count
    let _ = write_sync(response_writer, &[fd_count], 0);
    let _ = close_sync(response_writer);

    // Enter message loop
    let messages_handle = create_message_queue_handle();
    let wake_set = create_wake_set();

    let mut incoming_message = Message::empty();
    let mut message_read = read_message_op(&mut incoming_message);
    append_io_op(messages_handle, &message_read, Some(wake_set));

    loop {
        if message_read.is_complete() {
            let request_id = incoming_message.unique_id;
            let result = handle_driver_request(&mut driver, &incoming_message);
            driver_io_complete(request_id, result);

            message_read = read_message_op(&mut incoming_message);
            append_io_op(messages_handle, &message_read, Some(wake_set));
        } else {
            block_on_wake_set(wake_set, None);
        }
    }
}

fn parse_u8(s: &str) -> u8 {
    let mut result: u8 = 0;
    for b in s.bytes() {
        if b >= b'0' && b <= b'9' {
            result = result.wrapping_mul(10).wrapping_add(b - b'0');
        }
    }
    result
}

fn handle_driver_request(driver: &mut FloppyDeviceDriver, message: &Message) -> IoResult {
    match DriverCommand::from_u32(message.message_type) {
        DriverCommand::OpenRaw => {
            let sub_driver = message.args[0];
            driver.open(sub_driver)
        }
        DriverCommand::Read => {
            let instance = message.args[0];
            let buffer_ptr = message.args[1] as *mut u8;
            let buffer_len = message.args[2] as usize;
            let offset = message.args[3];
            let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_ptr, buffer_len) };
            driver.read_blocks(instance, buffer, offset)
        }
        DriverCommand::Write => {
            let instance = message.args[0];
            let buffer_ptr = message.args[1] as *const u8;
            let buffer_len = message.args[2] as usize;
            let offset = message.args[3];
            let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };
            driver.write_blocks(instance, buffer, offset)
        }
        DriverCommand::Close => {
            let instance = message.args[0];
            driver.close(instance)
        }
        _ => Err(IoError::UnsupportedOperation),
    }
}
