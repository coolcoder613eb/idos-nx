//! HitZone — mouse interaction hit-testing

use alloc::vec::Vec;

use crate::console::graphics::Region;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    DesktopTab(u8),          // desktop number 1-6
    WindowButton(u8, u8),    // (window_index, button_index: 0=float/tile, 2=close)
    WindowTitleBar(u8),      // (window_index) — drag handle
    WindowContent(u8),       // (window_index) — content area click-to-focus
    ScrollArrow(u8, ScrollDirection), // (window_index, direction)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

pub struct HitZone {
    pub region: Region,
    pub target: HitTarget,
}

pub struct HitMap {
    zones: Vec<HitZone>,
}

impl HitMap {
    pub fn new() -> Self {
        Self {
            zones: Vec::new(),
        }
    }

    /// Clear all zones — call at the start of each render frame.
    pub fn clear(&mut self) {
        self.zones.clear();
    }

    /// Register a clickable zone.
    pub fn add(&mut self, region: Region, target: HitTarget) {
        self.zones.push(HitZone { region, target });
    }

    /// Find what's under the cursor at (x, y).
    pub fn test(&self, x: u16, y: u16) -> Option<HitTarget> {
        for zone in self.zones.iter().rev() {
            let r = &zone.region;
            if x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height {
                return Some(zone.target);
            }
        }
        None
    }
}
