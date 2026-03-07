use super::manager::ConsoleManager;
use crate::{
    memory::{address::VirtualAddress, shared::release_buffer},
    task::id::TaskID,
};
use alloc::collections::VecDeque;
use idos_api::io::{
    driver::DriverCommand,
    error::{IoError, IoResult},
    termios,
};
use idos_api::ipc::Message;

mod read;

pub use self::read::PendingRead;

impl ConsoleManager {
    pub fn handle_request(&mut self, sender: TaskID, message: &Message) -> Option<IoResult> {
        match DriverCommand::from_u32(message.message_type) {
            DriverCommand::OpenRaw => {
                let console_id = message.args[0] as usize;
                let console = match self.consoles.get_mut(console_id) {
                    Some(console) => console,
                    None => return Some(Err(IoError::NotFound)),
                };
                console.add_reader_task(sender);
                let handle = self.open_io.insert((console_id, 1));
                Some(Ok(handle as u32))
            }

            DriverCommand::Close => {
                let instance = message.args[0] as usize;
                match self.open_io.get_mut(instance) {
                    Some((console_id, ref_count)) => {
                        let cid = *console_id;
                        if let Some(console) = self.consoles.get_mut(cid) {
                            console.remove_reader_task(sender);
                        }
                        if *ref_count > 1 {
                            *ref_count -= 1;
                        } else {
                            self.open_io.remove(instance);
                        }
                        Some(Ok(1))
                    }
                    None => Some(Err(IoError::FileHandleInvalid)),
                }
            }

            DriverCommand::Read => {
                let request_id = message.unique_id;
                let instance = message.args[0];
                let buffer_ptr = message.args[1] as *mut u8;
                let buffer_len = message.args[2] as usize;
                let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_ptr, buffer_len) };
                self.read(request_id, instance, buffer).inspect(|_| {
                    release_buffer(VirtualAddress::new(buffer_ptr as u32), buffer_len);
                })
            }

            DriverCommand::Write => {
                let instance = message.args[0];
                let buffer_ptr = message.args[1] as *const u8;
                let buffer_len = message.args[2] as usize;
                let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };
                let result = self.write(instance, buffer);
                release_buffer(VirtualAddress::new(buffer_ptr as u32), buffer_len);
                Some(result)
            }

            DriverCommand::Share => {
                let instance = message.args[0] as usize;
                let dest_task_id = TaskID::new(message.args[1]);
                let is_move = message.args[2] != 0;
                match self.open_io.get_mut(instance) {
                    Some((console_id, ref_count)) => {
                        let cid = *console_id;
                        if is_move {
                            // Move: the source task is giving up ownership.
                            if let Some(console) = self.consoles.get_mut(cid) {
                                console.remove_reader_task(sender);
                            }
                        } else {
                            // Duplicate: both tasks will share this instance.
                            *ref_count += 1;
                        }
                        if let Some(console) = self.consoles.get_mut(cid) {
                            console.add_reader_task(dest_task_id);
                        }
                        Some(Ok(1))
                    }
                    None => Some(Err(IoError::FileHandleInvalid)),
                }
            }

            DriverCommand::Ioctl => {
                let instance = message.args[0];
                let ioctl = message.args[1];
                let arg = message.args[2];
                let arg_len = message.args[3] as usize;

                if arg_len != 0 {
                    // attempt to interpret arg as pointer to struct
                    let result = self.ioctl_struct(instance, ioctl, arg as *mut u8, arg_len);
                    release_buffer(VirtualAddress::new(arg), arg_len);
                    Some(result)
                } else {
                    // assume arg is just a u32 value
                    let result = self.ioctl(instance, ioctl, arg);
                    Some(result)
                }
            }

            _ => Some(Err(IoError::UnsupportedOperation)),
        }
    }

    /// Attempt to read from user input on a specific console.
    /// If there is any input in the flush buffer, it will be copied and the
    /// request can be immediately resolved. Otherwise it pushes the request
    /// onto a pending read queue where it will be resolved the next time
    /// input is flushed.
    pub fn read(&mut self, request_id: u32, instance: u32, buffer: &mut [u8]) -> Option<IoResult> {
        let console_id = match self.open_io.get(instance as usize) {
            Some((id, _)) => id,
            None => return Some(Err(IoError::FileHandleInvalid)),
        };

        if let Some(queue) = self.pending_reads.get_mut(*console_id) {
            if !queue.is_empty() {
                // there are other pending reads, enqueue this one
                let pending_read = PendingRead {
                    request_id,
                    buffer_start: buffer.as_mut_ptr(),
                    max_length: buffer.len(),
                };
                queue.push_back(pending_read);
                return None;
            }
        }

        let mut bytes_written = 0;
        let console = self.consoles.get_mut(*console_id).unwrap();
        let bytes_available = console.flushed_input.len();
        let to_write = bytes_available.min(buffer.len());
        if to_write > 0 {
            while bytes_written < to_write {
                if let Some(byte) = console.flushed_input.pop_front() {
                    buffer[bytes_written] = byte;
                }
                bytes_written += 1;
            }
            return Some(Ok(bytes_written as u32));
        }

        // if there was no available flushed data, enqueue the request until
        // data becomes available
        let pending_read = PendingRead {
            request_id,
            buffer_start: buffer.as_mut_ptr(),
            max_length: buffer.len(),
        };
        match self.pending_reads.get_mut(*console_id) {
            Some(queue) => queue.push_back(pending_read),
            None => {
                let mut queue = VecDeque::with_capacity(1);
                queue.push_back(pending_read);
                self.pending_reads.replace(*console_id, queue);
            }
        }

        None
    }

    /// Write text to the console window.
    pub fn write(&mut self, instance: u32, buffer: &[u8]) -> IoResult {
        let (console_id, _) = self
            .open_io
            .get(instance as usize)
            .ok_or(IoError::FileHandleInvalid)?;

        let console = self.consoles.get_mut(*console_id).unwrap();
        if !buffer.is_empty() {
            console.dirty = true;
        }
        for ch in buffer.iter() {
            console.terminal.write_character(*ch);
        }
        Ok(buffer.len() as u32)
    }

    pub fn ioctl(&mut self, instance: u32, ioctl: u32, _arg: u32) -> IoResult {
        let (console_id, _) = self
            .open_io
            .get(instance as usize)
            .ok_or(IoError::FileHandleInvalid)?;

        let console = self.consoles.get_mut(*console_id).unwrap();
        match ioctl {
            termios::TSETTEXT => {
                console.terminal.exit_graphics_mode();
                Ok(1)
            }
            _ => Err(IoError::UnsupportedOperation),
        }
    }

    pub fn ioctl_struct(
        &mut self,
        instance: u32,
        ioctl: u32,
        arg_ptr: *mut u8,
        arg_len: usize,
    ) -> IoResult {
        let (console_id, _) = self
            .open_io
            .get(instance as usize)
            .ok_or(IoError::FileHandleInvalid)?;

        let console = self.consoles.get_mut(*console_id).unwrap();
        match ioctl {
            termios::TCSETS => {
                if arg_len != core::mem::size_of::<termios::Termios>() {
                    return Err(IoError::InvalidArgument);
                }
                let termios_struct = unsafe { &*(arg_ptr as *const termios::Termios) };
                console.terminal.set_termios(termios_struct);
                Ok(1)
            }
            termios::TCGETS => {
                if arg_len != core::mem::size_of::<termios::Termios>() {
                    return Err(IoError::InvalidArgument);
                }
                let termios_struct = unsafe { &mut *(arg_ptr as *mut termios::Termios) };
                console.terminal.get_termios(termios_struct);
                Ok(1)
            }
            termios::TSETGFX => {
                if arg_len != core::mem::size_of::<termios::GraphicsMode>() {
                    return Err(IoError::InvalidArgument);
                }
                let gfx_struct = unsafe { &mut *(arg_ptr as *mut termios::GraphicsMode) };
                console.terminal.set_graphics_mode(gfx_struct);
                console.dirty = true;
                Ok(1)
            }
            termios::TGETPAL => {
                if arg_len < termios::PALETTE_SIZE {
                    return Err(IoError::InvalidArgument);
                }
                let buf = unsafe { core::slice::from_raw_parts_mut(arg_ptr, termios::PALETTE_SIZE) };
                console.terminal.get_palette_rgb(buf);
                Ok(1)
            }
            termios::TSETPAL => {
                if arg_len < termios::PALETTE_SIZE {
                    return Err(IoError::InvalidArgument);
                }
                let buf = unsafe { core::slice::from_raw_parts(arg_ptr, termios::PALETTE_SIZE) };
                console.terminal.set_palette_rgb(buf);
                Ok(1)
            }
            termios::TSETTITLE => {
                let len = arg_len.min(40);
                let buf = unsafe { core::slice::from_raw_parts(arg_ptr, len) };
                console.title.clear();
                for &b in buf {
                    if b == 0 { break; }
                    console.title.push(b as char);
                }
                console.dirty = true;
                Ok(1)
            }
            _ => Err(IoError::UnsupportedOperation),
        }
    }
}
