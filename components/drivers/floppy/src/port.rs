use core::arch::asm;

#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct Port(u16);

impl Port {
    pub const fn new(number: u16) -> Self {
        Self(number)
    }

    pub fn write_u8(&self, value: u8) {
        unsafe {
            asm!(
                "out dx, al",
                in("dx") self.0,
                in("al") value,
            );
        }
    }

    pub fn read_u8(&self) -> u8 {
        let value: u8;
        unsafe {
            asm!(
                "in al, dx",
                out("al") value,
                in("dx") self.0,
            );
        }
        value
    }
}
