//! BIOS interrupt handlers (INT 10h video, INT 16h keyboard).

use core::sync::atomic::{AtomicU32, Ordering};

use idos_api::{
    compat::VMRegisters,
    io::{sync::{read_sync, write_sync}, Handle},
};

use super::{dos_log, fmt_unsupported, STDIN};
use crate::graphics::{VGA_MODE, VGA_PALETTE, enter_graphics_mode, exit_graphics_mode, push_idos_palette, update_bda_for_mode};

/// One-key lookahead buffer for peek_next_key / read_next_key.
static mut KEY_LOOKAHEAD: Option<(u8, u8)> = None;

/// Raw keyboard device handle (non-blocking). Opened during init and stored
/// as an AtomicU32 so it can be read without `unsafe`.
pub(crate) static KBD_HANDLE: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// INT 16h — BIOS Keyboard Services
// ---------------------------------------------------------------------------

pub(crate) fn bios_keyboard(regs: &mut VMRegisters) {
    match regs.ah() {
        0x00 | 0x10 => {
            // AH=00/10: Blocking read — wait for a keypress
            // Returns AH=IBM scancode, AL=ASCII character
            // First check lookahead / any non-blocking keyboard data
            if let Some((scancode, ascii)) = read_next_key() {
                regs.set_ah(scancode);
                regs.set_al(ascii);
                return;
            }
            // Block on stdin (console), which supports blocking reads.
            let stdin = Handle::new(0);
            loop {
                let mut buf = [0u8; 1];
                if let Ok(1) = read_sync(stdin, &mut buf, 0) {
                    regs.set_ah(0);
                    regs.set_al(buf[0]);
                    return;
                }
            }
        }
        0x01 | 0x11 => {
            // AH=01/11: Check if key available (non-blocking)
            // If key available: ZF=0, AX=key data (key remains in buffer)
            // If no key: ZF=1
            if let Some((scancode, ascii)) = peek_next_key() {
                regs.set_ah(scancode);
                regs.set_al(ascii);
                // Clear ZF to indicate key available
                regs.eflags &= !0x40;
            } else {
                // Set ZF to indicate no key
                regs.eflags |= 0x40;
                // Yield so the keyboard driver can deliver data
                idos_api::syscall::exec::yield_coop();
            }
        }
        0x02 | 0x12 => {
            // AH=02/12: Get shift key status
            // Return 0 for now (no modifier keys pressed)
            regs.set_al(0);
        }
        _ => {}
    }
}

/// Read and consume the next key press from the keyboard device.
/// Returns (IBM_scancode, ASCII) or None if no key press is available.
fn read_next_key() -> Option<(u8, u8)> {
    // Check lookahead first (populated by peek_next_key)
    unsafe {
        if let Some(key) = KEY_LOOKAHEAD.take() {
            return Some(key);
        }
    }
    let kbd = Handle::new(KBD_HANDLE.load(Ordering::Relaxed));
    let mut buf = [0u8; 2];
    loop {
        match read_sync(kbd, &mut buf, 0) {
            Ok(2) => {
                if buf[0] == 1 {
                    // Key press
                    if let Some(result) = keycode_to_bios(buf[1]) {
                        return Some(result);
                    }
                    // Modifier or unmapped key, skip
                }
                // Key release (buf[0] == 2), skip
            }
            _ => return None,
        }
    }
}

/// Peek at the next key press without consuming it.
/// Since the keyboard device doesn't support peek, we read into a small
/// lookahead buffer that read_next_key also checks.
fn peek_next_key() -> Option<(u8, u8)> {
    unsafe {
        if let Some(ref key) = KEY_LOOKAHEAD {
            return Some(*key);
        }
    }
    if let Some(key) = read_next_key() {
        unsafe {
            KEY_LOOKAHEAD = Some(key);
        }
        Some(key)
    } else {
        None
    }
}

fn keycode_to_bios(keycode: u8) -> Option<(u8, u8)> {
    // KeyCode -> (IBM scancode, ASCII lowercase)
    // IBM scancodes from the standard scan code set 1 make codes
    let (scancode, ascii) = match keycode {
        0x08 => (0x0E, 0x08u8), // Backspace
        0x09 => (0x0F, 0x09),   // Tab
        0x0D => (0x1C, 0x0D),   // Enter
        0x1B => (0x01, 0x1B),   // Escape
        0x20 => (0x39, 0x20),   // Space

        // Numbers 0-9
        0x30 => (0x0B, b'0'),
        0x31 => (0x02, b'1'),
        0x32 => (0x03, b'2'),
        0x33 => (0x04, b'3'),
        0x34 => (0x05, b'4'),
        0x35 => (0x06, b'5'),
        0x36 => (0x07, b'6'),
        0x37 => (0x08, b'7'),
        0x38 => (0x09, b'8'),
        0x39 => (0x0A, b'9'),

        // Letters A-Z (lowercase ASCII)
        0x41 => (0x1E, b'a'),
        0x42 => (0x30, b'b'),
        0x43 => (0x2E, b'c'),
        0x44 => (0x20, b'd'),
        0x45 => (0x12, b'e'),
        0x46 => (0x21, b'f'),
        0x47 => (0x22, b'g'),
        0x48 => (0x23, b'h'),
        0x49 => (0x17, b'i'),
        0x4A => (0x24, b'j'),
        0x4B => (0x25, b'k'),
        0x4C => (0x26, b'l'),
        0x4D => (0x32, b'm'),
        0x4E => (0x31, b'n'),
        0x4F => (0x18, b'o'),
        0x50 => (0x19, b'p'),
        0x51 => (0x10, b'q'),
        0x52 => (0x13, b'r'),
        0x53 => (0x1F, b's'),
        0x54 => (0x14, b't'),
        0x55 => (0x16, b'u'),
        0x56 => (0x2F, b'v'),
        0x57 => (0x11, b'w'),
        0x58 => (0x2D, b'x'),
        0x59 => (0x15, b'y'),
        0x5A => (0x2C, b'z'),

        // Punctuation
        0x2C => (0x33, b','),
        0x2D => (0x0C, b'-'),
        0x2E => (0x34, b'.'),
        0x2F => (0x35, b'/'),
        0x3A => (0x27, b';'),
        0x3B => (0x28, b'\''),
        0x3D => (0x0D, b'='),
        0x5B => (0x1A, b'['),
        0x5C => (0x2B, b'\\'),
        0x5D => (0x1B, b']'),
        0x5F => (0x29, b'`'),

        // Arrow keys (extended, no ASCII)
        0x21 => (0x4B, 0x00), // Left
        0x22 => (0x48, 0x00), // Up
        0x23 => (0x4D, 0x00), // Right
        0x24 => (0x50, 0x00), // Down

        0x07 => (0x53, 0x00), // Delete

        // Modifiers and unmapped keys return None
        _ => return None,
    };
    Some((scancode, ascii))
}

// ---------------------------------------------------------------------------
// INT 10h — BIOS Video Services
// ---------------------------------------------------------------------------

pub(crate) fn bios_video(regs: &mut VMRegisters) {
    let stdout = Handle::new(1);
    match regs.ah() {
        0x00 => {
            // AH=00: Set video mode
            let mode = regs.al() & 0x7F; // bit 7 = don't clear screen
            unsafe {
                VGA_MODE = mode;
            }
            match mode {
                0x03 => {
                    // 80x25 text mode — exit graphics if active
                    unsafe {
                        if crate::graphics::GFX_BUFFER_PADDR != 0 {
                            exit_graphics_mode();
                        }
                    }
                    update_bda_for_mode(0x03);
                    let _ = write_sync(stdout, b"\x1B[2J\x1B[H", 0);
                }
                0x13 => {
                    // 320x200x256 (mode 13h)
                    enter_graphics_mode(320, 200, 8);
                    update_bda_for_mode(0x13);
                }
                _ => {}
            }
        }
        0x02 => {
            // AH=02: Set cursor position
            // BH=page, DH=row (0-based), DL=col (0-based)
            let row = regs.dh() as u32 + 1; // ANSI is 1-based
            let col = regs.dl() as u32 + 1;
            let mut buf = [0u8; 16];
            let len = write_ansi_cursor(&mut buf, row, col);
            let _ = write_sync(stdout, &buf[..len], 0);
        }
        0x06 => {
            // AH=06: Scroll window up
            // AL=lines (0=clear), BH=attribute, CH/CL=top-left, DH/DL=bottom-right
            if regs.al() == 0 {
                let _ = write_sync(stdout, b"\x1B[2J\x1B[H", 0);
            }
        }
        0x09 => {
            // AH=09: Write character and attribute at cursor
            // AL=char, BH=page, BL=attribute, CX=count
            let ch = regs.al();
            let count = (regs.ecx & 0xffff) as usize;
            let buf = [ch];
            for _ in 0..count {
                let _ = write_sync(stdout, &buf, 0);
            }
        }
        0x0E => {
            // AH=0E: Teletype output — write character, advance cursor
            let ch = regs.al();
            let buf = [ch];
            let _ = write_sync(stdout, &buf, 0);
        }
        0x0F => {
            // AH=0F: Get video mode
            let mode = unsafe { VGA_MODE };
            let cols: u8 = if mode == 0x13 { 40 } else { 80 };
            regs.set_ah(cols);
            regs.set_al(mode);
            regs.ebx = regs.ebx & 0xffff00ff; // BH=0 (page 0)
        }
        0x10 => {
            // AH=10: Palette / DAC functions
            bios_palette(regs);
        }
        0x12 => {
            // AH=12h: Alternate select — VGA capability queries
            let bl = regs.bl();
            match bl {
                0x10 => {
                    // BL=10h: Get EGA info — return VGA-compatible values
                    regs.ebx = (regs.ebx & 0xffff0000) | 0x0003; // BH=0 (color), BL=3 (256K)
                    regs.ecx = (regs.ecx & 0xffff0000) | 0x0009; // CH=0 (feature bits), CL=9 (EGA switches)
                }
                0x30 => {
                    // BL=30h: Set vertical resolution / text rows
                    // AL=12h means success (VGA present)
                    regs.set_al(0x12);
                }
                _ => {
                    // Return AL=12h to indicate VGA is present
                    regs.set_al(0x12);
                }
            }
        }
        0x1A => {
            // AH=1Ah: Display combination code
            // AL=00: Get display combination
            if regs.al() == 0x00 {
                // AL=1Ah means function supported (VGA)
                regs.set_al(0x1A);
                // BL=active display code: 08h = VGA color
                // BH=inactive display: 00h = none
                regs.ebx = (regs.ebx & 0xffff0000) | 0x0008;
            }
        }
        0x4F => {
            // AH=4Fh: VESA BIOS Extensions — not supported.
            // VBE convention: AX=004Fh means supported, anything else = not supported.
            regs.set_ax(0x0100); // AH=01 (failed), AL=00 (not supported)
        }
        0x5F | 0x6F => {
            // AH=5Fh/6Fh: Vendor SVGA extensions — not supported.
            regs.set_ax(0x0000);
        }
        _ => {
            let mut buf = [0u8; 32];
            let len = fmt_unsupported(b"INT 10h AH=", regs.ah(), &mut buf);
            dos_log(&buf[..len]);
        }
    }
}

/// INT 10h AH=10h — VGA palette/DAC subfunctions
fn bios_palette(regs: &mut VMRegisters) {
    match regs.al() {
        0x10 => {
            // AL=10h: Set individual DAC register
            // BX=register number, DH=red, CH=green, CL=blue (6-bit VGA values)
            let idx = (regs.ebx & 0xffff) as usize;
            if idx < 256 {
                unsafe {
                    // VGA DAC values are 6-bit (0-63), scale to 8-bit
                    VGA_PALETTE[idx * 3] = (regs.dh() as u16 * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 1] = (regs.ch() as u16 * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 2] = (regs.cl() as u16 * 255 / 63) as u8;
                }
                push_idos_palette();
            }
        }
        0x12 => {
            // AL=12h: Set block of DAC registers
            // BX=first register, CX=count, ES:DX=table of R,G,B bytes (6-bit)
            let first = (regs.ebx & 0xffff) as usize;
            let count = (regs.ecx & 0xffff) as usize;
            let dx = regs.edx & 0xffff;
            let table_addr = (regs.es << 4) + dx;
            let table = table_addr as *const u8;
            for i in 0..count {
                let idx = first + i;
                if idx >= 256 {
                    break;
                }
                unsafe {
                    let r = core::ptr::read_volatile(table.add(i * 3)) as u16;
                    let g = core::ptr::read_volatile(table.add(i * 3 + 1)) as u16;
                    let b = core::ptr::read_volatile(table.add(i * 3 + 2)) as u16;
                    VGA_PALETTE[idx * 3] = (r * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 1] = (g * 255 / 63) as u8;
                    VGA_PALETTE[idx * 3 + 2] = (b * 255 / 63) as u8;
                }
            }
            push_idos_palette();
        }
        0x15 => {
            // AL=15h: Read individual DAC register
            // BX=register number → DH=red, CH=green, CL=blue (6-bit)
            let idx = (regs.ebx & 0xffff) as usize;
            if idx < 256 {
                unsafe {
                    let r = (VGA_PALETTE[idx * 3] as u16 * 63 / 255) as u8;
                    let g = (VGA_PALETTE[idx * 3 + 1] as u16 * 63 / 255) as u8;
                    let b = (VGA_PALETTE[idx * 3 + 2] as u16 * 63 / 255) as u8;
                    regs.edx = (regs.edx & 0xffff00ff) | ((r as u32) << 8);
                    regs.ecx = (regs.ecx & 0xffff0000) | ((g as u32) << 8) | b as u32;
                }
            }
        }
        0x17 => {
            // AL=17h: Read block of DAC registers
            // BX=first register, CX=count, ES:DX=buffer for R,G,B (6-bit)
            let first = (regs.ebx & 0xffff) as usize;
            let count = (regs.ecx & 0xffff) as usize;
            let dx = regs.edx & 0xffff;
            let table_addr = (regs.es << 4) + dx;
            let table = table_addr as *mut u8;
            for i in 0..count {
                let idx = first + i;
                if idx >= 256 {
                    break;
                }
                unsafe {
                    let r = (VGA_PALETTE[idx * 3] as u16 * 63 / 255) as u8;
                    let g = (VGA_PALETTE[idx * 3 + 1] as u16 * 63 / 255) as u8;
                    let b = (VGA_PALETTE[idx * 3 + 2] as u16 * 63 / 255) as u8;
                    core::ptr::write_volatile(table.add(i * 3), r);
                    core::ptr::write_volatile(table.add(i * 3 + 1), g);
                    core::ptr::write_volatile(table.add(i * 3 + 2), b);
                }
            }
        }
        _ => {}
    }
}

/// Write ANSI cursor positioning escape sequence into buf. Returns bytes written.
fn write_ansi_cursor(buf: &mut [u8; 16], row: u32, col: u32) -> usize {
    // Format: ESC [ row ; col H
    buf[0] = 0x1B;
    buf[1] = b'[';
    let mut pos = 2;
    pos += write_u32(&mut buf[pos..], row);
    buf[pos] = b';';
    pos += 1;
    pos += write_u32(&mut buf[pos..], col);
    buf[pos] = b'H';
    pos + 1
}

/// Write a u32 as decimal digits into a byte slice. Returns number of bytes written.
fn write_u32(buf: &mut [u8], mut val: u32) -> usize {
    if val == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut len = 0;
    while val > 0 {
        tmp[len] = b'0' + (val % 10) as u8;
        val /= 10;
        len += 1;
    }
    for i in 0..len {
        buf[i] = tmp[len - 1 - i];
    }
    len
}
