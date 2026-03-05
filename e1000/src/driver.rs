use idos_api::syscall::{exec::yield_coop, memory::map_dma_memory};

use crate::controller::E1000Controller;

pub const BUFFER_SIZE: usize = 1024;
pub const RX_DESC_COUNT: usize = 8;
pub const TX_DESC_COUNT: usize = 8;

const REG_CTRL: u16 = 0x00;
const REG_ICR: u16 = 0xc0;
const REG_IMS: u16 = 0xd0;
const REG_RCTL: u16 = 0x100;
const REG_TCTL: u16 = 0x400;
const REG_RDBAL: u16 = 0x2800;
const REG_RDBAH: u16 = 0x2804;
const REG_RDLEN: u16 = 0x2808;
const REG_RDH: u16 = 0x2810;
const REG_RDT: u16 = 0x2818;
const REG_TDBAL: u16 = 0x3800;
const REG_TDBAH: u16 = 0x3804;
const REG_TDLEN: u16 = 0x3808;
const REG_TDH: u16 = 0x3810;
const REG_TDT: u16 = 0x3818;

pub struct DmaRegion {
    pub vaddr: u32,
    pub paddr: u32,
}

pub struct EthernetDriver {
    controller: E1000Controller,

    rx_buffer: DmaRegion,
    tx_buffer: DmaRegion,
    descriptor: DmaRegion,

    rx_ring_index: usize,
    tx_ring_index: usize,
}

impl EthernetDriver {
    pub fn new(controller: E1000Controller) -> Self {
        let (rx_vaddr, rx_paddr) =
            map_dma_memory((BUFFER_SIZE * RX_DESC_COUNT) as u32).unwrap();
        let (tx_vaddr, tx_paddr) =
            map_dma_memory((BUFFER_SIZE * TX_DESC_COUNT) as u32).unwrap();

        let rx_ring_length = core::mem::size_of::<RxDescriptor>() * RX_DESC_COUNT;
        let tx_ring_length = core::mem::size_of::<TxDescriptor>() * TX_DESC_COUNT;
        let (desc_vaddr, desc_paddr) =
            map_dma_memory((rx_ring_length + tx_ring_length) as u32).unwrap();

        // Initialize RX descriptors
        let rd_ring_ptr = desc_vaddr as *mut RxDescriptor;
        for i in 0..RX_DESC_COUNT {
            let desc = unsafe { &mut *rd_ring_ptr.add(i) };
            let offset = (i * BUFFER_SIZE) as u32;
            desc.addr_low = rx_paddr + offset;
            desc.addr_high = 0;
        }

        // Initialize TX descriptors
        let td_ring_offset = rx_ring_length as u32;
        let td_ring_ptr = (desc_vaddr + td_ring_offset) as *mut TxDescriptor;
        for i in 0..TX_DESC_COUNT {
            let desc = unsafe { &mut *td_ring_ptr.add(i) };
            let offset = (i * BUFFER_SIZE) as u32;
            desc.addr_low = tx_paddr + offset;
            desc.addr_high = 0;
        }
        let td_ring_phys = desc_paddr + td_ring_offset;

        // Reset controller
        controller.set_flags(REG_CTRL, 1 << 26);
        while controller.read_register(REG_CTRL) & (1 << 26) != 0 {}

        // Link reset, auto detect speed
        controller.set_flags(REG_CTRL, (1 << 5) | (1 << 6));

        // Set interrupt mask
        controller.write_register(REG_IMS, 0xc0);

        // RX descriptor ring
        controller.write_register(REG_RDBAL, desc_paddr);
        controller.write_register(REG_RDBAH, 0);
        controller.write_register(
            REG_RDLEN,
            (RX_DESC_COUNT * core::mem::size_of::<RxDescriptor>()) as u32,
        );
        controller.write_register(REG_RDH, 0);
        controller.write_register(REG_RDT, RX_DESC_COUNT as u32 - 1);

        // RCTL: Enable; accept unicast, multicast; set packet size to 1024; strip CRC
        controller.clear_flags(REG_RCTL, 3 << 16);
        controller.set_flags(REG_RCTL, (1 << 1) | (1 << 3) | (1 << 15) | (1 << 16) | (1 << 26));

        // TX descriptor ring
        controller.write_register(REG_TDBAL, td_ring_phys);
        controller.write_register(REG_TDBAH, 0);
        let tdlen = TX_DESC_COUNT * core::mem::size_of::<TxDescriptor>();
        controller.write_register(REG_TDLEN, tdlen as u32);
        controller.write_register(REG_TDH, 0);
        controller.write_register(REG_TDT, 0);
        // TCTL: Enable, pad short packets
        controller.set_flags(REG_TCTL, (1 << 1) | (1 << 3));

        // Wait for link
        loop {
            if controller.read_register(0x08) & 2 == 2 {
                break;
            }
            yield_coop();
        }

        Self {
            controller,
            rx_buffer: DmaRegion { vaddr: rx_vaddr, paddr: rx_paddr },
            tx_buffer: DmaRegion { vaddr: tx_vaddr, paddr: tx_paddr },
            descriptor: DmaRegion { vaddr: desc_vaddr, paddr: desc_paddr },
            rx_ring_index: 0,
            tx_ring_index: 0,
        }
    }

    pub fn get_interrupt_cause(&self) -> u32 {
        self.controller.read_register(REG_ICR)
    }

    pub fn get_rx_buffer(&self, index: usize) -> &mut [u8] {
        unsafe {
            let ptr = (self.rx_buffer.vaddr as *mut u8).add(BUFFER_SIZE * index);
            core::slice::from_raw_parts_mut(ptr, BUFFER_SIZE)
        }
    }

    pub fn get_tx_buffer(&self, index: usize) -> &mut [u8] {
        unsafe {
            let ptr = (self.tx_buffer.vaddr as *mut u8).add(BUFFER_SIZE * index);
            core::slice::from_raw_parts_mut(ptr, BUFFER_SIZE)
        }
    }

    pub fn get_rx_descriptor(&self, index: usize) -> &mut RxDescriptor {
        let rd_ring_ptr = self.descriptor.vaddr as *mut RxDescriptor;
        unsafe { &mut *rd_ring_ptr.add(index) }
    }

    pub fn get_tx_descriptor(&self, index: usize) -> &mut TxDescriptor {
        let rd_ring_length = (core::mem::size_of::<RxDescriptor>() * RX_DESC_COUNT) as u32;
        let td_ring_ptr = (self.descriptor.vaddr + rd_ring_length) as *mut TxDescriptor;
        unsafe { &mut *td_ring_ptr.add(index) }
    }

    pub fn get_next_rx_buffer(&self) -> Option<&mut [u8]> {
        let index = self.rx_ring_index;
        let desc = self.get_rx_descriptor(index);
        if !desc.is_done() {
            return None;
        }
        Some(self.get_rx_buffer(index))
    }

    pub fn mark_current_rx_read(&mut self) {
        let index = self.rx_ring_index;
        let desc = self.get_rx_descriptor(index);
        desc.clear_status();
        self.rx_ring_index = Self::next_rdesc_index(index);
        self.controller.write_register(REG_RDT, index as u32);
    }

    fn next_tdesc_index(index: usize) -> usize {
        (index + 1) % TX_DESC_COUNT
    }

    fn next_rdesc_index(index: usize) -> usize {
        (index + 1) % RX_DESC_COUNT
    }

    pub fn tx(&mut self, data: &[u8]) -> usize {
        let mut cur_index = self.tx_ring_index;

        let mut bytes_remaining = data.len();
        let mut bytes_written = 0;
        while bytes_remaining > 0 {
            let tx_buffer = self.get_tx_buffer(cur_index);
            let write_length = bytes_remaining.min(tx_buffer.len());
            tx_buffer[0..write_length]
                .copy_from_slice(&data[bytes_written..(bytes_written + write_length)]);

            bytes_remaining -= write_length;
            bytes_written += write_length;

            let tx_desc = self.get_tx_descriptor(cur_index);
            tx_desc.length = write_length as u16;
            tx_desc.checksum_offset = 0;
            tx_desc.command = if bytes_remaining == 0 {
                0b00001011
            } else {
                0b00001000
            };
            tx_desc.status = 0;
            tx_desc.checksum_start = 0;
            tx_desc.special = 0;

            cur_index = Self::next_tdesc_index(cur_index);
        }
        self.tx_ring_index = cur_index;
        self.controller.write_register(REG_TDT, cur_index as u32);
        bytes_written
    }
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct RxDescriptor {
    addr_low: u32,
    addr_high: u32,
    length: u16,
    checksum: u16,
    status: u8,
    error: u8,
    special: u16,
}

impl RxDescriptor {
    pub fn is_done(&self) -> bool {
        self.status & 1 != 0
    }

    pub fn clear_status(&mut self) {
        self.status = 0;
    }
}

#[repr(C, packed)]
pub struct TxDescriptor {
    addr_low: u32,
    addr_high: u32,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}
