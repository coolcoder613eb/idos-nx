pub mod font;
pub mod framebuffer;
pub mod palette;

// UI color constants (0x00RRGGBB format)
pub const COLOR_BLACK: u32 = 0x000000;
pub const COLOR_WHITE: u32 = 0xFFFFFF;
pub const COLOR_GRAY: u32 = 0x606060;
pub const COLOR_DARK_GRAY: u32 = 0x404040;

// Theme palette
pub const BAR_BG: u32 = 0x1a1a2e;
pub const BAR_TEXT: u32 = 0xc8c8d0;
pub const DESKTOP_BG: u32 = 0x2a2a3a;
pub const WIN_BORDER: u32 = 0x5a5a6e;
pub const WIN_TITLEBAR: u32 = 0x3a3a50;
pub const WIN_TITLEBAR_ACTIVE: u32 = 0x4a4a6a;
pub const WIN_TITLE_TEXT: u32 = 0xe0e0e8;
pub const WIN_BODY_BG: u32 = 0x0c0c14;
pub const ACCENT: u32 = 0x6a7abb;
pub const ACCENT_ACTIVE: u32 = 0xe8ecff;
pub const DESKTOP_HOVER_BG: u32 = 0x3a3a50;

#[inline]
pub fn write_pixel(buffer: &mut [u8], offset: usize, color: u32, bytes_per_pixel: usize) {
    match bytes_per_pixel {
        1 => buffer[offset] = color as u8,
        3 => {
            buffer[offset] = color as u8; // B
            buffer[offset + 1] = (color >> 8) as u8; // G
            buffer[offset + 2] = (color >> 16) as u8; // R
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
pub struct Point {
    pub x: u16,
    pub y: u16,
}

#[derive(Clone, Copy)]
pub struct Region {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Region {
    pub fn intersects(&self, other: &Region) -> bool {
        if self.x > other.x + other.width {
            return false;
        }
        if other.x > self.x + self.width {
            return false;
        }
        if self.y > other.y + other.height {
            return false;
        }
        if other.y > self.y + self.height {
            return false;
        }
        true
    }

    pub fn fully_contains(&self, other: &Region) -> bool {
        if self.x > other.x {
            return false;
        }
        if self.x + self.width < other.x + other.width {
            return false;
        }
        if self.y > other.y {
            return false;
        }
        if self.y + self.height < other.y + other.height {
            return false;
        }
        true
    }

    pub fn merge(&self, other: &Region) -> Region {
        let x1 = self.x.min(other.x);
        let y1 = self.y.min(other.y);
        let x2 = (self.x + self.width).max(other.x + other.width);
        let y2 = (self.y + self.height).max(other.y + other.height);
        Region {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        }
    }
}
