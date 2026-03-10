//! Scrollbar rendering for console windows.
//!
//! Scrollbars appear when the window is too small to display all 80×25 cells.
//! Each scrollbar is 14px wide/tall with 14×14 arrow buttons at each end and
//! an empty gutter (track) in between. A corner square fills the intersection
//! when both scrollbars are visible.

use crate::console::graphics::framebuffer::Framebuffer;
use crate::console::graphics::{write_pixel, SB_ARROW_COLOR, SB_ARROW_HOVER, SB_BG, SB_TRACK};

pub const SCROLLBAR_SIZE: usize = 14;
pub const ARROW_SIZE: usize = 14;

/// Draw a vertical scrollbar on the right edge of the content area.
/// `x` and `y` are the top-left corner of the scrollbar in framebuffer coords.
/// `height` is the total height of the scrollbar (content area height).
pub fn draw_vertical(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    height: usize,
    bpp: usize,
    hover_arrow: Option<bool>, // Some(true) = up hovered, Some(false) = down hovered
) {
    let buffer = fb.get_buffer_mut();
    let stride = fb.stride as usize;

    // Fill the entire scrollbar column with track color
    for row in 0..height {
        let offset = (y + row) * stride + x * bpp;
        for col in 0..SCROLLBAR_SIZE {
            write_pixel(buffer, offset + col * bpp, SB_TRACK, bpp);
        }
    }

    // Up arrow button (top)
    let up_bg = if hover_arrow == Some(true) { SB_ARROW_HOVER } else { SB_BG };
    draw_arrow_button(buffer, stride, x, y, bpp, up_bg, ArrowDir::Up);

    // Down arrow button (bottom)
    if height >= ARROW_SIZE {
        let down_y = y + height - ARROW_SIZE;
        let down_bg = if hover_arrow == Some(false) { SB_ARROW_HOVER } else { SB_BG };
        draw_arrow_button(buffer, stride, x, down_y, bpp, down_bg, ArrowDir::Down);
    }
}

/// Draw a horizontal scrollbar along the bottom of the content area.
pub fn draw_horizontal(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    width: usize,
    bpp: usize,
    hover_arrow: Option<bool>, // Some(true) = left hovered, Some(false) = right hovered
) {
    let buffer = fb.get_buffer_mut();
    let stride = fb.stride as usize;

    // Fill the entire scrollbar row with track color
    for row in 0..SCROLLBAR_SIZE {
        let offset = (y + row) * stride + x * bpp;
        for col in 0..width {
            write_pixel(buffer, offset + col * bpp, SB_TRACK, bpp);
        }
    }

    // Left arrow button
    let left_bg = if hover_arrow == Some(true) { SB_ARROW_HOVER } else { SB_BG };
    draw_arrow_button(buffer, stride, x, y, bpp, left_bg, ArrowDir::Left);

    // Right arrow button
    if width >= ARROW_SIZE {
        let right_x = x + width - ARROW_SIZE;
        let right_bg = if hover_arrow == Some(false) { SB_ARROW_HOVER } else { SB_BG };
        draw_arrow_button(buffer, stride, right_x, y, bpp, right_bg, ArrowDir::Right);
    }
}

/// Draw the corner square where horizontal and vertical scrollbars meet.
pub fn draw_corner(
    fb: &mut Framebuffer,
    x: usize,
    y: usize,
    bpp: usize,
) {
    let buffer = fb.get_buffer_mut();
    let stride = fb.stride as usize;
    for row in 0..SCROLLBAR_SIZE {
        let offset = (y + row) * stride + x * bpp;
        for col in 0..SCROLLBAR_SIZE {
            write_pixel(buffer, offset + col * bpp, SB_BG, bpp);
        }
    }
}

#[derive(Clone, Copy)]
enum ArrowDir {
    Up,
    Down,
    Left,
    Right,
}

/// Draw a 14×14 arrow button (background fill + triangle).
fn draw_arrow_button(
    buffer: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    bpp: usize,
    bg: u32,
    dir: ArrowDir,
) {
    // Fill background
    for row in 0..ARROW_SIZE {
        let offset = (y + row) * stride + x * bpp;
        for col in 0..ARROW_SIZE {
            write_pixel(buffer, offset + col * bpp, bg, bpp);
        }
    }

    // Draw arrow triangle (5×5 centered in 14×14)
    // The demo uses a 10×10 viewBox with triangles like "3,6 7,6 5,2"
    // We'll draw simple pixel triangles centered in the button.
    match dir {
        ArrowDir::Up => {
            // Triangle pointing up: rows from tip to base
            // Center at (7, 4), tip at row 4, base at row 8
            let cx = 7usize;
            for i in 0..5usize {
                let row = 4 + i;
                let offset = (y + row) * stride;
                for col in (cx - i)..=(cx + i) {
                    write_pixel(buffer, offset + (x + col) * bpp, SB_ARROW_COLOR, bpp);
                }
            }
        }
        ArrowDir::Down => {
            // Triangle pointing down
            let cx = 7usize;
            for i in 0..5usize {
                let row = 5 + i;
                let w = 4 - i;
                let offset = (y + row) * stride;
                for col in (cx - w)..=(cx + w) {
                    write_pixel(buffer, offset + (x + col) * bpp, SB_ARROW_COLOR, bpp);
                }
            }
        }
        ArrowDir::Left => {
            // Triangle pointing left
            let cy = 7usize;
            for i in 0..5usize {
                let col = 4 + i;
                let offset_x = (x + col) * bpp;
                for row in (cy - i)..=(cy + i) {
                    let offset = (y + row) * stride + offset_x;
                    write_pixel(buffer, offset, SB_ARROW_COLOR, bpp);
                }
            }
        }
        ArrowDir::Right => {
            // Triangle pointing right
            let cy = 7usize;
            for i in 0..5usize {
                let col = 5 + i;
                let w = 4 - i;
                let offset_x = (x + col) * bpp;
                for row in (cy - w)..=(cy + w) {
                    let offset = (y + row) * stride + offset_x;
                    write_pixel(buffer, offset, SB_ARROW_COLOR, bpp);
                }
            }
        }
    }
}
