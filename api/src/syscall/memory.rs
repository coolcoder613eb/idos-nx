use super::syscall;

/// Sentinel: no physical address, pages allocated on demand.
const MMAP_FREE: u32 = 0xffff_ffff;
/// Sentinel: allocate physically contiguous pages at map time.
const MMAP_CONTIGUOUS: u32 = 0xffff_fffe;

pub fn map_memory(
    virtual_address: Option<u32>,
    size: u32,
    physical_address: Option<u32>,
) -> Result<u32, ()> {
    let result = syscall(
        0x30,
        virtual_address.unwrap_or(MMAP_FREE),
        size,
        physical_address.unwrap_or(MMAP_FREE),
    );

    if result == 0xffff_ffff {
        Err(())
    } else {
        Ok(result)
    }
}

/// Allocate a region of physically contiguous memory. Pages are allocated
/// and mapped immediately (not on demand), making this safe for DMA and
/// for sharing with device drivers.
pub fn map_memory_contiguous(size: u32) -> Result<u32, ()> {
    let result = syscall(
        0x30,
        MMAP_FREE,
        size,
        MMAP_CONTIGUOUS,
    );

    if result == 0xffff_ffff {
        Err(())
    } else {
        Ok(result)
    }
}

/// Allocate a region of physically contiguous DMA memory, returning both
/// the virtual address and the physical address. This is needed by device
/// drivers that must program hardware with physical addresses for DMA.
pub fn map_dma_memory(size: u32) -> Result<(u32, u32), ()> {
    let (vaddr, paddr) = super::syscall_2(0x62, size, 0, 0);
    if vaddr == 0xffff_ffff {
        Err(())
    } else {
        Ok((vaddr, paddr))
    }
}

pub fn unmap_memory(address: u32, size: u32) -> Result<(), ()> {
    let result = syscall(0x32, address, size, 0);
    if result == 0xffff_ffff {
        Err(())
    } else {
        Ok(())
    }
}

pub const MMAP_SHARED: u32 = 1;

#[repr(C)]
pub struct FileMapping {
    pub virtual_address: u32,
    pub size: u32,
    pub path_ptr: u32,
    pub path_len: u32,
    pub file_offset: u32,
    pub flags: u32,
}

pub fn map_file(
    virtual_address: Option<u32>,
    size: u32,
    path: &str,
    file_offset: u32,
    flags: u32,
) -> Result<u32, ()> {
    let path_bytes = path.as_bytes();
    let mapping = FileMapping {
        virtual_address: virtual_address.unwrap_or(0xffff_ffff),
        size,
        path_ptr: path_bytes.as_ptr() as u32,
        path_len: path_bytes.len() as u32,
        file_offset,
        flags,
    };

    let result = syscall(0x31, &mapping as *const FileMapping as u32, 0, 0);

    if result == 0xffff_ffff {
        Err(())
    } else {
        Ok(result)
    }
}
