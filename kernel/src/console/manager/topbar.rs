//! Top bar rendering — active window name, desktop tabs, clock

use crate::console::graphics::font::Font;
use crate::console::graphics::{
    Region, ACCENT, ACCENT_ACTIVE, BAR_BG, BAR_TEXT, WIN_BORDER, WIN_TITLE_TEXT,
};

use super::hit::{HitMap, HitTarget};
use super::ui::UiSurface;

pub const TOP_BAR_HEIGHT: u16 = 28;
const BORDER_HEIGHT: u16 = 3;
pub const DESKTOP_COUNT: u8 = 6;
const BUTTON_WIDTH: u16 = 24;
const BUTTONS_TOTAL_WIDTH: u16 = BUTTON_WIDTH * DESKTOP_COUNT as u16; // 144px

pub struct TopBarState {
    pub active_window_name: [u8; 40],
    pub active_name_len: u8,
    pub active_desktop: u8,
    pub occupied_desktops: u8, // bitmask
    pub clock_text: [u8; 5],  // "HH:MM"
    pub hover: Option<HitTarget>,

    // Previous state for dirty optimization
    prev_active_desktop: u8,
    prev_hover: Option<HitTarget>,
    prev_clock: [u8; 5],
    prev_name_len: u8,
    pub needs_full_draw: bool,
}

impl TopBarState {
    pub fn new() -> Self {
        Self {
            active_window_name: [b' '; 40],
            active_name_len: 0,
            active_desktop: 1,
            occupied_desktops: 0x01, // desktop 1 occupied
            clock_text: *b"00:00",
            hover: None,

            prev_active_desktop: 0,
            prev_hover: None,
            prev_clock: [0; 5],
            prev_name_len: 0,
            needs_full_draw: true,
        }
    }

    pub fn set_window_name(&mut self, name: &[u8]) {
        let len = name.len().min(40);
        self.active_window_name[..len].copy_from_slice(&name[..len]);
        for i in len..40 {
            self.active_window_name[i] = b' ';
        }
        self.active_name_len = len as u8;
    }
}

/// Draw the top bar into the surface and register hit zones.
pub fn draw<F: Font>(
    surface: &mut UiSurface,
    hit_map: &mut HitMap,
    state: &mut TopBarState,
    font: &F,
    screen_width: u16,
) {
    let full = state.needs_full_draw;
    let desktop_changed = state.active_desktop != state.prev_active_desktop;
    let hover_changed = state.hover != state.prev_hover;
    let clock_changed = state.clock_text != state.prev_clock;
    let name_changed = state.active_name_len != state.prev_name_len;

    if !full && !desktop_changed && !hover_changed && !clock_changed && !name_changed {
        // Still need to register hit zones even if we skip drawing
        register_hit_zones(hit_map, screen_width);
        return;
    }

    let bar_height = TOP_BAR_HEIGHT - BORDER_HEIGHT;
    let font_height = font.get_height() as u16;
    let text_y = (bar_height.saturating_sub(font_height)) / 2;

    // Center section position
    let buttons_x = (screen_width - BUTTONS_TOTAL_WIDTH) / 2;

    if full {
        // Full background fill
        surface.fill_rect(0, 0, screen_width, bar_height, BAR_BG);
    }

    // Left section: active window name
    if full || name_changed {
        // Clear the left section up to the buttons area
        if !full {
            surface.fill_rect(0, 0, buttons_x, bar_height, BAR_BG);
        }
        if state.active_name_len > 0 {
            let name = &state.active_window_name[..state.active_name_len as usize];
            surface.draw_text(font, 10, text_y, name, WIN_TITLE_TEXT);
        }
        state.prev_name_len = state.active_name_len;
    }

    // Center section: desktop tab buttons
    if full || desktop_changed || hover_changed {
        draw_desktop_buttons(surface, font, state, buttons_x, bar_height, text_y);
        state.prev_active_desktop = state.active_desktop;
        state.prev_hover = state.hover;
    }

    // Right section: clock
    if full || clock_changed {
        let clock_width = font.compute_width(&state.clock_text);
        let clock_x = screen_width - clock_width - 10;
        // Clear the right section from after buttons to edge
        let right_start = buttons_x + BUTTONS_TOTAL_WIDTH;
        if !full {
            surface.fill_rect(right_start, 0, screen_width - right_start, bar_height, BAR_BG);
        }
        surface.draw_text(font, clock_x, text_y, &state.clock_text, WIN_TITLE_TEXT);
        state.prev_clock = state.clock_text;
    }

    // Bottom border: 3px line
    if full {
        for i in 0..BORDER_HEIGHT {
            surface.draw_hline(0, bar_height + i, screen_width, WIN_BORDER);
        }
    }

    state.needs_full_draw = false;

    // Register hit zones
    register_hit_zones(hit_map, screen_width);
}

fn draw_desktop_buttons<F: Font>(
    surface: &mut UiSurface,
    font: &F,
    state: &TopBarState,
    buttons_x: u16,
    bar_height: u16,
    text_y: u16,
) {
    let font_height = font.get_height() as u16;

    // Clear the buttons area
    surface.fill_rect(buttons_x, 0, BUTTONS_TOTAL_WIDTH, bar_height, BAR_BG);

    for i in 0..DESKTOP_COUNT {
        let desktop_num = i + 1;
        let btn_x = buttons_x + i as u16 * BUTTON_WIDTH;
        let is_active = desktop_num == state.active_desktop;
        let is_hovered = state.hover == Some(HitTarget::DesktopTab(desktop_num));
        let is_occupied = state.occupied_desktops & (1 << i) != 0;

        // Desktop number text (ASCII digit)
        let digit = [b'0' + desktop_num];
        let digit_width = font.compute_width(&digit);
        let digit_x = btn_x + (BUTTON_WIDTH - digit_width) / 2;

        let text_color = if is_active {
            ACCENT_ACTIVE
        } else {
            BAR_TEXT
        };
        surface.draw_text(font, digit_x, text_y, &digit, text_color);

        // Active: 3px underline in ACCENT_ACTIVE
        if is_active {
            for j in 0..3u16 {
                surface.draw_hline(btn_x + 2, bar_height - 3 + j, BUTTON_WIDTH - 4, ACCENT_ACTIVE);
            }
        } else if is_hovered {
            // Hovered (non-active): 3px underline in ACCENT
            for j in 0..3u16 {
                surface.draw_hline(btn_x + 2, bar_height - 3 + j, BUTTON_WIDTH - 4, ACCENT);
            }
        }

        // Occupied indicator: 4px wide × 1px dot at bottom center
        if is_occupied && !is_active {
            let dot_x = btn_x + (BUTTON_WIDTH - 4) / 2;
            surface.draw_hline(dot_x, bar_height - 1, 4, BAR_TEXT);
        }
    }
}

fn register_hit_zones(hit_map: &mut HitMap, screen_width: u16) {
    let buttons_x = (screen_width - BUTTONS_TOTAL_WIDTH) / 2;
    let bar_height = TOP_BAR_HEIGHT - BORDER_HEIGHT;

    for i in 0..DESKTOP_COUNT {
        let desktop_num = i + 1;
        let btn_x = buttons_x + i as u16 * BUTTON_WIDTH;
        hit_map.add(
            Region {
                x: btn_x,
                y: 0,
                width: BUTTON_WIDTH,
                height: bar_height,
            },
            HitTarget::DesktopTab(desktop_num),
        );
    }
}
