#[derive(Copy, Clone)]
pub struct E1000Controller {
    mmio_base: u32,
}

impl E1000Controller {
    pub fn new(mmio_base: u32) -> Self {
        Self { mmio_base }
    }

    pub fn write_register(&self, address: u16, command: u32) {
        let ptr = (self.mmio_base + address as u32) as *mut u32;
        unsafe { core::ptr::write_volatile(ptr, command); }
    }

    pub fn read_register(&self, address: u16) -> u32 {
        let ptr = (self.mmio_base + address as u32) as *const u32;
        unsafe { core::ptr::read_volatile(ptr) }
    }

    pub fn set_flags(&self, address: u16, flags: u32) {
        let prev = self.read_register(address);
        self.write_register(address, prev | flags);
    }

    pub fn clear_flags(&self, address: u16, flags: u32) {
        let prev = self.read_register(address);
        self.write_register(address, prev & !flags);
    }

    pub fn get_mac_address(&self) -> [u8; 6] {
        let ral = self.read_register(0x5400);
        let rah = self.read_register(0x5404);
        [
            ral as u8,
            (ral >> 8) as u8,
            (ral >> 16) as u8,
            (ral >> 24) as u8,
            rah as u8,
            (rah >> 8) as u8,
        ]
    }
}
