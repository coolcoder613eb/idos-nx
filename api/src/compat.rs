/// Exit reasons returned by enter_8086 syscall
pub const VM86_EXIT_GPF: u32 = 0;
pub const VM86_EXIT_DEBUG: u32 = 1;

/// IRQ mask for enter_8086 syscall — bit N = deliver IRQ N as a virtual interrupt
pub const VM86_IRQ_TIMER: u32 = 1 << 0;
pub const VM86_IRQ_KEYBOARD: u32 = 1 << 1;

#[derive(Clone)]
pub struct VMRegisters {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    pub esi: u32,
    pub edi: u32,
    pub ebp: u32,

    pub eip: u32,
    pub cs: u32,
    pub eflags: u32,
    pub esp: u32,
    pub ss: u32,

    pub es: u32,
    pub ds: u32,
    pub fs: u32,
    pub gs: u32,
}

impl VMRegisters {
    pub fn ah(&self) -> u8 {
        ((self.eax & 0xff00) >> 8) as u8
    }

    pub fn al(&self) -> u8 {
        (self.eax & 0xff) as u8
    }

    pub fn set_al(&mut self, al: u8) {
        self.eax &= 0xffffff00;
        self.eax |= al as u32;
    }

    pub fn set_ah(&mut self, ah: u8) {
        self.eax &= 0xffff00ff;
        self.eax |= (ah as u32) << 8;
    }

    pub fn set_ax(&mut self, ax: u16) {
        self.eax &= 0xffff0000;
        self.eax |= ax as u32;
    }

    pub fn dl(&self) -> u8 {
        (self.edx & 0xff) as u8
    }

    pub fn dh(&self) -> u8 {
        ((self.edx & 0xff00) >> 8) as u8
    }

    pub fn bh(&self) -> u8 {
        ((self.ebx & 0xff00) >> 8) as u8
    }

    pub fn bl(&self) -> u8 {
        (self.ebx & 0xff) as u8
    }

    pub fn ch(&self) -> u8 {
        ((self.ecx & 0xff00) >> 8) as u8
    }

    pub fn cl(&self) -> u8 {
        (self.ecx & 0xff) as u8
    }

    pub fn set_cx(&mut self, cx: u16) {
        self.ecx &= 0xffff0000;
        self.ecx |= cx as u32;
    }

    pub fn set_dx(&mut self, dx: u16) {
        self.edx &= 0xffff0000;
        self.edx |= dx as u32;
    }
}
