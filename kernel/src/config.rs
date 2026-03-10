use alloc::string::String;
use alloc::vec::Vec;

use crate::log::TaggedLogger;
use crate::task::actions::handle::create_file_handle;
use crate::task::actions::io::{close_sync, open_sync, read_sync};

const LOGGER: TaggedLogger = TaggedLogger::new("CONFIG", 33);

#[derive(Debug)]
pub enum Directive {
    /// Launch a built-in kernel driver by name (ps2, ata, ethernet)
    Driver(String),
    /// Launch a userspace ISA driver ELF: path and IRQ number
    Isa { path: String, irq: u8 },
    /// Find PCI device by vendor:device, launch userspace ELF, optionally enable bus mastering
    Pci {
        vendor_id: u16,
        device_id: u16,
        path: String,
        busmaster: bool,
    },
    /// Mount a filesystem: drive_letter, fs_type, device_name
    Mount {
        drive_letter: String,
        fs_type: String,
        device: String,
    },
    /// Register a graphics driver ELF
    Graphics(String),
    /// Initialize the console manager
    Console,
    /// Start the network stack
    Net,
    /// Set timezone offset from UTC in minutes (e.g. -420 for UTC-7, 60 for UTC+1)
    Timezone(i32),
}

/// Read and parse `C:\DRIVERS.CFG`, returning a list of directives.
pub fn read_config(path: &str) -> Vec<Directive> {
    let handle = create_file_handle();
    match open_sync(handle, path, 0) {
        Ok(_) => {}
        Err(e) => {
            LOGGER.log(format_args!("Failed to open {}: {:?}", path, e));
            return Vec::new();
        }
    }

    // Read the entire file in chunks
    let mut contents = Vec::new();
    let mut buf = [0u8; 512];
    let mut offset = 0u32;
    loop {
        match read_sync(handle, &mut buf, offset) {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    break;
                }
                contents.extend_from_slice(&buf[..bytes_read as usize]);
                offset += bytes_read;
            }
            Err(_) => break,
        }
    }
    let _ = close_sync(handle);

    let text = match core::str::from_utf8(&contents) {
        Ok(s) => s,
        Err(_) => {
            LOGGER.log(format_args!("{} contains invalid UTF-8", path));
            return Vec::new();
        }
    };

    parse_config(text)
}

fn parse_config(text: &str) -> Vec<Directive> {
    let mut directives = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "driver" => {
                if parts.len() < 2 {
                    LOGGER.log(format_args!("Config: 'driver' missing name: {}", line));
                    continue;
                }
                directives.push(Directive::Driver(String::from(parts[1])));
            }
            "isa" => {
                if parts.len() < 3 {
                    LOGGER.log(format_args!("Config: 'isa' missing args: {}", line));
                    continue;
                }
                let path = parts[1];
                let irq = match u8::from_str_radix(parts[2], 10) {
                    Ok(v) => v,
                    Err(_) => {
                        LOGGER.log(format_args!("Config: invalid IRQ '{}': {}", parts[2], line));
                        continue;
                    }
                };
                directives.push(Directive::Isa {
                    path: String::from(path),
                    irq,
                });
            }
            "pci" => {
                if parts.len() < 3 {
                    LOGGER.log(format_args!("Config: 'pci' missing args: {}", line));
                    continue;
                }
                let ids = parts[1];
                let path = parts[2];
                let busmaster = parts.get(3).map_or(false, |&s| s == "busmaster");

                match parse_vendor_device(ids) {
                    Some((vendor_id, device_id)) => {
                        directives.push(Directive::Pci {
                            vendor_id,
                            device_id,
                            path: String::from(path),
                            busmaster,
                        });
                    }
                    None => {
                        LOGGER.log(format_args!("Config: invalid PCI ID '{}': {}", ids, line));
                    }
                }
            }
            "mount" => {
                if parts.len() < 4 {
                    LOGGER.log(format_args!("Config: 'mount' missing args: {}", line));
                    continue;
                }
                directives.push(Directive::Mount {
                    drive_letter: String::from(parts[1]),
                    fs_type: String::from(parts[2]),
                    device: String::from(parts[3]),
                });
            }
            "graphics" => {
                if parts.len() < 2 {
                    LOGGER.log(format_args!("Config: 'graphics' missing path: {}", line));
                    continue;
                }
                directives.push(Directive::Graphics(String::from(parts[1])));
            }
            "console" => {
                directives.push(Directive::Console);
            }
            "net" => {
                directives.push(Directive::Net);
            }
            "timezone" => {
                if parts.len() < 2 {
                    LOGGER.log(format_args!("Config: 'timezone' missing offset: {}", line));
                    continue;
                }
                match parse_i32(parts[1]) {
                    Some(offset) => {
                        directives.push(Directive::Timezone(offset));
                    }
                    None => {
                        LOGGER.log(format_args!(
                            "Config: invalid timezone offset '{}': {}",
                            parts[1], line
                        ));
                    }
                }
            }
            _ => {
                LOGGER.log(format_args!("Config: unknown directive: {}", line));
            }
        }
    }

    directives
}

/// Parse a signed decimal integer (e.g. "-420", "60")
fn parse_i32(s: &str) -> Option<i32> {
    let (negative, digits) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    };
    let abs = u32::from_str_radix(digits, 10).ok()?;
    if negative {
        Some(-(abs as i32))
    } else {
        Some(abs as i32)
    }
}

/// Parse "XXXX:YYYY" hex vendor:device ID pair
fn parse_vendor_device(s: &str) -> Option<(u16, u16)> {
    let mut parts = s.split(':');
    let vendor_str = parts.next()?;
    let device_str = parts.next()?;
    let vendor_id = u16::from_str_radix(vendor_str, 16).ok()?;
    let device_id = u16::from_str_radix(device_str, 16).ok()?;
    Some((vendor_id, device_id))
}
