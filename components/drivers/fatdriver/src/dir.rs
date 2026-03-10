use alloc::{string::String, vec::Vec};

use crate::disk::{DiskAccess, DiskIO};
use crate::table::AllocationTable;

/// On-disk representation of a file or subdirectory
#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct DirEntry {
    /// Short filename
    file_name: [u8; 8],
    /// File extension
    ext: [u8; 3],
    /// File attributes
    attributes: u8,
    /// Reserved byte used for various nonstandard things
    nonstandard_attributes: u8,
    /// Fine resolution of creation time, in 10ms units. Ranges from 0-199
    fine_create_time: u8,
    /// File creation time
    creation_time: FileTime,
    /// File creation date
    creation_date: FileDate,
    /// Last access date
    access_date: FileDate,
    /// Extended attributes
    extended_attributes: u16,
    /// Last modified time
    last_modify_time: FileTime,
    /// Last modified date
    last_modify_date: FileDate,
    /// First cluster of file data
    first_file_cluster: u16,
    /// File size in bytes
    byte_size: u32,
}

impl DirEntry {
    pub fn new() -> Self {
        Self {
            file_name: [0x20; 8],
            ext: [0x20; 3],
            attributes: 0,
            nonstandard_attributes: 0,
            fine_create_time: 0,
            creation_time: FileTime(0),
            creation_date: FileDate(0),
            access_date: FileDate(0),
            extended_attributes: 0,
            last_modify_time: FileTime(0),
            last_modify_date: FileDate(0),
            first_file_cluster: 0,
            byte_size: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self.file_name[0] {
            0x00 => true,
            0xe5 => true,
            _ => false,
        }
    }

    pub fn get_filename(&self) -> &str {
        let mut len = 8;
        for i in 0..8 {
            if self.file_name[i] == 0x20 {
                len = i;
                break;
            }
        }
        core::str::from_utf8(&self.file_name[..len]).unwrap_or("!!!!!!!!")
    }

    pub fn get_full_name(&self) -> String {
        let mut name = String::new();
        name.push_str(self.get_filename());
        if self.ext[0] != 0x20 {
            name.push('.');
            name.push_str(self.get_ext());
        }
        name
    }

    pub fn get_ext(&self) -> &str {
        let mut len = 3;
        for i in 0..3 {
            if self.ext[i] == 0x20 {
                len = i;
                break;
            }
        }
        core::str::from_utf8(&self.ext[..len]).unwrap_or("!!!")
    }

    pub fn get_modification_timestamp(&self) -> u32 {
        let mod_time = self.last_modify_time;
        let mod_date = self.last_modify_date;
        encode_timestamp(
            mod_date.get_year(),
            mod_date.get_month(),
            mod_date.get_day(),
            mod_time.get_hours(),
            mod_time.get_minutes(),
            mod_time.get_seconds(),
        )
    }

    pub fn is_directory(&self) -> bool {
        self.attributes & 0x10 != 0
    }

    pub fn set_size(&mut self, size: u32) {
        self.byte_size = size;
    }

    pub fn set_first_cluster(&mut self, cluster: u16) {
        self.first_file_cluster = cluster;
    }

    pub fn set_filename(&mut self, filename: &[u8; 8], ext: &[u8; 3]) {
        self.file_name = *filename;
        self.ext = *ext;
    }

    pub fn set_attributes(&mut self, attributes: u8) {
        self.attributes = attributes;
    }

    pub fn first_file_cluster(&self) -> u16 {
        self.first_file_cluster
    }

    pub fn mark_deleted(&mut self) {
        self.file_name[0] = 0xE5;
    }

    pub fn matches_name(&self, filename: &[u8; 8], ext: &[u8; 3]) -> bool {
        for i in 0..8 {
            if ascii_char_matches(self.file_name[i], filename[i]) {
                continue;
            }
            return false;
        }
        for i in 0..3 {
            if ascii_char_matches(self.ext[i], ext[i]) {
                continue;
            }
            return false;
        }
        true
    }
}

fn ascii_char_matches(a: u8, b: u8) -> bool {
    if a > 0x40 && a < 0x5b {
        return a == b || (a + 0x20) == b;
    }
    if a > 0x60 && a < 0x7b {
        return a == b || a == (b + 0x20);
    }
    return a == b;
}

fn is_leap_year(year: u16) -> bool {
    let y = year as u32;
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Encode a date/time into a timestamp (seconds since 1980-01-01).
/// This is a standalone implementation so we don't depend on kernel time types.
fn encode_timestamp(year: u16, month: u16, day: u16, hours: u16, minutes: u16, seconds: u16) -> u32 {
    if year < 1980 || month == 0 || month > 12 {
        return 0;
    }

    const MONTH_START_OFFSET: [u32; 12] = [
        0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334,
    ];

    let yr = year - 1980;
    let quadrennials = yr as u32 / 4;
    let year_remainder = yr as u32 % 4;
    let mut days = quadrennials * (366 + 365 + 365 + 365) + year_remainder * 365;
    if year_remainder > 0 {
        days += 1;
    }
    days += MONTH_START_OFFSET[month as usize - 1];
    if month > 2 && is_leap_year(year) {
        days += 1; // leap day
    }
    days += day as u32 - 1;

    days * 86400 + hours as u32 * 3600 + minutes as u32 * 60 + seconds as u32
}

/// Decode a timestamp (seconds since 1980-01-01) into FAT FileDate and FileTime.
fn decode_timestamp(ts: u32) -> (FileDate, FileTime) {
    const MONTH_START_OFFSET: [u32; 12] = [
        0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334,
    ];

    let days = ts / 86400;
    let raw_time = ts % 86400;

    // year offset from days
    let year_offset = (days * 100) / 36525;
    let quadrennial_days = days % (365 + 365 + 365 + 366);
    let year_days = if quadrennial_days > 365 {
        (quadrennial_days - 366) % 365
    } else {
        quadrennial_days
    };
    let mut month = 0usize;
    let mut leap = 0u32;
    while month < 12 && MONTH_START_OFFSET[month] + leap <= year_days {
        month += 1;
        if month == 2 && year_offset % 4 == 0 {
            leap = 1;
        }
    }
    let mut day = year_days + 1 - MONTH_START_OFFSET[month - 1];
    if month > 2 {
        day -= leap;
    }

    let total_minutes = raw_time / 60;
    let seconds = raw_time % 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;

    let year = year_offset as u16 + 1980;
    let fat_date = FileDate::from_parts(year, month as u8, day as u8);
    let fat_time = FileTime::from_parts(hours as u8, minutes as u8, seconds as u8);
    (fat_date, fat_time)
}

#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct FileTime(u16);

impl FileTime {
    pub fn get_hours(&self) -> u16 {
        self.0 >> 11
    }

    pub fn get_minutes(&self) -> u16 {
        (self.0 >> 5) & 0x3f
    }

    pub fn get_seconds(&self) -> u16 {
        (self.0 & 0x1f) << 1
    }

    pub fn from_parts(hours: u8, minutes: u8, seconds: u8) -> Self {
        let val = ((hours as u16) << 11)
            | ((minutes as u16) << 5)
            | ((seconds as u16) >> 1);
        FileTime(val)
    }
}

#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct FileDate(u16);

impl FileDate {
    pub fn get_year(&self) -> u16 {
        ((self.0 >> 9) & 0x7f) + 1980
    }

    pub fn get_month(&self) -> u16 {
        (self.0 >> 5) & 0xf
    }

    pub fn get_day(&self) -> u16 {
        self.0 & 0x1f
    }

    pub fn from_parts(year: u16, month: u8, day: u8) -> Self {
        let year_val = if year >= 1980 { year - 1980 } else { 0 };
        let val = ((year_val & 0x7f) << 9)
            | ((month as u16 & 0xf) << 5)
            | (day as u16 & 0x1f);
        FileDate(val)
    }
}

pub struct RootDirectory {
    first_sector: u32,
    max_entries: u32,
}

impl RootDirectory {
    pub fn new(first_sector: u32, max_entries: u32) -> Self {
        Self {
            first_sector,
            max_entries,
        }
    }

    pub fn iter<'disk, D: DiskIO>(&self, disk: &'disk mut DiskAccess<D>) -> RootDirectoryIter<'disk, D> {
        let mut current = DirEntry::new();
        let dir_offset = self.first_sector * 512;
        disk.read_struct_from_disk(dir_offset, &mut current);

        RootDirectoryIter {
            disk,
            dir_offset,
            current_index: 0,
            max_index: self.max_entries,
            current,
        }
    }

    /// Write a DirEntry into the first free slot in the root directory.
    /// Returns the disk offset of the newly written entry.
    pub fn write_entry<D: DiskIO>(
        &self,
        entry: &DirEntry,
        disk: &mut DiskAccess<D>,
    ) -> Option<u32> {
        let dir_offset = self.first_sector * 512;
        let entry_size = core::mem::size_of::<DirEntry>() as u32;

        for i in 0..self.max_entries {
            let offset = dir_offset + i * entry_size;
            let mut slot = DirEntry::new();
            disk.read_struct_from_disk(offset, &mut slot);

            if slot.file_name[0] == 0x00 || slot.file_name[0] == 0xE5 {
                disk.write_struct_to_disk(offset, entry);
                return Some(offset);
            }
        }
        None
    }

    /// Create a new directory entry with fresh timestamps and write it to
    /// the first free slot. Returns the disk offset of the newly written entry.
    pub fn add_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        attributes: u8,
        first_cluster: u16,
        disk: &mut DiskAccess<D>,
        get_timestamp: fn() -> u32,
    ) -> Option<u32> {
        let mut new_entry = DirEntry::new();
        new_entry.set_filename(filename, ext);
        new_entry.set_attributes(attributes);
        new_entry.set_first_cluster(first_cluster);

        let (fat_date, fat_time) = decode_timestamp(get_timestamp());
        new_entry.creation_date = fat_date;
        new_entry.creation_time = fat_time;
        new_entry.last_modify_date = fat_date;
        new_entry.last_modify_time = fat_time;
        new_entry.access_date = fat_date;

        self.write_entry(&new_entry, disk)
    }

    /// Remove a directory entry by name. Sets the first byte to 0xE5 (deleted marker).
    /// Returns the DirEntry that was removed (for cluster chain cleanup).
    pub fn remove_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        disk: &mut DiskAccess<D>,
    ) -> Option<DirEntry> {
        let dir_offset = self.first_sector * 512;
        let entry_size = core::mem::size_of::<DirEntry>() as u32;

        for i in 0..self.max_entries {
            let offset = dir_offset + i * entry_size;
            let mut entry = DirEntry::new();
            disk.read_struct_from_disk(offset, &mut entry);

            if entry.file_name[0] == 0x00 {
                return None;
            }
            if entry.file_name[0] == 0xE5 {
                continue;
            }

            if entry.matches_name(filename, ext) {
                let removed = entry;
                entry.mark_deleted();
                disk.write_struct_to_disk(offset, &entry);
                return Some(removed);
            }
        }
        None
    }

    pub fn find_entry<D: DiskIO>(&self, name: &str, disk: &mut DiskAccess<D>) -> Option<Entity> {
        let (filename, ext) = match name.rsplit_once('.') {
            Some(pair) => pair,
            None => (name, ""),
        };
        let mut short_filename: [u8; 8] = [0x20; 8];
        let mut short_ext: [u8; 3] = [0x20; 3];
        let filename_len = filename.len().min(8);
        let ext_len = ext.len().min(3);
        short_filename[..filename_len].copy_from_slice(&filename.as_bytes()[..filename_len]);
        short_ext[..ext_len].copy_from_slice(&ext.as_bytes()[..ext_len]);

        for (entry, disk_offset) in self.iter(disk) {
            if entry.matches_name(&short_filename, &short_ext) {
                if entry.is_directory() {
                    return Some(Entity::Dir(Directory::from_dir_entry(entry)));
                } else {
                    return Some(Entity::File(File::from_dir_entry(entry, disk_offset)));
                }
            }
        }
        None
    }
}

pub struct RootDirectoryIter<'disk, D: DiskIO> {
    disk: &'disk mut DiskAccess<D>,
    dir_offset: u32,
    current_index: u32,
    max_index: u32,
    current: DirEntry,
}

impl<D: DiskIO> RootDirectoryIter<'_, D> {
    fn current_entry_offset(&self) -> u32 {
        self.dir_offset + self.current_index * core::mem::size_of::<DirEntry>() as u32
    }
}

impl<D: DiskIO> Iterator for RootDirectoryIter<'_, D> {
    type Item = (DirEntry, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_empty() {
            return None;
        }

        if self.current_index + 1 >= self.max_index {
            return None;
        }

        let entry = self.current.clone();
        let entry_offset = self.current_entry_offset();
        self.current_index += 1;
        let offset = self.dir_offset + self.current_index * core::mem::size_of::<DirEntry>() as u32;
        self.disk.read_struct_from_disk(offset, &mut self.current);

        Some((entry, entry_offset))
    }
}

pub enum Entity {
    Dir(Directory),
    File(File),
}

pub enum DirectoryType {
    Root(RootDirectory),
    Subdir(DirEntry),
}

pub struct Directory {
    dir_type: DirectoryType,
    entries_fetched: bool,
    entries: Vec<u8>,
}

impl Directory {
    pub fn dir_type(&self) -> &DirectoryType {
        &self.dir_type
    }

    pub fn get_modification_time(&self) -> u32 {
        match &self.dir_type {
            DirectoryType::Root(_) => 0,
            DirectoryType::Subdir(entry) => entry.get_modification_timestamp(),
        }
    }

    pub fn from_dir_entry(dir_entry: DirEntry) -> Self {
        Self {
            dir_type: DirectoryType::Subdir(dir_entry),
            entries_fetched: false,
            entries: Vec::new(),
        }
    }

    pub fn from_root_dir(root: RootDirectory) -> Self {
        Self {
            dir_type: DirectoryType::Root(root),
            entries_fetched: false,
            entries: Vec::new(),
        }
    }

    pub fn read<D: DiskIO>(
        &mut self,
        buffer: &mut [u8],
        offset: u32,
        table: AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> u32 {
        if !self.entries_fetched {
            match &self.dir_type {
                DirectoryType::Root(root) => {
                    for (entry, _offset) in root.iter(disk) {
                        let name = entry.get_full_name();
                        self.entries.extend_from_slice(name.as_bytes());
                        self.entries.push(0);
                    }
                    self.entries_fetched = true;
                }
                DirectoryType::Subdir(entry) => {
                    let subdir = SubDirectory::new(entry.first_file_cluster() as u32);
                    for (dir_entry, _offset) in subdir.iter(&table, disk) {
                        let name = dir_entry.get_full_name();
                        self.entries.extend_from_slice(name.as_bytes());
                        self.entries.push(0);
                    }
                    self.entries_fetched = true;
                }
            }
        }

        let mut bytes_written = 0;
        let bytes_remaining = self.entries.len() - offset as usize;
        let bytes_to_write = bytes_remaining.min(buffer.len());
        while bytes_written < bytes_to_write {
            buffer[bytes_written] = *self.entries.get(offset as usize + bytes_written).unwrap();
            bytes_written += 1;
        }

        bytes_written as u32
    }
}

#[derive(Clone)]
pub struct File {
    dir_entry: DirEntry,
    dir_entry_disk_offset: u32,
    cluster_cache: Vec<u32>,
}

impl File {
    pub fn from_dir_entry(dir_entry: DirEntry, disk_offset: u32) -> Self {
        Self {
            dir_entry,
            dir_entry_disk_offset: disk_offset,
            cluster_cache: Vec::new(),
        }
    }

    pub fn file_name(&self) -> String {
        let mut full_name = String::from(self.dir_entry.get_filename());
        full_name.push('.');
        full_name.push_str(self.dir_entry.get_ext());
        full_name
    }

    pub fn byte_size(&self) -> u32 {
        self.dir_entry.byte_size
    }

    pub fn first_cluster(&self) -> u16 {
        self.dir_entry.first_file_cluster
    }

    pub fn dir_entry_disk_offset(&self) -> u32 {
        self.dir_entry_disk_offset
    }

    pub fn dir_entry_mut(&mut self) -> &mut DirEntry {
        &mut self.dir_entry
    }

    pub fn invalidate_cluster_cache(&mut self) {
        self.cluster_cache.clear();
    }

    pub fn get_modification_time(&self) -> u32 {
        self.dir_entry.get_modification_timestamp()
    }

    pub fn cache_cluster_chain<D: DiskIO>(
        &mut self,
        table: AllocationTable,
        start_cluster: u32,
        disk: &mut DiskAccess<D>,
    ) {
        self.cluster_cache.clear();
        let mut current_cluster = start_cluster;
        while current_cluster != 0xfff {
            self.cluster_cache.push(current_cluster);
            current_cluster = match table.get_next_cluster(current_cluster, disk) {
                Some(next) => next,
                None => return,
            }
        }
    }

    pub fn write<D: DiskIO>(
        &mut self,
        data: &[u8],
        initial_offset: u32,
        table: AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> u32 {
        let mut offset = initial_offset;
        let mut bytes_written = 0usize;

        if self.cluster_cache.is_empty() && self.dir_entry.first_file_cluster != 0 {
            self.cache_cluster_chain(table, self.dir_entry.first_file_cluster as u32, disk);
        }

        if self.cluster_cache.is_empty() {
            if let Some(cluster) = table.allocate_cluster(disk) {
                self.dir_entry.first_file_cluster = cluster as u16;
                self.cluster_cache.push(cluster);
            } else {
                return 0;
            }
        }

        loop {
            if bytes_written >= data.len() {
                break;
            }

            let current_relative_cluster = offset / table.bytes_per_cluster();
            let cluster_offset = offset % table.bytes_per_cluster();

            while current_relative_cluster as usize >= self.cluster_cache.len() {
                let prev_cluster = *self.cluster_cache.last().unwrap();
                if let Some(new_cluster) = table.allocate_cluster(disk) {
                    table.set_cluster_entry(prev_cluster, new_cluster, disk);
                    self.cluster_cache.push(new_cluster);
                } else {
                    return bytes_written as u32;
                }
            }

            let current_cluster = self.cluster_cache[current_relative_cluster as usize];
            let cluster_location = table.get_cluster_location(current_cluster);

            let bytes_remaining_in_cluster = table.bytes_per_cluster() - cluster_offset;
            let bytes_to_write = (data.len() - bytes_written).min(bytes_remaining_in_cluster as usize);

            disk.write_bytes_to_disk(
                cluster_location + cluster_offset,
                &data[bytes_written..bytes_written + bytes_to_write],
            );

            bytes_written += bytes_to_write;
            offset += bytes_to_write as u32;
        }

        let new_end = initial_offset + bytes_written as u32;
        if new_end > self.dir_entry.byte_size {
            self.dir_entry.byte_size = new_end;
            disk.write_struct_to_disk(self.dir_entry_disk_offset, &self.dir_entry);
        }

        bytes_written as u32
    }

    pub fn read<D: DiskIO>(
        &mut self,
        buffer: &mut [u8],
        initial_offset: u32,
        table: AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> u32 {
        let mut offset = initial_offset;
        let mut bytes_written = 0;

        loop {
            let current_relative_cluster = offset / table.bytes_per_cluster();
            let cluster_offset = offset % table.bytes_per_cluster();

            if self.cluster_cache.is_empty() {
                self.cache_cluster_chain(table, self.dir_entry.first_file_cluster as u32, disk);
            }

            let current_cluster = match self.cluster_cache.get(current_relative_cluster as usize) {
                Some(&cluster) => cluster,
                None => return bytes_written as u32,
            };
            let cluster_location = table.get_cluster_location(current_cluster);

            let bytes_remaining_in_file = self.byte_size() - offset;
            let bytes_remaining_in_cluster = table.bytes_per_cluster() - cluster_offset;

            let bytes_from_disk = bytes_remaining_in_file.min(bytes_remaining_in_cluster) as usize;
            let buffer_end = buffer.len().min(bytes_written + bytes_from_disk);

            let read_buffer = &mut buffer[bytes_written..buffer_end];

            let read_size =
                disk.read_bytes_from_disk(cluster_location + cluster_offset, read_buffer);
            bytes_written += read_size as usize;
            offset += read_size;

            if bytes_written as u32 + initial_offset >= self.byte_size() {
                return bytes_written as u32;
            }
            if bytes_written >= buffer.len() {
                return bytes_written as u32;
            }
        }
    }
}

/// Parse a filename string into FAT 8.3 format (uppercase, space-padded).
pub fn parse_short_name(name: &str) -> ([u8; 8], [u8; 3]) {
    let (filename, ext) = match name.rsplit_once('.') {
        Some(pair) => pair,
        None => (name, ""),
    };
    let mut short_filename: [u8; 8] = [0x20; 8];
    let mut short_ext: [u8; 3] = [0x20; 3];
    let filename_len = filename.len().min(8);
    let ext_len = ext.len().min(3);
    for i in 0..filename_len {
        short_filename[i] = filename.as_bytes()[i].to_ascii_uppercase();
    }
    for i in 0..ext_len {
        short_ext[i] = ext.as_bytes()[i].to_ascii_uppercase();
    }
    (short_filename, short_ext)
}

/// Check if a subdirectory (given by its first cluster) is empty.
pub fn is_subdir_empty<D: DiskIO>(
    first_cluster: u32,
    table: &AllocationTable,
    disk: &mut DiskAccess<D>,
) -> bool {
    let entry_size = core::mem::size_of::<DirEntry>() as u32;
    let entries_per_cluster = table.bytes_per_cluster() / entry_size;

    let mut cluster = first_cluster;
    loop {
        let loc = table.get_cluster_location(cluster);
        for i in 0..entries_per_cluster {
            let offset = loc + i * entry_size;
            let mut entry = DirEntry::new();
            disk.read_struct_from_disk(offset, &mut entry);

            if entry.file_name[0] == 0x00 {
                return true;
            }
            if entry.file_name[0] == 0xE5 {
                continue;
            }
            if entry.get_filename() == "." || entry.get_filename() == ".." {
                continue;
            }
            return false;
        }
        match table.get_next_cluster(cluster, disk) {
            Some(next) => cluster = next,
            None => return true,
        }
    }
}

/// Represents a subdirectory stored in a cluster chain
pub struct SubDirectory {
    first_cluster: u32,
}

impl SubDirectory {
    pub fn new(first_cluster: u32) -> Self {
        Self { first_cluster }
    }

    pub fn iter<'a, D: DiskIO>(
        &self,
        table: &AllocationTable,
        disk: &'a mut DiskAccess<D>,
    ) -> SubdirIter<'a, D> {
        let entry_size = core::mem::size_of::<DirEntry>() as u32;
        let entries_per_cluster = table.bytes_per_cluster() / entry_size;
        let cluster_location = table.get_cluster_location(self.first_cluster);
        let mut current = DirEntry::new();
        disk.read_struct_from_disk(cluster_location, &mut current);

        SubdirIter {
            disk,
            table: *table,
            current_cluster: self.first_cluster,
            current_index_in_cluster: 0,
            entries_per_cluster,
            current,
        }
    }

    pub fn find_entry<D: DiskIO>(
        &self,
        name: &str,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<Entity> {
        let (short_filename, short_ext) = parse_short_name(name);

        for (entry, disk_offset) in self.iter(table, disk) {
            if entry.matches_name(&short_filename, &short_ext) {
                if entry.is_directory() {
                    return Some(Entity::Dir(Directory::from_dir_entry(entry)));
                } else {
                    return Some(Entity::File(File::from_dir_entry(entry, disk_offset)));
                }
            }
        }
        None
    }

    pub fn write_entry<D: DiskIO>(
        &self,
        entry: &DirEntry,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<u32> {
        let entry_size = core::mem::size_of::<DirEntry>() as u32;
        let entries_per_cluster = table.bytes_per_cluster() / entry_size;

        let mut cluster = self.first_cluster;
        loop {
            let cluster_location = table.get_cluster_location(cluster);
            for i in 0..entries_per_cluster {
                let offset = cluster_location + i * entry_size;
                let mut slot = DirEntry::new();
                disk.read_struct_from_disk(offset, &mut slot);

                if slot.file_name[0] == 0x00 || slot.file_name[0] == 0xE5 {
                    disk.write_struct_to_disk(offset, entry);
                    return Some(offset);
                }
            }
            match table.get_next_cluster(cluster, disk) {
                Some(next) => cluster = next,
                None => break,
            }
        }

        // `cluster` is the last cluster in the chain (get_next_cluster returned None)
        let new_cluster = table.allocate_cluster(disk)?;
        table.set_cluster_entry(cluster, new_cluster, disk);

        let new_loc = table.get_cluster_location(new_cluster);
        let bytes_per_cluster = table.bytes_per_cluster();
        let zero_buf = [0u8; 512];
        let mut off = 0u32;
        while off < bytes_per_cluster {
            let n = (bytes_per_cluster - off).min(512);
            disk.write_bytes_to_disk(new_loc + off, &zero_buf[..n as usize]);
            off += n;
        }

        disk.write_struct_to_disk(new_loc, entry);
        Some(new_loc)
    }

    pub fn add_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        attributes: u8,
        first_cluster: u16,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
        get_timestamp: fn() -> u32,
    ) -> Option<u32> {
        let mut new_entry = DirEntry::new();
        new_entry.set_filename(filename, ext);
        new_entry.set_attributes(attributes);
        new_entry.set_first_cluster(first_cluster);

        let (fat_date, fat_time) = decode_timestamp(get_timestamp());
        new_entry.creation_date = fat_date;
        new_entry.creation_time = fat_time;
        new_entry.last_modify_date = fat_date;
        new_entry.last_modify_time = fat_time;
        new_entry.access_date = fat_date;

        self.write_entry(&new_entry, table, disk)
    }

    pub fn remove_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<DirEntry> {
        let entry_size = core::mem::size_of::<DirEntry>() as u32;
        let entries_per_cluster = table.bytes_per_cluster() / entry_size;

        let mut cluster = self.first_cluster;
        loop {
            let cluster_location = table.get_cluster_location(cluster);
            for i in 0..entries_per_cluster {
                let offset = cluster_location + i * entry_size;
                let mut entry = DirEntry::new();
                disk.read_struct_from_disk(offset, &mut entry);

                if entry.file_name[0] == 0x00 {
                    return None;
                }
                if entry.file_name[0] == 0xE5 {
                    continue;
                }

                if entry.matches_name(filename, ext) {
                    let removed = entry;
                    entry.mark_deleted();
                    disk.write_struct_to_disk(offset, &entry);
                    return Some(removed);
                }
            }
            match table.get_next_cluster(cluster, disk) {
                Some(next) => cluster = next,
                None => return None,
            }
        }
    }
}

pub struct SubdirIter<'a, D: DiskIO> {
    disk: &'a mut DiskAccess<D>,
    table: AllocationTable,
    current_cluster: u32,
    current_index_in_cluster: u32,
    entries_per_cluster: u32,
    current: DirEntry,
}

impl<D: DiskIO> SubdirIter<'_, D> {
    fn current_entry_offset(&self) -> u32 {
        let cluster_location = self.table.get_cluster_location(self.current_cluster);
        cluster_location + self.current_index_in_cluster * core::mem::size_of::<DirEntry>() as u32
    }
}

impl<D: DiskIO> Iterator for SubdirIter<'_, D> {
    type Item = (DirEntry, u32);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current.file_name[0] == 0x00 {
                return None;
            }

            let entry = self.current;
            let offset = self.current_entry_offset();

            self.current_index_in_cluster += 1;
            if self.current_index_in_cluster >= self.entries_per_cluster {
                match self.table.get_next_cluster(self.current_cluster, self.disk) {
                    Some(next) => {
                        self.current_cluster = next;
                        self.current_index_in_cluster = 0;
                    }
                    None => {
                        self.current.file_name[0] = 0x00;
                    }
                }
            }

            if self.current.file_name[0] != 0x00 {
                let next_offset = self.current_entry_offset();
                self.disk.read_struct_from_disk(next_offset, &mut self.current);
            }

            if entry.file_name[0] == 0xE5 {
                continue;
            }

            return Some((entry, offset));
        }
    }
}

/// Unified directory handle that can represent either the root directory or a subdirectory
pub enum AnyDirectory {
    Root(RootDirectory),
    Sub(SubDirectory),
}

impl AnyDirectory {
    pub fn first_cluster(&self) -> u32 {
        match self {
            AnyDirectory::Root(_) => 0,
            AnyDirectory::Sub(sub) => sub.first_cluster,
        }
    }

    pub fn find_entry<D: DiskIO>(
        &self,
        name: &str,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<Entity> {
        match self {
            AnyDirectory::Root(root) => root.find_entry(name, disk),
            AnyDirectory::Sub(sub) => sub.find_entry(name, table, disk),
        }
    }

    pub fn add_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        attributes: u8,
        first_cluster: u16,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
        get_timestamp: fn() -> u32,
    ) -> Option<u32> {
        match self {
            AnyDirectory::Root(root) => root.add_entry(filename, ext, attributes, first_cluster, disk, get_timestamp),
            AnyDirectory::Sub(sub) => sub.add_entry(filename, ext, attributes, first_cluster, table, disk, get_timestamp),
        }
    }

    pub fn remove_entry<D: DiskIO>(
        &self,
        filename: &[u8; 8],
        ext: &[u8; 3],
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<DirEntry> {
        match self {
            AnyDirectory::Root(root) => root.remove_entry(filename, ext, disk),
            AnyDirectory::Sub(sub) => sub.remove_entry(filename, ext, table, disk),
        }
    }

    pub fn write_entry<D: DiskIO>(
        &self,
        entry: &DirEntry,
        table: &AllocationTable,
        disk: &mut DiskAccess<D>,
    ) -> Option<u32> {
        match self {
            AnyDirectory::Root(root) => root.write_entry(entry, disk),
            AnyDirectory::Sub(sub) => sub.write_entry(entry, table, disk),
        }
    }
}

/// Resolve a path into the parent directory and the leaf component name.
pub fn resolve_path<'a, D: DiskIO>(
    path: &'a str,
    root: RootDirectory,
    table: &AllocationTable,
    disk: &mut DiskAccess<D>,
) -> Result<(AnyDirectory, &'a str), PathError> {
    if path.is_empty() {
        return Err(PathError::InvalidArgument);
    }

    let sep_pos = path.rfind('/').or_else(|| path.rfind('\\'));
    let (parent_path, leaf) = match sep_pos {
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => ("", path),
    };

    if leaf.is_empty() {
        return Err(PathError::InvalidArgument);
    }

    let mut current = AnyDirectory::Root(root);

    if !parent_path.is_empty() {
        for component in parent_path.split(|c: char| c == '/' || c == '\\') {
            if component.is_empty() {
                continue;
            }
            let entity = current
                .find_entry(component, table, disk)
                .ok_or(PathError::NotFound)?;
            match entity {
                Entity::Dir(d) => match d.dir_type() {
                    DirectoryType::Subdir(entry) => {
                        current = AnyDirectory::Sub(SubDirectory::new(
                            entry.first_file_cluster() as u32,
                        ));
                    }
                    DirectoryType::Root(_) => return Err(PathError::InvalidArgument),
                },
                Entity::File(_) => return Err(PathError::NotFound),
            }
        }
    }

    Ok((current, leaf))
}

/// Path resolution error type - platform-independent
#[derive(Debug)]
pub enum PathError {
    InvalidArgument,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::DirEntry;

    #[test]
    fn filename_matching() {
        let mut direntry = DirEntry::new();
        direntry.file_name.copy_from_slice("MYFILE  ".as_bytes());
        direntry.ext.copy_from_slice("TXT".as_bytes());

        assert!(direntry.matches_name(
            &[b'M', b'Y', b'F', b'I', b'L', b'E', b' ', b' '],
            &[b'T', b'X', b'T'],
        ));
        assert!(direntry.matches_name(
            &[b'M', b'y', b'F', b'i', b'l', b'e', b' ', b' '],
            &[b't', b'x', b't'],
        ));

        assert!(!direntry.matches_name(
            &[b'O', b'T', b'H', b'E', b'R', b' ', b' ', b' '],
            &[b'T', b'X', b'T'],
        ));
    }
}
