//! UiSurface — drawing primitives with automatic dirty-rect tracking

use alloc::vec::Vec;

use crate::console::graphics::font::{Font, Glyph};
use crate::console::graphics::framebuffer::Framebuffer;
use crate::console::graphics::{write_pixel, Region};
use crate::memory::address::VirtualAddress;

/// A thin wrapper around a scratch buffer slice that tracks dirty rects as you draw.
pub struct UiSurface<'a> {
    buffer: &'a mut [u8],
    buffer_vaddr: VirtualAddress,
    stride: usize,
    width: u16,
    height: u16,
    bpp: usize,
    dirty: &'a mut Vec<Region>,
}

impl<'a> UiSurface<'a> {
    pub fn new(
        buffer: &'a mut [u8],
        buffer_vaddr: VirtualAddress,
        stride: usize,
        width: u16,
        height: u16,
        bpp: usize,
        dirty: &'a mut Vec<Region>,
    ) -> Self {
        Self {
            buffer,
            buffer_vaddr,
            stride,
            width,
            height,
            bpp,
            dirty,
        }
    }

    /// Fill a rectangle with a solid color.
    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: u32) {
        let x_end = (x + w).min(self.width) as usize;
        let y_end = (y + h).min(self.height) as usize;
        let x = x as usize;
        let y = y as usize;
        let bpp = self.bpp;

        for row in y..y_end {
            let row_offset = row * self.stride;
            for col in x..x_end {
                write_pixel(self.buffer, row_offset + col * bpp, color, bpp);
            }
        }

        self.mark_dirty(x as u16, y as u16, (x_end - x) as u16, (y_end - y) as u16);
    }

    /// Draw a single-pixel horizontal line.
    pub fn draw_hline(&mut self, x: u16, y: u16, w: u16, color: u32) {
        if y >= self.height {
            return;
        }
        let x_end = (x + w).min(self.width) as usize;
        let x = x as usize;
        let bpp = self.bpp;
        let row_offset = y as usize * self.stride;

        for col in x..x_end {
            write_pixel(self.buffer, row_offset + col * bpp, color, bpp);
        }

        self.mark_dirty(x as u16, y, (x_end - x) as u16, 1);
    }

    /// Render a string with transparent background.
    pub fn draw_text<F: Font>(&mut self, font: &F, x: u16, y: u16, text: &[u8], fg: u32) {
        let text_width = font.compute_width(text);
        let text_height = font.get_height() as u16;

        // Build a temporary Framebuffer pointing at our buffer
        let fb = Framebuffer {
            width: self.width,
            height: self.height,
            stride: self.stride as u16,
            buffer: self.buffer_vaddr,
        };

        font.draw_string(&fb, x, y, text.iter().copied(), fg, self.bpp);

        self.mark_dirty(x, y, text_width, text_height);
    }

    /// Render a string with a background color.
    pub fn draw_text_bg<F: Font>(
        &mut self,
        font: &F,
        x: u16,
        y: u16,
        text: &[u8],
        fg: u32,
        bg: u32,
    ) {
        let text_width = font.compute_width(text);
        let text_height = font.get_height() as u16;

        let fb = Framebuffer {
            width: self.width,
            height: self.height,
            stride: self.stride as u16,
            buffer: self.buffer_vaddr,
        };

        let colored = text.iter().map(|&b| (b, fg, bg));
        font.draw_colored_string(&fb, x, y, colored, self.bpp);

        self.mark_dirty(x, y, text_width, text_height);
    }

    fn mark_dirty(&mut self, x: u16, y: u16, w: u16, h: u16) {
        if w == 0 || h == 0 {
            return;
        }
        let region = Region {
            x,
            y,
            width: w,
            height: h,
        };
        // Skip if already fully contained by an existing dirty region
        for existing in self.dirty.iter() {
            if existing.fully_contains(&region) {
                return;
            }
        }
        self.dirty.push(region);
    }
}
