pub mod compositor;
pub mod decor;
pub mod hit;
pub mod scrollbar;
pub mod topbar;
pub mod ui;

use crate::collections::SlotList;
use crate::io::filesystem::install_task_dev;
use crate::task::id::TaskID;
use crate::task::switching::get_current_id;

use super::driver::PendingRead;
use super::graphics::font::Font;
use super::graphics::framebuffer::Framebuffer;
use super::graphics::{Point, Region};
use super::{
    console::Console,
    input::{AltAction, KeyAction, KeyState},
};
use alloc::{collections::VecDeque, vec::Vec};

const COLS: usize = 80;
const ROWS: usize = 25;

/// Information about scrollbar layout returned from draw_window,
/// so the compositor can register hit zones.
pub struct ScrollbarInfo {
    /// Whether a vertical scrollbar was drawn
    pub has_vertical: bool,
    /// Whether a horizontal scrollbar was drawn
    pub has_horizontal: bool,
    /// Screen-relative X of the vertical scrollbar's up arrow
    pub v_x: u16,
    /// Screen-relative Y of the vertical scrollbar's up arrow
    pub v_y: u16,
    /// Height of the vertical scrollbar
    pub v_height: u16,
    /// Screen-relative X of the horizontal scrollbar's left arrow
    pub h_x: u16,
    /// Screen-relative Y of the horizontal scrollbar's left arrow
    pub h_y: u16,
    /// Width of the horizontal scrollbar
    pub h_width: u16,
}

pub struct ConsoleManager {
    key_state: KeyState,
    pub current_console: usize,
    pub consoles: Vec<Console<COLS, ROWS>>,

    /// mapping of open handles to the consoles they reference.
    /// Each entry is (console_id, ref_count). The ref count is incremented
    /// on share (duplicate) and decremented on close; the slot is only
    /// removed when the count reaches zero.
    pub open_io: SlotList<(usize, u32)>,
    pub pending_reads: SlotList<VecDeque<PendingRead>>,
}

impl ConsoleManager {
    pub fn new() -> Self {
        let consoles = Vec::with_capacity(1);

        Self {
            key_state: KeyState::new(),
            current_console: 0,
            consoles,

            open_io: SlotList::new(),
            pending_reads: SlotList::new(),
        }
    }

    pub fn add_console(&mut self) -> usize {
        let new_console = Console::new();
        // the new memory may be any value; make sure it's all space characters
        new_console.terminal.clear_buffer();
        self.consoles.push(new_console);
        let index = self.consoles.len() - 1;

        // each console needs a device driver installed so that programs like
        // the command prompt can read / write to it
        let name = alloc::format!("CON{}", index + 1);
        install_task_dev(&name, get_current_id(), index as u32);

        index
    }

    pub fn attach_reader_task_to_console(&mut self, console_index: usize, task: TaskID) {
        let console = self.consoles.get_mut(console_index).unwrap();
        console.add_reader_task(task);
    }

    /// Take a key action from the keyboard interrupt handler and send it to the
    /// current console for processing. Depending on the key pressed and the
    /// mode of the console, it may trigger a flush. If any content is flushed,
    /// it will also check for pending reads and copy bytes to them.
    ///
    /// Returns `Some(AltAction)` if a window-manager shortcut was detected.
    pub fn handle_key_action(&mut self, action: KeyAction) -> Option<AltAction> {
        // Check for alt-key combos before processing normal input
        if let KeyAction::Press(code) = &action {
            if let Some(alt) = self.key_state.check_alt_action(*code) {
                return Some(alt);
            }
        }

        let mut input_bytes: [u8; 4] = [0; 4];
        let result = self.key_state.process_key_action(action, &mut input_bytes);
        if let Some(len) = result {
            // send input buffer to current console
            let input = &input_bytes[0..len];
            let console: &mut Console<COLS, ROWS> =
                self.consoles.get_mut(self.current_console).unwrap();
            console.send_input(input);

            if console.flushed_input.len() > 0 {
                // if any input was flushed, check for pending reads and complete them
                if let Some(queue) = self.pending_reads.get_mut(self.current_console) {
                    while !queue.is_empty() {
                        let pending_read = queue.pop_front().unwrap();
                        pending_read.complete(&mut console.flushed_input);
                    }
                }
            }
        }
        None
    }

    // Move these to another location:

    pub fn draw_window<F: Font>(
        console: &mut Console<COLS, ROWS>,
        fb: &mut Framebuffer,
        font: &F,
        avail_w: u16,
        avail_h: u16,
        force: bool,
        hover_button: Option<u8>,
        hover_scroll: Option<hit::ScrollDirection>,
        focused: bool,
        bpp: usize,
    ) -> (u16, u16, Option<Region>, Option<ScrollbarInfo>) {
        let window_pos = Point { x: 0, y: 0 };

        // In text mode, skip rendering if nothing changed
        if !force && console.terminal.graphics_buffer.is_none() && !console.dirty {
            return (avail_w, avail_h, None, None);
        }

        let content_y = decor::CONTENT_Y as usize;
        let content_x = decor::CONTENT_X as usize;

        let inner_width = avail_w;
        self::decor::draw_window_bar(fb, window_pos, inner_width, font, &console.title, focused, bpp, hover_button);

        // The outer area is always avail_w × avail_h (border + black fill).
        let outer_w = avail_w as usize;
        let outer_h = avail_h as usize;

        // Fill the entire content area with black
        let buffer = fb.get_buffer_mut();
        for row in 0..outer_h {
            let offset = (content_y + row) * fb.stride as usize + content_x * bpp;
            for px in 0..outer_w {
                crate::console::graphics::write_pixel(buffer, offset + px * bpp, 0x000000, bpp);
            }
        }

        let mut sb_info: Option<ScrollbarInfo> = None;

        if let Some(graphics_buffer) = &console.terminal.graphics_buffer {
            let gfx_w = (graphics_buffer.width as usize).min(outer_w);
            let gfx_h = (graphics_buffer.height as usize).min(outer_h);

            if !force && !console.dirty && graphics_buffer.read_dirty_rect().is_none() {
                return (avail_w, avail_h, None, None);
            }
            graphics_buffer.clear_dirty_rect();

            let copy_width = gfx_w.min(graphics_buffer.width as usize);
            let copy_height = gfx_h.min(graphics_buffer.height as usize);
            let raw_buffer = graphics_buffer.get_pixels();
            let src_bpp = (graphics_buffer.bits_per_pixel + 7) / 8;

            for row in 0..copy_height {
                let dest_offset = (content_y + row) * fb.stride as usize + content_x * bpp;
                let src_offset = row * graphics_buffer.width as usize * src_bpp;

                if src_bpp == bpp {
                    let byte_width = copy_width * bpp;
                    buffer[dest_offset..dest_offset + byte_width]
                        .copy_from_slice(&raw_buffer[src_offset..src_offset + byte_width]);
                } else if src_bpp == 1 {
                    let palette = console.terminal.get_palette();
                    for px in 0..copy_width {
                        let color = palette[raw_buffer[src_offset + px] as usize];
                        crate::console::graphics::write_pixel(buffer, dest_offset + px * bpp, color, bpp);
                    }
                }
            }
        } else {
            let font_row_height = font.get_height() as usize;
            let char_width = font.get_glyph(b'A').map_or(8, |g| g.width as usize);

            // Determine how many rows/cols fit in the full outer area
            let total_text_h = ROWS * font_row_height;
            let total_text_w = COLS * char_width;

            // Decide which scrollbars are needed (may be interdependent)
            let mut need_v = total_text_h > outer_h;
            let mut need_h = total_text_w > outer_w;
            // Adding one scrollbar may cause the other to be needed
            if need_v && !need_h {
                let text_area_w = outer_w - scrollbar::SCROLLBAR_SIZE;
                need_h = total_text_w > text_area_w;
            }
            if need_h && !need_v {
                let text_area_h = outer_h - scrollbar::SCROLLBAR_SIZE;
                need_v = total_text_h > text_area_h;
            }

            // Content area available for text after scrollbar reservation
            let text_area_w = if need_v { outer_w.saturating_sub(scrollbar::SCROLLBAR_SIZE) } else { outer_w };
            let text_area_h = if need_h { outer_h.saturating_sub(scrollbar::SCROLLBAR_SIZE) } else { outer_h };

            let visible_rows = if font_row_height > 0 {
                ROWS.min(text_area_h / font_row_height)
            } else {
                ROWS
            };
            let visible_cols = if char_width > 0 {
                COLS.min(text_area_w / char_width)
            } else {
                COLS
            };

            // Use console's scroll offset, or pin to bottom/right if None
            let max_scroll_row = ROWS.saturating_sub(visible_rows);
            let max_scroll_col = COLS.saturating_sub(visible_cols);
            console.max_scroll_row = max_scroll_row;
            console.max_scroll_col = max_scroll_col;
            let start_row = match console.scroll_row {
                Some(r) => r.min(max_scroll_row),
                None => max_scroll_row,
            };
            let start_col = match console.scroll_col {
                Some(c) => c.min(max_scroll_col),
                None => 0, // default to leftmost for horizontal
            };

            let palette = console.terminal.get_palette();
            for r in 0..visible_rows {
                let src_row = start_row + r;
                let colored_chars = console.row_cells_iter(src_row)
                    .skip(start_col)
                    .take(visible_cols)
                    .map(|cell| {
                        let fg_index = (cell.color.0 & 0x0F) as usize;
                        let bg_index = ((cell.color.0 >> 4) & 0x0F) as usize;
                        (cell.glyph, palette[fg_index], palette[bg_index])
                    });
                font.draw_colored_string(
                    fb,
                    window_pos.x + content_x as u16,
                    (window_pos.y + content_y as u16) + (r as u16 * font_row_height as u16),
                    colored_chars,
                    bpp,
                );
            }

            // Draw scrollbars
            if need_v || need_h {
                let mut info = ScrollbarInfo {
                    has_vertical: need_v,
                    has_horizontal: need_h,
                    v_x: 0, v_y: 0, v_height: 0,
                    h_x: 0, h_y: 0, h_width: 0,
                };

                if need_v {
                    let sb_x = content_x + text_area_w;
                    let sb_y = content_y;
                    let sb_h = text_area_h;
                    let v_hover = match hover_scroll {
                        Some(hit::ScrollDirection::Up) => Some(true),
                        Some(hit::ScrollDirection::Down) => Some(false),
                        _ => None,
                    };
                    scrollbar::draw_vertical(fb, sb_x, sb_y, sb_h, bpp, v_hover);
                    info.v_x = sb_x as u16;
                    info.v_y = sb_y as u16;
                    info.v_height = sb_h as u16;
                }

                if need_h {
                    let sb_x = content_x;
                    let sb_y = content_y + text_area_h;
                    let sb_w = text_area_w;
                    let h_hover = match hover_scroll {
                        Some(hit::ScrollDirection::Left) => Some(true),
                        Some(hit::ScrollDirection::Right) => Some(false),
                        _ => None,
                    };
                    scrollbar::draw_horizontal(fb, sb_x, sb_y, sb_w, bpp, h_hover);
                    info.h_x = sb_x as u16;
                    info.h_y = sb_y as u16;
                    info.h_width = sb_w as u16;
                }

                if need_v && need_h {
                    let corner_x = content_x + text_area_w;
                    let corner_y = content_y + text_area_h;
                    scrollbar::draw_corner(fb, corner_x, corner_y, bpp);
                }

                sb_info = Some(info);
            }
        };

        self::decor::draw_window_border(fb, window_pos, avail_w, avail_h, focused, bpp);

        (
            avail_w,
            avail_h,
            Some(Region {
                x: window_pos.x,
                y: window_pos.y,
                width: avail_w + decor::DECOR_EXTRA_W,
                height: avail_h + decor::DECOR_EXTRA_H,
            }),
            sb_info,
        )
    }

    /// Scroll a console in the given direction, clamping to valid range.
    /// Returns true if the scroll position actually changed.
    pub fn scroll_console(
        &mut self,
        console_index: usize,
        direction: hit::ScrollDirection,
    ) -> bool {
        let console = match self.consoles.get_mut(console_index) {
            Some(c) => c,
            None => return false,
        };

        let max_row = console.max_scroll_row;
        let max_col = console.max_scroll_col;

        match direction {
            hit::ScrollDirection::Up => {
                // None means "pinned to bottom" — unpin to max-1
                let current = console.scroll_row.unwrap_or(max_row);
                if current > 0 {
                    console.scroll_row = Some(current - 1);
                    console.dirty = true;
                    return true;
                }
            }
            hit::ScrollDirection::Down => {
                if let Some(row) = console.scroll_row {
                    if row + 1 >= max_row {
                        // Reached the bottom — snap back to None (follow output)
                        console.scroll_row = None;
                    } else {
                        console.scroll_row = Some(row + 1);
                    }
                    console.dirty = true;
                    return true;
                }
                // None = already at bottom, nothing to do
            }
            hit::ScrollDirection::Left => {
                let current = console.scroll_col.unwrap_or(0);
                if current > 0 {
                    console.scroll_col = Some(current - 1);
                    console.dirty = true;
                    return true;
                }
            }
            hit::ScrollDirection::Right => {
                let current = console.scroll_col.unwrap_or(0);
                if current < max_col {
                    console.scroll_col = Some(current + 1);
                    console.dirty = true;
                    return true;
                }
            }
        }
        false
    }
}
