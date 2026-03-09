use crate::{
    collections::SlotList, files::path::Path, io::driver::kernel_driver::KernelDriver,
    memory::physical::with_allocator,
};
use alloc::string::String;
use idos_api::io::error::{IoError, IoResult};
use idos_api::io::file::FileStatus;
use spin::RwLock;

use super::{driver::AsyncIOCallback, get_all_drive_names};

struct OpenFile {
    listing: ListingType,
}

enum ListingType {
    RootDir,
    CPU,
    Drives,
    KernInfo,
    Memory,
}

impl ListingType {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "CPU" => Some(Self::CPU),
            "DRIVES" => Some(Self::Drives),
            "KERNINFO" => Some(Self::KernInfo),
            "MEMORY" => Some(Self::Memory),
            _ => None,
        }
    }
}

const ROOT_LISTING: &str = "CPU\0DRIVES\0KERNINFO\0MEMORY\0";

pub struct SysFS {
    open_files: RwLock<SlotList<OpenFile>>,
}

impl SysFS {
    pub fn new() -> Self {
        Self {
            open_files: RwLock::new(SlotList::new()),
        }
    }

    fn open_impl(&self, path: Path) -> IoResult {
        let open_file = if path.is_empty() {
            OpenFile {
                listing: ListingType::RootDir,
            }
        } else if let Some(listing_type) = ListingType::from_str(path.as_str()) {
            OpenFile {
                listing: listing_type,
            }
        } else {
            return Err(IoError::NotFound);
        };
        let index = self.open_files.write().insert(open_file);
        Ok(index as u32)
    }

    fn read_impl(&self, instance: u32, buffer: &mut [u8], offset: u32) -> IoResult {
        let mut open_files = self.open_files.write();
        let open_file = open_files
            .get_mut(instance as usize)
            .ok_or(IoError::FileHandleInvalid)?;
        let content_string = match open_file.listing {
            ListingType::RootDir => String::from(ROOT_LISTING),
            ListingType::CPU => Self::generate_cpu_content(),
            ListingType::Drives => Self::generate_drives_content(),
            ListingType::KernInfo => Self::generate_kerninfo_content(),
            ListingType::Memory => Self::generate_memory_content(),
        };
        let content_bytes = content_string.as_bytes();
        if offset >= content_bytes.len() as u32 {
            return Ok(0);
        }
        let offset_slice = &content_bytes[(offset as usize)..];
        let to_write = offset_slice.len().min(buffer.len());
        buffer[..to_write].copy_from_slice(&offset_slice[..to_write]);
        Ok(to_write as u32)
    }

    fn generate_cpu_content() -> String {
        let (user_ticks, kernel_ticks, idle_ticks) = crate::time::system::get_cpu_ticks();
        let total = user_ticks + kernel_ticks + idle_ticks;
        let ms_per_tick = crate::time::system::MS_PER_TICK as u32;
        alloc::format!(
            "CPU Usage:\nUser Time: {} ms\nKernel Time: {} ms\nIdle Time: {} ms\nTotal Ticks: {}",
            user_ticks * ms_per_tick,
            kernel_ticks * ms_per_tick,
            idle_ticks * ms_per_tick,
            total,
        )
    }

    fn generate_drives_content() -> String {
        let mut names = get_all_drive_names();
        names.push(String::from("DEV"));
        names.sort();

        names.join("\n")
    }

    fn generate_kerninfo_content() -> String {
        String::from("IDOS-NX Version 0.1\n")
    }

    fn generate_memory_content() -> String {
        let (total, free) = with_allocator(|a| (a.total_frame_count(), a.get_free_frame_count()));
        let total_memory = total * 4; // in KiB
        let free_memory = free * 4; // in KiB

        alloc::format!(
            "Total Memory: {} KiB\nFree Memory: {} KiB",
            total_memory,
            free_memory,
        )
    }

    fn stat_impl(&self, instance: u32, file_status: &mut FileStatus) -> IoResult {
        let open_files = self.open_files.read();
        let _ = open_files
            .get(instance as usize)
            .ok_or(IoError::FileHandleInvalid)?;
        file_status.byte_size = 0;
        file_status.file_type = 1;
        file_status.modification_time = 0;
        Ok(1)
    }
}

impl KernelDriver for SysFS {
    fn open(&self, path: Option<Path>, _flags: u32, _io_callback: AsyncIOCallback) -> Option<IoResult> {
        match path {
            Some(p) => Some(self.open_impl(p)),
            None => Some(Err(IoError::NotFound)),
        }
    }

    fn read(
        &self,
        instance: u32,
        buffer: &mut [u8],
        offset: u32,
        _io_callback: AsyncIOCallback,
    ) -> Option<IoResult> {
        Some(self.read_impl(instance, buffer, offset))
    }

    fn stat(
        &self,
        instance: u32,
        file_status: &mut FileStatus,
        _io_callback: AsyncIOCallback,
    ) -> Option<IoResult> {
        Some(self.stat_impl(instance, file_status))
    }

    fn close(&self, instance: u32, _io_callback: AsyncIOCallback) -> Option<IoResult> {
        if self.open_files.write().remove(instance as usize).is_none() {
            Some(Err(IoError::FileHandleInvalid))
        } else {
            Some(Ok(1))
        }
    }
}
