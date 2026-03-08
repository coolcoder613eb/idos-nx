use crate::port::Port;
use idos_api::syscall::{exec::yield_coop, time::sleep_ms};

pub struct FloppyController {
    pub motor_on: [bool; 2],
}

impl FloppyController {
    pub fn new() -> Self {
        Self {
            motor_on: [false, false],
        }
    }

    pub fn get_status(&self) -> u8 {
        Port::new(0x3f4).read_u8()
    }

    pub fn dor_read(&self) -> u8 {
        Port::new(0x3f2).read_u8()
    }

    pub fn dor_write(&self, value: u8) {
        Port::new(0x3f2).write_u8(value);
    }

    pub fn ccr_write(&self, value: u8) {
        Port::new(0x3f7).write_u8(value);
    }

    fn fifo_read(&self) -> u8 {
        Port::new(0x3f5).read_u8()
    }

    fn fifo_write(&self, value: u8) {
        Port::new(0x3f5).write_u8(value);
    }

    pub fn ensure_motor_on(&mut self, drive: DriveSelect) {
        let dor = self.dor_read();
        let (index, flag) = match drive {
            DriveSelect::Primary => (0, 0x10),
            DriveSelect::Secondary => (1, 0x20),
        };
        self.dor_write(dor | flag);
        sleep_ms(300);
        self.motor_on[index] = true;
    }

    fn wait_for_rqm(&self) -> Result<(), ControllerError> {
        let mut retry_count = 10;
        let mut ready = false;
        while !ready && retry_count > 0 {
            retry_count -= 1;
            ready = self.get_status() & 0x80 == 0x80;
            if !ready {
                yield_coop();
            }
        }
        if ready {
            Ok(())
        } else {
            Err(ControllerError::ReadyTimeout)
        }
    }

    pub fn send_command(&self, command: Command, params: &[u8]) -> Result<(), ControllerError> {
        self.fifo_write(command as u8);

        for param in params {
            self.wait_for_rqm()?;
            if self.get_status() & 0x40 != 0 {
                return Err(ControllerError::NotReadyForParam);
            }
            self.fifo_write(*param);
        }

        self.wait_for_rqm()?;
        Ok(())
    }

    pub fn get_response(&self, response: &mut [u8]) -> Result<usize, ControllerError> {
        self.wait_for_rqm()?;
        let mut has_response = self.get_status() & 0x50 == 0x50;
        let mut response_index = 0;
        while has_response {
            if let Some(entry) = response.get_mut(response_index) {
                *entry = self.fifo_read();
                response_index += 1;
            }
            self.wait_for_rqm()?;
            has_response = self.get_status() & 0x50 == 0x50;
        }

        if self.get_status() & 0xd0 == 0x80 {
            Ok(response_index)
        } else {
            Err(ControllerError::InvalidResponse)
        }
    }
}

#[repr(u8)]
pub enum Command {
    ReadData = 0x06 | 0x40,
    Specify = 0x03,
    WriteData = 0x05 | 0x40,
    Recalibrate = 0x07,
    SenseInterrupt = 0x08,
    Version = 0x10,
    Configure = 0x13,
    Lock = 0x94,
}

#[derive(Debug)]
pub enum ControllerError {
    InvalidResponse,
    NotReadyForParam,
    ReadyTimeout,
    UnsupportedController,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum DriveSelect {
    Primary,
    Secondary,
}

pub enum DriveType {
    None,
    Capacity360K,
    Capacity1200K,
    Capacity720K,
    Capacity1440K,
    Capacity2880K,
}

impl DriveType {
    pub fn from_cmos_value(value: u8) -> Self {
        match value {
            1 => Self::Capacity360K,
            2 => Self::Capacity720K,
            3 => Self::Capacity1200K,
            4 => Self::Capacity1440K,
            5 => Self::Capacity2880K,
            _ => Self::None,
        }
    }

    pub fn read_cmos() -> [Self; 2] {
        Port::new(0x70).write_u8(0x10);
        let cmos_value = Port::new(0x71).read_u8();
        let primary = DriveType::from_cmos_value(cmos_value >> 4);
        let secondary = DriveType::from_cmos_value(cmos_value & 0x0f);

        [primary, secondary]
    }
}

impl core::fmt::Display for DriveType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DriveType::None => f.write_str("Unavailable"),
            DriveType::Capacity360K => f.write_str("360KB 5.25 Disk"),
            DriveType::Capacity1200K => f.write_str("1.2MB 5.25 Disk"),
            DriveType::Capacity720K => f.write_str("720KB 3.5 Disk"),
            DriveType::Capacity1440K => f.write_str("1.44MB 3.5 Disk"),
            DriveType::Capacity2880K => f.write_str("2.88MB 3.5 Disk"),
        }
    }
}
