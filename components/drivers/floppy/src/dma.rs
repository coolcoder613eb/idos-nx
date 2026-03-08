use crate::port::Port;

pub struct DmaChannelRegisters {
    start_address: Port,
    count_register: Port,
    page: Port,
    flip_flop_reset: Port,
    multi_mask_prev: u8,
}

impl DmaChannelRegisters {
    pub fn for_channel(channel: u8) -> Self {
        let multi_mask = Port::new(0x0f);
        let multi_mask_prev = multi_mask.read_u8();

        let (start_address, count_register, page) = match channel {
            1 => (Port::new(0x02), Port::new(0x03), Port::new(0x83)),
            2 => (Port::new(0x04), Port::new(0x05), Port::new(0x81)),
            3 => (Port::new(0x06), Port::new(0x07), Port::new(0x82)),
            _ => panic!("invalid channel"),
        };

        let flag = 1u8 << (channel as usize);
        multi_mask.write_u8(multi_mask_prev | flag);

        Self {
            start_address,
            count_register,
            page,
            flip_flop_reset: Port::new(0x0c),
            multi_mask_prev,
        }
    }

    pub fn set_address(&self, paddr: u32) {
        let addr_low = (paddr & 0xff) as u8;
        let addr_mid = ((paddr >> 8) & 0xff) as u8;
        let addr_high = ((paddr >> 16) & 0xff) as u8;

        self.flip_flop_reset.write_u8(0xff);
        self.start_address.write_u8(addr_low);
        self.start_address.write_u8(addr_mid);
        self.page.write_u8(addr_high);
    }

    pub fn set_count(&self, byte_count: u32) {
        let count_low = (byte_count & 0xff) as u8;
        let count_high = ((byte_count >> 8) & 0xff) as u8;

        self.flip_flop_reset.write_u8(0xff);
        self.count_register.write_u8(count_low);
        self.count_register.write_u8(count_high);
    }

    pub fn set_mode(&self, dma_mode: u8) {
        Port::new(0x0b).write_u8(dma_mode);
    }
}

impl Drop for DmaChannelRegisters {
    fn drop(&mut self) {
        Port::new(0x0f).write_u8(self.multi_mask_prev);
    }
}
