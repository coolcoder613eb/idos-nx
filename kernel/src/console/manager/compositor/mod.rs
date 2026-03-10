pub mod cursor;

use alloc::vec::Vec;
use core::marker::ConstParamTy;

use self::cursor::Cursor;
use crate::{
    console::graphics::{font::Font, framebuffer::Framebuffer, Region, DESKTOP_BG},
    memory::address::VirtualAddress,
};

use super::decor;
use super::hit::{HitMap, HitTarget};
use super::topbar::{self, TopBarState, TOP_BAR_HEIGHT};
use super::ui::UiSurface;
use super::ConsoleManager;

#[derive(ConstParamTy, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    Color8Bit,
    Color555,
    Color565,
    Color888,
}

impl ColorDepth {
    pub const fn to_usize(&self) -> usize {
        match self {
            ColorDepth::Color8Bit => 1,
            ColorDepth::Color555 => 2,
            ColorDepth::Color565 => 2,
            ColorDepth::Color888 => 3,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    Tiled,
    Floating,
}

struct Window {
    console_index: usize,
    mode: WindowMode,
    x: u16,
    y: u16,
    last_width: u16,
    last_height: u16,
}

/// State for fast window dragging. While active, we skip full re-renders
/// and instead composite a cached window snapshot over a cached background.
struct DragRender {
    /// Window index being dragged
    window_idx: usize,
    /// Snapshot of the dragged window's pixels (row-major, no stride padding)
    window_snapshot: VirtualAddress,
    window_snapshot_size: usize,
    /// Total pixel dimensions of the snapshot (including decorations)
    snap_w: u16,
    snap_h: u16,
    /// Previous position of the dragged window (for erasing)
    prev_x: u16,
    prev_y: u16,
}

pub struct Compositor<const COLOR_DEPTH: ColorDepth> {
    /// Framebuffer representing the graphics memory to draw to
    fb: Framebuffer,
    /// Virtual address of the scratch buffer
    scratch_buffer_vaddr: VirtualAddress,
    /// Size of the scratch buffer
    scratch_buffer_size: usize,

    /// If true, on the next render force redraw of all elements
    pub force_redraw: bool,

    cursor_x: u16,
    cursor_y: u16,
    current_cursor: Cursor,

    dirty_regions: Vec<Region>,

    windows: Vec<Window>,
    /// Index of the focused window (receives keyboard input)
    pub focused_window: usize,
    /// Z-order for floating windows (indices into `windows`, bottom to top)
    float_order: Vec<usize>,

    /// Active drag-rendering state (None when not dragging)
    drag_render: Option<DragRender>,

    pub hit_map: HitMap,
    pub topbar_state: TopBarState,
}

impl<const COLOR_DEPTH: ColorDepth> Compositor<COLOR_DEPTH> {
    pub fn new(fb: Framebuffer) -> Self {
        // allocate double buffer memory
        // stride is already in bytes (VBE pitch), so no need to multiply by bpp
        let scratch_buffer_size =
            (fb.stride as usize) * (fb.height as usize);
        let scratch_page_count = (scratch_buffer_size + 0xfff) / 0x1000;
        let scratch_buffer_size = scratch_page_count * 0x1000;
        let scratch_buffer_vaddr = crate::task::actions::memory::map_memory(
            None,
            scratch_buffer_size as u32,
            crate::task::memory::MemoryBacking::FreeMemory,
        )
        .unwrap();

        let mut compositor = Self {
            fb,
            scratch_buffer_vaddr,
            scratch_buffer_size,
            force_redraw: true,

            cursor_x: 0,
            cursor_y: 0,
            current_cursor: Cursor::new(16, 16, &DEFAULT_CURSOR),

            dirty_regions: Vec::new(),

            windows: Vec::new(),
            focused_window: 0,
            float_order: Vec::new(),

            drag_render: None,

            hit_map: HitMap::new(),
            topbar_state: TopBarState::new(),
        };
        compositor.draw_bg(Region {
            x: 0,
            y: 0,
            width: compositor.fb.width,
            height: compositor.fb.height,
        });

        compositor
    }

    pub fn get_scratch_buffer(&mut self) -> &'static mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.scratch_buffer_vaddr.as_ptr_mut::<u8>(),
                self.scratch_buffer_size,
            )
        }
    }

    pub fn draw_bg(&mut self, region: Region) {
        let buffer = self.get_scratch_buffer();
        let bpp = COLOR_DEPTH.to_usize();
        for row in region.y..(region.y + region.height) {
            let offset = self.fb.stride as usize * row as usize;
            for col in region.x..(region.x + region.width) {
                crate::console::graphics::write_pixel(buffer, offset + col as usize * bpp, DESKTOP_BG, bpp);
            }
        }
    }

    pub fn render<F: Font>(
        &mut self,
        mouse_x: u16,
        mouse_y: u16,
        conman: &mut ConsoleManager,
        font: &F,
    ) {
        self.dirty_regions.clear();
        self.hit_map.clear();

        if self.force_redraw {
            let full = Region {
                x: 0,
                y: 0,
                width: self.fb.width,
                height: self.fb.height,
            };
            self.draw_bg(full);
            self.dirty_regions.push(full);
            self.topbar_state.needs_full_draw = true;
        }

        // Draw the top bar via UiSurface
        {
            let screen_width = self.fb.width;
            let stride = self.fb.stride as usize;
            let bpp = COLOR_DEPTH.to_usize();
            let scratch = unsafe {
                core::slice::from_raw_parts_mut(
                    self.scratch_buffer_vaddr.as_ptr_mut::<u8>(),
                    self.scratch_buffer_size,
                )
            };
            let mut surface = UiSurface::new(
                scratch,
                self.scratch_buffer_vaddr,
                stride,
                screen_width,
                TOP_BAR_HEIGHT,
                bpp,
                &mut self.dirty_regions,
            );
            topbar::draw(
                &mut surface,
                &mut self.hit_map,
                &mut self.topbar_state,
                font,
                screen_width,
            );
        }

        // draw any active windows
        self.draw_windows(conman, font);

        // copy the dirty region of the scratch buffer to the main framebuffer
        self.blit_scratch_to_fb();

        // The cursor is never drawn to the scratch buffer. Once the scratch
        // buffer is ready and has been copied to the screen, we draw the cursor.
        // This guarantees that the "correct" background behind the cursor is
        // always available in the scratch buffer.
        self.redraw_cursor(mouse_x, mouse_y);

        self.force_redraw = false;
    }

    pub fn add_dirty(&mut self, region: Region) {
        for existing in &self.dirty_regions {
            if existing.fully_contains(&region) {
                return;
            }
        }
        self.dirty_regions.push(region);
    }

    fn draw_windows_except<F: Font>(&mut self, conman: &mut ConsoleManager, font: &F, skip: usize) {
        self.draw_windows_inner(conman, font, Some(skip));
    }

    pub fn draw_windows<F: Font>(&mut self, conman: &mut ConsoleManager, font: &F) {
        self.draw_windows_inner(conman, font, None);
    }

    fn draw_windows_inner<F: Font>(&mut self, conman: &mut ConsoleManager, font: &F, skip_idx: Option<usize>) {
        let screen_width = self.fb.width;
        let screen_height = self.fb.height;
        let stride = self.fb.stride;
        let scratch_vaddr = self.scratch_buffer_vaddr;
        let bpp = COLOR_DEPTH.to_usize();

        // Compute tiling geometry
        let desk_h = screen_height - TOP_BAR_HEIGHT;
        let tiled_indices: Vec<usize> = self.windows.iter().enumerate()
            .filter(|(_, w)| w.mode == WindowMode::Tiled)
            .map(|(i, _)| i)
            .collect();
        let tiled_count = tiled_indices.len();

        // Build draw order: tiled windows first, then floating in z-order
        let mut draw_order: Vec<usize> = tiled_indices.clone();
        for &fi in &self.float_order {
            if fi < self.windows.len() && self.windows[fi].mode == WindowMode::Floating {
                draw_order.push(fi);
            }
        }

        for &win_idx in &draw_order {
            if skip_idx == Some(win_idx) {
                continue;
            }
            let window = &self.windows[win_idx];

            // Compute per-window position and available content area
            let (win_x, win_y, avail_w, avail_h) = match window.mode {
                WindowMode::Tiled => {
                    let tile_pos = tiled_indices.iter().position(|&i| i == win_idx).unwrap_or(0);
                    if tiled_count <= 1 {
                        let x = 0u16;
                        let y = TOP_BAR_HEIGHT;
                        let w = screen_width - decor::DECOR_EXTRA_W;
                        let h = desk_h - decor::DECOR_EXTRA_H;
                        (x, y, w, h)
                    } else if tiled_count == 2 {
                        let cell_h = desk_h / 2;
                        let x = 0u16;
                        let y = TOP_BAR_HEIGHT + (tile_pos as u16) * cell_h;
                        let w = screen_width - decor::DECOR_EXTRA_W;
                        let h = cell_h - decor::DECOR_EXTRA_H;
                        (x, y, w, h)
                    } else if tiled_count == 3 {
                        // Top window full-width, bottom two side-by-side
                        let half_h = desk_h / 2;
                        if tile_pos == 0 {
                            let x = 0u16;
                            let y = TOP_BAR_HEIGHT;
                            let w = screen_width - decor::DECOR_EXTRA_W;
                            let h = half_h - decor::DECOR_EXTRA_H;
                            (x, y, w, h)
                        } else {
                            let half_w = screen_width / 2;
                            let col = tile_pos - 1;
                            let x = col as u16 * half_w;
                            let y = TOP_BAR_HEIGHT + half_h;
                            let w = half_w - decor::DECOR_EXTRA_W;
                            let h = half_h - decor::DECOR_EXTRA_H;
                            (x, y, w, h)
                        }
                    } else {
                        // 2x2 grid for 4 tiled windows
                        let cell_w = screen_width / 2;
                        let cell_h = desk_h / 2;
                        let row = tile_pos / 2;
                        let col = tile_pos % 2;
                        let x = col as u16 * cell_w;
                        let y = TOP_BAR_HEIGHT + row as u16 * cell_h;
                        let w = cell_w - decor::DECOR_EXTRA_W;
                        let h = cell_h - decor::DECOR_EXTRA_H;
                        (x, y, w, h)
                    }
                }
                WindowMode::Floating => {
                    (window.x, window.y, 640, 400)
                }
            };

            let mut sub_buffer = Framebuffer {
                width: screen_width - win_x,
                height: screen_height - win_y,
                stride,
                buffer: scratch_vaddr
                    + (win_y as u32 * stride as u32)
                    + win_x as u32 * bpp as u32,
            };

            let console = conman.consoles.get_mut(window.console_index).unwrap();

            // Determine if a button on this window is hovered
            let hover_button = match self.topbar_state.hover {
                Some(HitTarget::WindowButton(idx, btn)) if idx as usize == win_idx => Some(btn),
                _ => None,
            };

            // Determine if a scroll arrow on this window is hovered
            let hover_scroll = match self.topbar_state.hover {
                Some(HitTarget::ScrollArrow(idx, dir)) if idx as usize == win_idx => Some(dir),
                _ => None,
            };

            let focused = win_idx == self.focused_window;

            let (new_width, new_height, dirty_region, scrollbar_info) =
                ConsoleManager::draw_window(console, &mut sub_buffer, font, avail_w, avail_h, self.force_redraw, hover_button, hover_scroll, focused, bpp);

            let window = &mut self.windows[win_idx];
            let screen_dirty = if new_width != window.last_width || new_height != window.last_height
            {
                if !self.force_redraw {
                    if new_width < window.last_width && new_height < window.last_height {
                        draw_bg(
                            &mut sub_buffer,
                            COLOR_DEPTH,
                            Region {
                                x: 0,
                                y: 0,
                                width: window.last_width + decor::DECOR_EXTRA_W,
                                height: window.last_height + decor::DECOR_EXTRA_H,
                            },
                        );
                    } else if new_width < window.last_width {
                        draw_bg(
                            &mut sub_buffer,
                            COLOR_DEPTH,
                            Region {
                                x: new_width + decor::DECOR_EXTRA_W,
                                y: 0,
                                width: window.last_width - new_width,
                                height: new_height,
                            },
                        );
                    } else if new_height < window.last_height {
                        draw_bg(
                            &mut sub_buffer,
                            COLOR_DEPTH,
                            Region {
                                x: 0,
                                y: new_height + decor::DECOR_EXTRA_H,
                                width: new_width,
                                height: window.last_height - new_height,
                            },
                        );
                    }
                }

                let dirty = Region {
                    x: 0,
                    y: 0,
                    width: window.last_width.max(new_width) + decor::DECOR_EXTRA_W,
                    height: window.last_height.max(new_height) + decor::DECOR_EXTRA_H,
                };
                window.last_width = new_width;
                window.last_height = new_height;
                Some(dirty)
            } else {
                dirty_region
            };

            // Remap dirty region to screen space, clamped to screen bounds
            if let Some(r) = screen_dirty {
                let dx = r.x + win_x;
                let dy = r.y + win_y;
                let dirty = Region {
                    x: dx,
                    y: dy,
                    width: r.width.min(screen_width - dx),
                    height: r.height.min(screen_height - dy),
                };
                if !self
                    .dirty_regions
                    .iter()
                    .any(|existing| existing.fully_contains(&dirty))
                {
                    self.dirty_regions.push(dirty);
                }
            }

            // Register hit zones: content area first (lowest priority),
            // then title bar, then buttons on top (reverse iteration in test())
            let inner_width = avail_w;
            let total_width = inner_width + decor::DECOR_EXTRA_W;
            self.hit_map.add(
                Region {
                    x: win_x,
                    y: win_y + decor::CONTENT_Y,
                    width: total_width,
                    height: avail_h + decor::DECOR_EXTRA_H - decor::CONTENT_Y,
                },
                HitTarget::WindowContent(win_idx as u8),
            );
            self.hit_map.add(
                Region {
                    x: win_x,
                    y: win_y,
                    width: total_width,
                    height: decor::WINDOW_BAR_HEIGHT as u16,
                },
                HitTarget::WindowTitleBar(win_idx as u8),
            );
            for btn in 0..decor::BTN_COUNT {
                let rect = decor::button_screen_rect(win_x, win_y, inner_width, btn);
                self.hit_map.add(rect, HitTarget::WindowButton(win_idx as u8, btn as u8));
            }

            // Register scrollbar arrow hit zones (highest priority — added last)
            if let Some(ref sb) = scrollbar_info {
                use super::hit::ScrollDirection;
                let arrow = super::scrollbar::ARROW_SIZE as u16;
                if sb.has_vertical {
                    // Up arrow
                    self.hit_map.add(
                        Region {
                            x: win_x + sb.v_x,
                            y: win_y + sb.v_y,
                            width: arrow,
                            height: arrow,
                        },
                        HitTarget::ScrollArrow(win_idx as u8, ScrollDirection::Up),
                    );
                    // Down arrow
                    let down_y = sb.v_y + sb.v_height - arrow;
                    self.hit_map.add(
                        Region {
                            x: win_x + sb.v_x,
                            y: win_y + down_y,
                            width: arrow,
                            height: arrow,
                        },
                        HitTarget::ScrollArrow(win_idx as u8, ScrollDirection::Down),
                    );
                }
                if sb.has_horizontal {
                    // Left arrow
                    self.hit_map.add(
                        Region {
                            x: win_x + sb.h_x,
                            y: win_y + sb.h_y,
                            width: arrow,
                            height: arrow,
                        },
                        HitTarget::ScrollArrow(win_idx as u8, ScrollDirection::Left),
                    );
                    // Right arrow
                    let right_x = sb.h_x + sb.h_width - arrow;
                    self.hit_map.add(
                        Region {
                            x: win_x + right_x,
                            y: win_y + sb.h_y,
                            width: arrow,
                            height: arrow,
                        },
                        HitTarget::ScrollArrow(win_idx as u8, ScrollDirection::Right),
                    );
                }
            }
        }
    }

    /// Toggle a window between tiled and floating. If the window is floating
    /// and there are already 4 tiled windows, the toggle is refused.
    pub fn try_toggle_window_mode(&mut self, idx: usize) {
        let window = match self.windows.get(idx) {
            Some(w) => w,
            None => return,
        };
        match window.mode {
            WindowMode::Tiled => {
                let window = &mut self.windows[idx];
                window.mode = WindowMode::Floating;
                let content_w = 640 + decor::DECOR_EXTRA_W;
                let content_h = 400 + decor::DECOR_EXTRA_H;
                window.x = (self.fb.width.saturating_sub(content_w)) / 2;
                window.y = TOP_BAR_HEIGHT
                    + (self.fb.height - TOP_BAR_HEIGHT).saturating_sub(content_h) / 2;
                self.float_order.push(idx);
                self.force_redraw = true;
            }
            WindowMode::Floating => {
                let tiled_count = self.windows.iter().filter(|w| w.mode == WindowMode::Tiled).count();
                if tiled_count >= 4 {
                    return; // grid is full
                }
                let window = &mut self.windows[idx];
                window.mode = WindowMode::Tiled;
                self.float_order.retain(|&i| i != idx);
                self.force_redraw = true;
            }
        }
    }

    /// Remove a window and adjust all index references.
    /// Returns the console_index that was associated with the removed window.
    pub fn remove_window(&mut self, idx: usize) -> Option<usize> {
        if idx >= self.windows.len() {
            return None;
        }
        let console_index = self.windows[idx].console_index;
        self.windows.remove(idx);

        // Remove from float_order and adjust remaining indices
        self.float_order.retain(|&i| i != idx);
        for fi in &mut self.float_order {
            if *fi > idx {
                *fi -= 1;
            }
        }

        // Adjust focused_window
        if self.windows.is_empty() {
            self.focused_window = 0;
        } else if self.focused_window == idx {
            // Focus the previous window, or 0 if we removed the first
            self.focused_window = idx.saturating_sub(1);
        } else if self.focused_window > idx {
            self.focused_window -= 1;
        }

        self.force_redraw = true;
        Some(console_index)
    }

    pub fn raise_window(&mut self, idx: usize) {
        if self.windows.get(idx).map_or(false, |w| w.mode == WindowMode::Floating) {
            self.float_order.retain(|&i| i != idx);
            self.float_order.push(idx);
            self.force_redraw = true;
        }
    }

    pub fn focused_console(&self) -> usize {
        self.windows.get(self.focused_window).map_or(0, |w| w.console_index)
    }

    pub fn window_console(&self, window_idx: usize) -> usize {
        self.windows.get(window_idx).map_or(0, |w| w.console_index)
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn move_window(&mut self, idx: usize, x: u16, y: u16) {
        if let Some(window) = self.windows.get_mut(idx) {
            let total_w = window.last_width + decor::DECOR_EXTRA_W;
            let total_h = window.last_height + decor::DECOR_EXTRA_H;
            let max_x = self.fb.width.saturating_sub(total_w);
            let max_y = self.fb.height.saturating_sub(total_h);
            window.x = x.min(max_x);
            window.y = y.clamp(TOP_BAR_HEIGHT, max_y);
            if self.drag_render.is_none() {
                self.force_redraw = true;
            }
        }
    }

    pub fn get_window_x(&self, idx: usize) -> u16 {
        self.windows.get(idx).map_or(0, |w| w.x)
    }

    pub fn get_window_y(&self, idx: usize) -> u16 {
        self.windows.get(idx).map_or(0, |w| w.y)
    }

    pub fn is_window_floating(&self, idx: usize) -> bool {
        self.windows.get(idx).map_or(false, |w| w.mode == WindowMode::Floating)
    }

    /// Begin fast drag rendering. Renders the scene without the dragged
    /// window into the scratch buffer, then snapshots the window's pixels
    /// into a temporary buffer.
    pub fn begin_drag<F: Font>(&mut self, idx: usize, conman: &mut ConsoleManager, font: &F) {
        if idx >= self.windows.len() {
            return;
        }
        let window = &self.windows[idx];
        let snap_w = window.last_width + decor::DECOR_EXTRA_W;
        let snap_h = window.last_height + decor::DECOR_EXTRA_H;
        let bpp = COLOR_DEPTH.to_usize();
        let snap_row_bytes = snap_w as usize * bpp;
        let snap_total = snap_row_bytes * snap_h as usize;
        let snap_pages = (snap_total + 0xfff) / 0x1000;
        let snap_alloc = snap_pages * 0x1000;

        let snap_vaddr = match crate::task::actions::memory::map_memory(
            None,
            snap_alloc as u32,
            crate::task::memory::MemoryBacking::FreeMemory,
        ) {
            Ok(addr) => addr,
            Err(_) => return,
        };

        let win_x = window.x;
        let win_y = window.y;

        // 1. Render the full scene (with the window) into the scratch buffer
        self.force_redraw = true;
        self.draw_windows(conman, font);

        // 2. Copy the window's region from scratch into the snapshot buffer
        let scratch = self.get_scratch_buffer();
        let snap_buf = unsafe {
            core::slice::from_raw_parts_mut(snap_vaddr.as_ptr_mut::<u8>(), snap_alloc)
        };
        let stride = self.fb.stride as usize;
        for row in 0..snap_h as usize {
            let src_y = win_y as usize + row;
            if src_y >= self.fb.height as usize {
                break;
            }
            let src_offset = src_y * stride + win_x as usize * bpp;
            let dst_offset = row * snap_row_bytes;
            snap_buf[dst_offset..dst_offset + snap_row_bytes]
                .copy_from_slice(&scratch[src_offset..src_offset + snap_row_bytes]);
        }

        // 3. Re-render the scene WITHOUT the dragged window into scratch
        //    (this becomes the background for fast compositing)
        let full = Region {
            x: 0, y: 0,
            width: self.fb.width,
            height: self.fb.height,
        };
        self.draw_bg(full);
        // Redraw the top bar
        {
            let screen_width = self.fb.width;
            let stride = self.fb.stride as usize;
            let scratch_ptr = unsafe {
                core::slice::from_raw_parts_mut(
                    self.scratch_buffer_vaddr.as_ptr_mut::<u8>(),
                    self.scratch_buffer_size,
                )
            };
            let mut surface = UiSurface::new(
                scratch_ptr,
                self.scratch_buffer_vaddr,
                stride,
                screen_width,
                TOP_BAR_HEIGHT,
                bpp,
                &mut self.dirty_regions,
            );
            self.topbar_state.needs_full_draw = true;
            topbar::draw(
                &mut surface,
                &mut self.hit_map,
                &mut self.topbar_state,
                font,
                screen_width,
            );
        }
        // Draw all windows EXCEPT the dragged one
        self.draw_windows_except(conman, font, idx);

        self.drag_render = Some(DragRender {
            window_idx: idx,
            window_snapshot: snap_vaddr,
            window_snapshot_size: snap_alloc,
            snap_w,
            snap_h,
            prev_x: win_x,
            prev_y: win_y,
        });
    }

    /// Render a drag frame: blit the background for the old and new window
    /// positions, then composite the window snapshot at its current position.
    pub fn render_drag(&mut self, mouse_x: u16, mouse_y: u16) {
        let dr = match self.drag_render.as_mut() {
            Some(dr) => dr,
            None => return,
        };

        let bpp = COLOR_DEPTH.to_usize();
        let stride = self.fb.stride as usize;
        let screen_w = self.fb.width;
        let screen_h = self.fb.height;

        let window = &self.windows[dr.window_idx];
        let new_x = window.x;
        let new_y = window.y;
        let snap_w = dr.snap_w;
        let snap_h = dr.snap_h;
        let snap_row_bytes = snap_w as usize * bpp;

        let fb_raw = self.fb.get_buffer_mut();
        let scratch_raw = unsafe {
            core::slice::from_raw_parts(
                self.scratch_buffer_vaddr.as_ptr_mut::<u8>() as *const u8,
                self.scratch_buffer_size,
            )
        };

        // Restore background at the OLD position
        let old_x = dr.prev_x;
        let old_y = dr.prev_y;
        for row in 0..snap_h as usize {
            let sy = old_y as usize + row;
            if sy >= screen_h as usize {
                break;
            }
            let offset = sy * stride + old_x as usize * bpp;
            let copy_w = (snap_w as usize).min(screen_w as usize - old_x as usize) * bpp;
            fb_raw[offset..offset + copy_w]
                .copy_from_slice(&scratch_raw[offset..offset + copy_w]);
        }

        // Blit window snapshot at the NEW position
        let snap_buf = unsafe {
            core::slice::from_raw_parts(
                dr.window_snapshot.as_ptr_mut::<u8>() as *const u8,
                dr.window_snapshot_size,
            )
        };
        for row in 0..snap_h as usize {
            let dy = new_y as usize + row;
            if dy >= screen_h as usize {
                break;
            }
            let dst_offset = dy * stride + new_x as usize * bpp;
            let src_offset = row * snap_row_bytes;
            let copy_w = (snap_w as usize).min(screen_w as usize - new_x as usize) * bpp;
            let copy_w = copy_w.min(snap_row_bytes);
            fb_raw[dst_offset..dst_offset + copy_w]
                .copy_from_slice(&snap_buf[src_offset..src_offset + copy_w]);
        }

        // Draw cursor on top
        dr.prev_x = new_x;
        dr.prev_y = new_y;
        self.redraw_cursor(mouse_x, mouse_y);
    }

    /// End fast drag rendering and free the snapshot buffer.
    pub fn end_drag(&mut self) {
        if let Some(dr) = self.drag_render.take() {
            let _ = crate::task::actions::memory::unmap_memory(
                dr.window_snapshot,
                dr.window_snapshot_size as u32,
            );
            self.force_redraw = true;
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_render.is_some()
    }

    pub fn blit_scratch_to_fb(&mut self) {
        let fb_raw = self.fb.get_buffer_mut();
        let scratch_raw = self.get_scratch_buffer();

        for region in &self.dirty_regions {
            for row in 0..region.height {
                let offset = (region.y + row) as usize * self.fb.stride as usize
                    + region.x as usize * COLOR_DEPTH.to_usize();
                let length = region.width as usize * COLOR_DEPTH.to_usize();
                fb_raw[offset..(offset + length)]
                    .copy_from_slice(&scratch_raw[offset..(offset + length)]);
            }
        }
    }

    pub fn redraw_cursor(&mut self, new_x: u16, new_y: u16) {
        let cursor_region: Region = Region {
            x: new_x,
            y: new_y,
            width: self.current_cursor.width as u16,
            height: self.current_cursor.height as u16,
        };
        if new_x == self.cursor_x && new_y == self.cursor_y {
            // if the cursor didn't move, check if any dirty region overwrote
            // the cursor
            let mut needs_redraw = false;
            for region in &self.dirty_regions {
                if region.intersects(&cursor_region) {
                    needs_redraw = true;
                    break;
                }
            }
            if !needs_redraw {
                return;
            }
        }

        let fb_raw = self.fb.get_buffer_mut();
        let clear_start = (self.cursor_y as usize * self.fb.stride as usize)
            + (self.cursor_x as usize * COLOR_DEPTH.to_usize());
        let scratch_buffer = self.get_scratch_buffer();
        // TODO: if the cursor changed height, we need to know the *previous* height here
        let clear_rows = self
            .current_cursor
            .height
            .min(self.fb.height - self.cursor_y) as usize;
        let clear_cols = self.current_cursor.width.min(self.fb.width - self.cursor_x) as usize;
        for row in 0..clear_rows {
            let row_offset = clear_start + row * self.fb.stride as usize;
            let copy_size = clear_cols * COLOR_DEPTH.to_usize();
            fb_raw[row_offset..(row_offset + copy_size)]
                .copy_from_slice(&scratch_buffer[row_offset..(row_offset + copy_size)]);
        }

        let total_rows = self.current_cursor.height.min(self.fb.height - new_y) as usize;
        let total_cols = self.current_cursor.width.min(self.fb.width - new_x) as usize;
        let start =
            (new_y as usize * self.fb.stride as usize) + (new_x as usize * COLOR_DEPTH.to_usize());
        let bpp = COLOR_DEPTH.to_usize();
        for row in 0..total_rows {
            let row_offset = start + row * self.fb.stride as usize;
            let mut cursor_row = self.current_cursor.bitmap[row];
            for col in 0..total_cols {
                if cursor_row & 0x8000 != 0 {
                    crate::console::graphics::write_pixel(fb_raw, row_offset + col * bpp, 0xFFFFFF, bpp);
                }
                cursor_row <<= 1;
            }
        }

        self.cursor_x = new_x;
        self.cursor_y = new_y;
    }

    pub fn add_window(&mut self, console_index: usize) {
        let tiled_count = self.windows.iter().filter(|w| w.mode == WindowMode::Tiled).count();
        let mode = if tiled_count >= 4 {
            WindowMode::Floating
        } else {
            WindowMode::Tiled
        };

        let mut window = Window {
            console_index,
            mode,
            x: 0,
            y: TOP_BAR_HEIGHT,
            last_width: 0,
            last_height: 0,
        };

        if mode == WindowMode::Floating {
            let content_w = 640 + decor::DECOR_EXTRA_W;
            let content_h = 400 + decor::DECOR_EXTRA_H;
            // Offset each floating window slightly so they don't stack exactly
            let float_count = self.float_order.len() as u16;
            let offset = float_count * 20;
            window.x = (self.fb.width.saturating_sub(content_w)) / 2 + offset;
            window.y = TOP_BAR_HEIGHT
                + (self.fb.height - TOP_BAR_HEIGHT).saturating_sub(content_h) / 2
                + offset;
        }

        self.windows.push(window);

        if mode == WindowMode::Floating {
            let idx = self.windows.len() - 1;
            self.float_order.push(idx);
        }

        self.force_redraw = true;
    }
}

fn draw_bg(framebuffer: &mut Framebuffer, color_depth: ColorDepth, region: Region) {
    let buffer = framebuffer.get_buffer_mut();
    let bpp = color_depth.to_usize();
    for row in region.y..(region.y + region.height) {
        let offset = framebuffer.stride as usize * row as usize;
        for col in region.x..(region.x + region.width) {
            crate::console::graphics::write_pixel(buffer, offset + col as usize * bpp, DESKTOP_BG, bpp);
        }
    }
}

static DEFAULT_CURSOR: [u16; 16] = [
    0b1000000000000000,
    0b1100000000000000,
    0b1110000000000000,
    0b1111000000000000,
    0b1111100000000000,
    0b1111110000000000,
    0b1111111000000000,
    0b1111111100000000,
    0b1111111110000000,
    0b1111111111000000,
    0b1111111111100000,
    0b1111111000000000,
    0b1110011000000000,
    0b1100011000000000,
    0b1000001100000000,
    0b0000001100000000,
];
