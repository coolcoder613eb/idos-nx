#[repr(C)]
pub struct PciDeviceQuery {
    // Input
    pub vendor_id: u16,
    pub device_id: u16,
    // Output
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub irq: u8,
    pub bar: [u32; 6],
}

impl PciDeviceQuery {
    pub fn new(vendor_id: u16, device_id: u16) -> Self {
        Self {
            vendor_id,
            device_id,
            bus: 0,
            device: 0,
            function: 0,
            irq: 0,
            bar: [0; 6],
        }
    }
}

/// Find a PCI device by vendor:device ID. On success, the query struct is
/// filled with bus/device/function, IRQ, and BAR values.
/// Returns true if a matching device was found.
pub fn query_pci_device(query: &mut PciDeviceQuery) -> bool {
    super::syscall(0x60, query as *mut PciDeviceQuery as u32, 0, 0) == 0
}

/// Enable PCI bus mastering for the device at the given bus/device/function.
/// This is required for DMA-capable devices.
pub fn pci_enable_bus_master(bus: u8, device: u8, function: u8) {
    super::syscall(0x61, bus as u32, device as u32, function as u32);
}
