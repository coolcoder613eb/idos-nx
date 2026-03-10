//! VGA graphics mode support for the DOS compatibility layer.
//!
//! Manages the VGA mode state, graphics buffer mapping, palette, and BDA
//! (BIOS Data Area) updates when the video mode changes.

use idos_api::{
    io::{
        sync::ioctl_sync,
        Handle,
    },
    syscall::memory::map_memory,
};

use super::STDIN;

/// Current VGA video mode (default 0x03 = 80x25 text)
pub(crate) static mut VGA_MODE: u8 = 0x03;
/// Physical address of the IDOS graphics buffer (returned by TSETGFX ioctl).
/// Zero means no graphics mode is active.
pub(crate) static mut GFX_BUFFER_PADDR: u32 = 0;
/// Virtual address where we mapped the graphics buffer (for writing dirty rect).
pub(crate) static mut GFX_BUFFER_VADDR: u32 = 0;
/// Size of the mapped graphics buffer in bytes.
pub(crate) static mut GFX_BUFFER_SIZE: u32 = 0;

/// VGA palette: 256 entries of (R, G, B), used for INT 10h AH=10h palette ops.
/// Initialized to the standard VGA 256-color palette.
pub(crate) static mut VGA_PALETTE: [u8; 768] = [0; 768];

/// Reset all graphics statics to their initial state.
pub(crate) fn init() {
    unsafe {
        VGA_MODE = 0x03;
        GFX_BUFFER_PADDR = 0;
        GFX_BUFFER_VADDR = 0;
        GFX_BUFFER_SIZE = 0;
        VGA_PALETTE = [0; 768];
    }
}

/// Enter graphics mode: request a graphics buffer from the console via ioctl,
/// map it into our address space, and map shadow RAM at 0xA0000 for the v86 program.
pub(crate) fn enter_graphics_mode(width: u16, height: u16, bpp: u8) {
    use idos_api::io::termios::{GraphicsMode, TSETGFX};

    let pixel_bytes = width as u32 * height as u32 * ((bpp as u32 + 7) / 8);

    // Shadow RAM at 0xA0000 is already mapped by setup_dos_memory.

    let mut gfx_mode = GraphicsMode {
        width,
        height,
        bpp_flags: bpp as u32,
        framebuffer: 0,
    };

    let _ = ioctl_sync(
        STDIN,
        TSETGFX,
        &mut gfx_mode as *mut GraphicsMode as u32,
        core::mem::size_of::<GraphicsMode>() as u32,
    );

    let paddr = gfx_mode.framebuffer;
    if paddr == 0 {
        return;
    }

    let buf_size = 8 + pixel_bytes;
    let buf_pages = (buf_size + 0xfff) & !0xfff;

    // Map the graphics buffer into our address space so we can write the dirty
    // rect and copy pixel data into it.
    let vaddr = map_memory(None, buf_pages, Some(paddr)).unwrap_or(0);

    unsafe {
        GFX_BUFFER_PADDR = paddr;
        GFX_BUFFER_VADDR = vaddr;
        GFX_BUFFER_SIZE = pixel_bytes;
    }

    // Read the kernel's default palette into our local copy
    load_idos_palette();
}

/// Exit graphics mode: unmap the graphics buffer and tell the console to
/// return to text mode.
pub(crate) fn exit_graphics_mode() {
    use idos_api::io::termios::TSETTEXT;

    let _ = ioctl_sync(STDIN, TSETTEXT, 0, 0);

    unsafe {
        GFX_BUFFER_PADDR = 0;
        GFX_BUFFER_VADDR = 0;
        GFX_BUFFER_SIZE = 0;
    }
}

/// Copy the shadow VGA framebuffer at 0xA0000 into the IDOS graphics buffer
/// and mark it dirty so the compositor redraws.
pub(crate) fn sync_graphics_buffer() {
    unsafe {
        let vaddr = core::ptr::read_volatile(&GFX_BUFFER_VADDR);
        let size = core::ptr::read_volatile(&GFX_BUFFER_SIZE);
        if vaddr == 0 || size == 0 {
            return;
        }
        let src = 0xA0000 as *const u8;
        let dst = (vaddr + 8) as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, size as usize);

        // Write dirty rect header: full screen
        let header = vaddr as *mut u16;
        core::ptr::write_volatile(header, 0); // x
        core::ptr::write_volatile(header.add(1), 0); // y
        core::ptr::write_volatile(header.add(2), 0xFFFF); // w (full)
        core::ptr::write_volatile(header.add(3), 0xFFFF); // h (full)
    }
}

/// Read the console's current palette into VGA_PALETTE.
pub(crate) fn load_idos_palette() {
    use idos_api::io::termios::TGETPAL;
    unsafe {
        let _ = ioctl_sync(STDIN, TGETPAL, VGA_PALETTE.as_mut_ptr() as u32, 768);
    }
}

/// Push VGA_PALETTE to the console.
pub(crate) fn push_idos_palette() {
    use idos_api::io::termios::TSETPAL;
    unsafe {
        let _ = ioctl_sync(STDIN, TSETPAL, VGA_PALETTE.as_ptr() as u32, 768);
    }
}

/// Update BDA fields when the video mode changes.
pub(crate) fn update_bda_for_mode(mode: u8) {
    unsafe {
        let bda = 0x400 as *mut u8;
        core::ptr::write_volatile(bda.add(0x49), mode);
        match mode {
            0x13 => {
                // 320x200x256
                core::ptr::write_volatile((bda.add(0x4A)) as *mut u16, 40); // columns
                core::ptr::write_volatile((bda.add(0x4C)) as *mut u16, 0); // page size (N/A)
                core::ptr::write_volatile(bda.add(0x84), 24); // rows - 1
            }
            _ => {
                // 80x25 text
                core::ptr::write_volatile((bda.add(0x4A)) as *mut u16, 80);
                core::ptr::write_volatile((bda.add(0x4C)) as *mut u16, 4000);
                core::ptr::write_volatile(bda.add(0x84), 24);
            }
        }
        // Reset cursor and page
        core::ptr::write_volatile((bda.add(0x4E)) as *mut u16, 0);
        core::ptr::write_volatile(bda.add(0x62), 0);
    }
}
