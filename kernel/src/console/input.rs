use crate::hardware::ps2::keycodes::{KeyCode, US_LAYOUT};

pub enum KeyAction {
    Press(u8),
    Release(u8),
}

impl KeyAction {
    pub fn from_raw(action_code: u8, key_code: u8) -> Option<Self> {
        match action_code {
            1 => Some(KeyAction::Press(key_code)),
            2 => Some(KeyAction::Release(key_code)),
            _ => None,
        }
    }
}

/// Alt-key combinations that the console manager intercepts.
pub enum AltAction {
    /// Alt+q: close the focused window
    CloseWindow,
    /// Alt+Return: open a new terminal window
    NewTerminal,
    /// Alt+Tab: rotate focus to the next window
    CycleFocus,
    /// Alt+Space: toggle focused window between tiled and floating
    ToggleFloat,
}

pub struct KeyState {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl KeyState {
    pub fn new() -> KeyState {
        Self {
            ctrl: false,
            shift: false,
            alt: false,
        }
    }

    pub fn process_key_action(&mut self, action: KeyAction, buffer: &mut [u8]) -> Option<usize> {
        match action {
            KeyAction::Press(code) => {
                if code == KeyCode::Shift as u8 {
                    self.shift = true;
                    None
                } else if code == KeyCode::Control as u8 {
                    self.ctrl = true;
                    None
                } else if code == KeyCode::Alt as u8 {
                    self.alt = true;
                    None
                } else {
                    let len = self.key_code_to_ascii(code, buffer);
                    if len > 0 {
                        Some(len)
                    } else {
                        None
                    }
                }
            }
            KeyAction::Release(code) => {
                if code == KeyCode::Shift as u8 {
                    self.shift = false;
                } else if code == KeyCode::Control as u8 {
                    self.ctrl = false;
                } else if code == KeyCode::Alt as u8 {
                    self.alt = false;
                }
                None
            }
        }
    }

    /// Check if a key press is an alt-key combination.
    /// Returns Some(AltAction) if the alt key is held and the key matches.
    pub fn check_alt_action(&self, code: u8) -> Option<AltAction> {
        if !self.alt {
            return None;
        }
        if code == KeyCode::Q as u8 {
            Some(AltAction::CloseWindow)
        } else if code == KeyCode::Enter as u8 {
            Some(AltAction::NewTerminal)
        } else if code == KeyCode::Tab as u8 {
            Some(AltAction::CycleFocus)
        } else if code == KeyCode::Space as u8 {
            Some(AltAction::ToggleFloat)
        } else {
            None
        }
    }

    pub fn key_code_to_ascii(&self, code: u8, buffer: &mut [u8]) -> usize {
        match code {
            // handle non-printable keys here
            _ => {
                let index = code as usize;
                let (normal, shifted) = if index < 0x60 {
                    US_LAYOUT[index]
                } else {
                    (0, 0)
                };
                if self.ctrl {
                    if index < 0x60 {
                        // Control characters are in the range 0x00 to 0x1F
                        buffer[0] = (index & 0x1F) as u8; // Convert to control character
                        return 1;
                    } else {
                        // Non-control characters are ignored when Ctrl is pressed
                        return 0;
                    }
                }
                buffer[0] = if self.shift { shifted } else { normal };
                1
            }
        }
    }
}
