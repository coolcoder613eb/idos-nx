use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::io::handle::Handle;

pub fn _kprint(args: fmt::Arguments) {
    use crate::hardware::com::serial::with_port;
    match with_port(0, |port| port.write_fmt(args)) {
        Some(Ok(())) => {}
        _ => {
            // Fallback for early boot before init_port has been called
            let mut serial = crate::hardware::com::serial::SerialPort::new(0x3f8);
            serial.write_fmt(args).unwrap();
        }
    }
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::log::_kprint(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}

pub struct BufferedLogger {
    log_lines: Vec<String>,
}

impl BufferedLogger {
    pub fn new() -> Self {
        Self {
            log_lines: Vec::new(),
        }
    }

    pub fn log(&mut self, message: &str) {
        use alloc::string::ToString;
        self.log_lines.push(message.to_string());
        kprintln!("LOG: {}", message);
    }

    pub fn flush_to_file(&mut self, handle: Handle) {
        for line in &self.log_lines {
            let _ = crate::task::actions::io::write_sync(handle, line.as_bytes(), 0);
        }
        self.log_lines.clear();
    }
}

pub struct TaggedLogger {
    tag: [u8; 8],
    color: u8,
}

impl TaggedLogger {
    pub const fn new(tag_str: &str, color: u8) -> Self {
        let tag_bytes = tag_str.as_bytes();
        let mut tag = [0x20u8; 8];
        let copy_len = if tag_bytes.len() < 8 {
            tag_bytes.len()
        } else {
            8
        };
        // copy from slice is not const stable yet
        let mut i = 0;
        while i < copy_len {
            tag[i] = tag_bytes[i];
            i += 1;
        }
        TaggedLogger { tag, color }
    }

    pub fn tag_bytes(&self) -> [u8; 8] {
        self.tag
    }

    pub fn log(&self, args: fmt::Arguments) {
        kprint!(
            "\x1b[{}m{}\x1b[0m: ",
            self.color,
            core::str::from_utf8(&self.tag).unwrap(),
        );
        _kprint(args);
        kprint!("\n");
    }
}
