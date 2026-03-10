//! Conventional memory arena and DPMI high-memory block allocator.
//!
//! This is a leaf module with no dependencies on other doslayer modules
//! except for `DOS_MEM_TOP_SEGMENT` from the parent.

use crate::dos::DOS_MEM_TOP_SEGMENT;

// ---- Conventional memory arena ----

/// Simple conventional memory allocator.
/// Tracks allocated blocks as (segment, size_in_paragraphs) pairs.
/// Free space starts at DOS_ARENA_START and goes up to DOS_MEM_TOP_SEGMENT.
const DOS_ARENA_MAX_BLOCKS: usize = 32;
/// Each entry: (segment, size_paragraphs). segment=0 means free slot.
static mut DOS_ARENA: [(u16, u16); DOS_ARENA_MAX_BLOCKS] = [(0, 0); DOS_ARENA_MAX_BLOCKS];
/// Start of free conventional memory (paragraph/segment). Set after program load.
static mut DOS_ARENA_START: u16 = 0;

// ---- High memory block tracker ----

/// High memory block tracker for DPMI 0x501/0x502.
/// Each entry: (linear_address, size_bytes). address=0 means free slot.
const DPMI_HIGH_MEM_MAX: usize = 32;
static mut DPMI_HIGH_MEM: [(u32, u32); DPMI_HIGH_MEM_MAX] = [(0, 0); DPMI_HIGH_MEM_MAX];

/// Zero out all memory allocator state. Call once at startup.
pub(crate) fn init() {
    unsafe {
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            DOS_ARENA[i] = (0, 0);
        }
        DOS_ARENA_START = 0;
        for i in 0..DPMI_HIGH_MEM_MAX {
            DPMI_HIGH_MEM[i] = (0, 0);
        }
    }
}

/// Set the arena start segment (call after loading the program).
pub(crate) fn dos_arena_set_start(segment: u16) {
    unsafe {
        DOS_ARENA_START = segment;
    }
}

/// Get the current arena start segment.
pub(crate) fn dos_arena_start() -> u16 {
    unsafe { DOS_ARENA_START }
}

/// Record an initial allocation in the arena (e.g. the program's own block).
pub(crate) fn dos_arena_record(segment: u16, paras: u16) {
    unsafe {
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == 0 {
                DOS_ARENA[i] = (segment, paras);
                return;
            }
        }
    }
}

/// Allocate `paras` paragraphs from the conventional memory arena.
/// Returns the segment of the allocated block, or None.
pub(crate) fn dos_arena_alloc(paras: u16) -> Option<u16> {
    unsafe {
        // Find the lowest free address by scanning existing blocks
        let mut cursor = DOS_ARENA_START;
        // Sort blocks by segment to find gaps (simple approach: find first fit)
        // Collect occupied regions
        let mut occupied = [(0u16, 0u16); DOS_ARENA_MAX_BLOCKS];
        let mut n_occupied = 0;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 != 0 {
                occupied[n_occupied] = DOS_ARENA[i];
                n_occupied += 1;
            }
        }
        // Simple insertion sort by segment
        for i in 1..n_occupied {
            let key = occupied[i];
            let mut j = i;
            while j > 0 && occupied[j - 1].0 > key.0 {
                occupied[j] = occupied[j - 1];
                j -= 1;
            }
            occupied[j] = key;
        }
        // First-fit: walk through sorted blocks, look for gap
        cursor = DOS_ARENA_START;
        for i in 0..n_occupied {
            let blk_start = occupied[i].0;
            let blk_end = blk_start + occupied[i].1;
            if cursor + paras <= blk_start {
                // Found a gap before this block
                break;
            }
            if blk_end > cursor {
                cursor = blk_end;
            }
        }
        // Check if there's room before DOS_MEM_TOP
        if (cursor as u32 + paras as u32) > DOS_MEM_TOP_SEGMENT as u32 {
            return None;
        }
        // Find a free slot in the arena table
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == 0 {
                DOS_ARENA[i] = (cursor, paras);
                return Some(cursor);
            }
        }
        None // table full
    }
}

/// Free a conventional memory block by segment.
pub(crate) fn dos_arena_free(segment: u16) -> bool {
    unsafe {
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == segment {
                DOS_ARENA[i] = (0, 0);
                return true;
            }
        }
        false
    }
}

/// Resize a conventional memory block. Only grows/shrinks in place.
pub(crate) fn dos_arena_resize(segment: u16, new_paras: u16) -> bool {
    unsafe {
        // Find the block
        let mut idx = DOS_ARENA_MAX_BLOCKS;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 == segment {
                idx = i;
                break;
            }
        }
        if idx == DOS_ARENA_MAX_BLOCKS {
            return false;
        }
        let blk_end = segment as u32 + new_paras as u32;
        if blk_end > DOS_MEM_TOP_SEGMENT as u32 {
            return false;
        }
        // Check no other block overlaps the new range
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if i == idx || DOS_ARENA[i].0 == 0 {
                continue;
            }
            let other_start = DOS_ARENA[i].0 as u32;
            let other_end = other_start + DOS_ARENA[i].1 as u32;
            // Overlap check
            if (segment as u32) < other_end && blk_end > other_start {
                return false;
            }
        }
        DOS_ARENA[idx].1 = new_paras;
        true
    }
}

/// Return the largest free contiguous block in paragraphs.
pub(crate) fn dos_arena_largest() -> u16 {
    unsafe {
        let mut occupied = [(0u16, 0u16); DOS_ARENA_MAX_BLOCKS];
        let mut n = 0;
        for i in 0..DOS_ARENA_MAX_BLOCKS {
            if DOS_ARENA[i].0 != 0 {
                occupied[n] = DOS_ARENA[i];
                n += 1;
            }
        }
        // Sort by segment
        for i in 1..n {
            let key = occupied[i];
            let mut j = i;
            while j > 0 && occupied[j - 1].0 > key.0 {
                occupied[j] = occupied[j - 1];
                j -= 1;
            }
            occupied[j] = key;
        }
        let mut cursor = DOS_ARENA_START;
        let mut largest: u16 = 0;
        for i in 0..n {
            let gap = occupied[i].0.saturating_sub(cursor);
            if gap > largest {
                largest = gap;
            }
            let end = occupied[i].0 + occupied[i].1;
            if end > cursor {
                cursor = end;
            }
        }
        // Gap after last block
        let gap = DOS_MEM_TOP_SEGMENT.saturating_sub(cursor);
        if gap > largest {
            largest = gap;
        }
        largest
    }
}

/// Record a high-memory allocation. Returns a handle (1-based index) or None.
pub(crate) fn dpmi_high_mem_record(addr: u32, size: u32) -> Option<u32> {
    unsafe {
        for i in 0..DPMI_HIGH_MEM_MAX {
            if DPMI_HIGH_MEM[i].0 == 0 {
                DPMI_HIGH_MEM[i] = (addr, size);
                return Some((i + 1) as u32); // 1-based handle
            }
        }
        None
    }
}

/// Free a high-memory block by handle. Unmaps the memory.
pub(crate) fn dpmi_high_mem_free(handle: u32) -> bool {
    if handle == 0 {
        return false;
    }
    let idx = (handle - 1) as usize;
    unsafe {
        if idx >= DPMI_HIGH_MEM_MAX || DPMI_HIGH_MEM[idx].0 == 0 {
            return false;
        }
        let (addr, size) = DPMI_HIGH_MEM[idx];
        let _ = idos_api::syscall::memory::unmap_memory(addr, size);
        DPMI_HIGH_MEM[idx] = (0, 0);
        true
    }
}
