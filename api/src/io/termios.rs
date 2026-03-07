//! Consoles need to support a lot of IOCTL commands to modify functionality.
//! All of that is handled here.

// consts for supported IOCTL commands

pub const TCGETS: u32 = 0x5401;
pub const TCSETS: u32 = 0x5402;
pub const TCSETSW: u32 = 0x5403;
pub const TCSETSF: u32 = 0x5404;
pub const TIOCGWINSZ: u32 = 0x5413;
pub const TIOCSWINSZ: u32 = 0x5414;

// custom IOCTLs for graphics mode
/// Enable raw graphics mode
/// To enter graphics mode, the user must provide a GraphicsMode struct
/// with the desired width, height, and bpp. The framebuffer field is ignored.
/// On success, the framebuffer field will be filled with the physical address
/// of the framebuffer.
pub const TSETGFX: u32 = 0x6001;
/// Disable raw graphics mode, return to text mode
pub const TSETTEXT: u32 = 0x6002;
/// Get the current 256-color palette (returns 768 bytes of packed R,G,B)
pub const TGETPAL: u32 = 0x6003;
/// Set the current 256-color palette (expects 768 bytes of packed R,G,B)
pub const TSETPAL: u32 = 0x6004;
/// Set the console window title (expects a byte string, max 40 bytes)
pub const TSETTITLE: u32 = 0x6005;

pub const PALETTE_ENTRIES: usize = 256;
pub const PALETTE_SIZE: usize = PALETTE_ENTRIES * 3;

// TERMIOS: structure for getting / setting attributes
#[repr(C, packed)]
#[derive(Clone)]
pub struct Termios {
    pub iflags: u32,
    pub oflags: u32,
    pub cflags: u32,
    pub lflags: u32,
    pub cc: [u8; 20],
}

impl Termios {
    pub const fn default() -> Self {
        Self {
            iflags: 0,
            oflags: 0,
            cflags: 0,
            lflags: 0,
            cc: [0; 20],
        }
    }
}

// TERMIOS: local flags
pub const ISIG: u32 = 0x00000001;
pub const ICANON: u32 = 0x00000002;
pub const ECHO: u32 = 0x00000008;
pub const ECHOE: u32 = 0x00000010;
pub const ECHOK: u32 = 0x00000020;
pub const ECHONL: u32 = 0x00000040;
pub const NOFLSH: u32 = 0x00000080;
pub const TOSTOP: u32 = 0x00000100;

// WINDOW SIZE: structure for getting / setting window size
#[repr(C, packed)]
pub struct WinSize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

// GRAPHICS MODE: structure for getting / setting graphics mode
#[repr(C, packed)]
pub struct GraphicsMode {
    /// requested width of the new buffer
    pub width: u16,
    /// requested height of the new buffer
    pub height: u16,
    /// stores information on the color depth and has room for other flags
    pub bpp_flags: u32,
    /// on return, this field will be set to the physical address of the
    /// graphics framebuffer (including the 8-byte header), and can be mapped
    /// to local memory space to draw graphics to the screen
    pub framebuffer: u32,
}
