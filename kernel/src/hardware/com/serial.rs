use crate::arch::port::Port;

const STATUS_TRANSMIT_BUFFER_EMPTY: u8 = 1 << 5;
const STATUS_DATA_READY: u8 = 1;

/// How many bytes the 16550 FIFO can accept in one burst
const FIFO_SIZE: usize = 16;
/// Size of the software ring buffer backing each port
const BUFFER_SIZE: usize = 256;

#[allow(dead_code)]
pub struct SerialPort {
    /// Writing to data sends to the transmit buffer, reading pulls from the
    /// receive buffer
    data: Port,
    /// Bitmap enabling various interrupts when serial state changes.
    ///   Bit 0 - Triggered when data available
    ///   Bit 1 - Triggered when transmit buffer is empty
    ///   Bit 2 - Triggered on error
    ///   Bit 3 - Triggered on status change
    interrupt_enable: Port,
    /// Reading from this port is used to identify the current interrupt, as
    /// well as properties of the UART device.
    /// Writing to it changes how the buffers behave
    fifo_control: Port,
    /// Determines the behavior and format of data on the wire
    line_control: Port,
    /// Gives direct control of the hardware transmitting and receiving data
    modem_control: Port,
    ///
    line_status: Port,
    modem_status: Port,
}

impl SerialPort {
    pub fn new(base_port: u16) -> Self {
        Self {
            data: Port::new(base_port),
            interrupt_enable: Port::new(base_port + 1),
            fifo_control: Port::new(base_port + 2),
            line_control: Port::new(base_port + 3),
            modem_control: Port::new(base_port + 4),
            line_status: Port::new(base_port + 5),
            modem_status: Port::new(base_port + 6),
        }
    }

    pub fn init(&self) {
        // disable interrupts until we get them working
        self.interrupt_enable.write_u8(0);

        // Enable divisor latch access, allowing the baud rate to be changed
        self.line_control.write_u8(0x80);
        // With DLAB enabled, the data register accesses the low 8 bits of the
        // internal divisor, and the interrupt register accesses the high bits
        self.data.write_u8(0x01); // 115200 / 1 = 115,200 baud
        self.interrupt_enable.write_u8(0);

        // Set a standard 8n1 protocol: 8 bits, no parity, 1 stop bit
        self.line_control.write_u8(0x03);

        // Enable FIFO buffers: set the highest buffer size, clear the buffers,
        // and enable them.
        self.fifo_control.write_u8(0xc7);

        // Enable Aux Output 2 so that interrupts can work;
        self.modem_control.write_u8(0x08);

        // Enable interrupt for data available
        self.interrupt_enable.write_u8(1);
    }

    fn is_transmitting(&self) -> bool {
        (self.line_status.read_u8() & STATUS_TRANSMIT_BUFFER_EMPTY) == 0
    }

    pub fn send_byte(&self, byte: u8) {
        // Bounded wait: if the UART never becomes ready (eg. missing hardware
        // or an emulator that doesn't implement 16550), give up rather than
        // hanging the kernel.
        let mut timeout = 100_000u32;
        while self.is_transmitting() {
            timeout -= 1;
            if timeout == 0 {
                return;
            }
        }
        self.data.write_u8(byte);
    }

    fn has_data(&self) -> bool {
        (self.line_status.read_u8() & STATUS_DATA_READY) != 0
    }

    fn read_byte(&self) -> Option<u8> {
        if self.has_data() {
            Some(self.data.read_u8())
        } else {
            None
        }
    }
}

/// Used only during early boot, before the buffered port is initialized
impl core::fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            self.send_byte(byte);
        }
        Ok(())
    }
}

/// A serial port wrapped with a software ring buffer that batches writes
/// through the 16550 FIFO in 16-byte bursts, reducing status register polls
/// by up to 16x.
pub struct BufferedSerialPort {
    port: SerialPort,
    buffer: [u8; BUFFER_SIZE],
    read_idx: usize,
    write_idx: usize,
}

impl BufferedSerialPort {
    fn new(port: SerialPort) -> Self {
        Self {
            port,
            buffer: [0; BUFFER_SIZE],
            read_idx: 0,
            write_idx: 0,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        for &byte in data {
            let next = (self.write_idx + 1) % BUFFER_SIZE;
            if next == self.read_idx {
                // buffer full, flush to make room
                self.flush();
                if (self.write_idx + 1) % BUFFER_SIZE == self.read_idx {
                    // still full (hardware stuck), drop the rest
                    return;
                }
            }
            self.buffer[self.write_idx] = byte;
            self.write_idx = next;
        }
        self.flush();
    }

    pub fn flush(&mut self) {
        while self.read_idx != self.write_idx {
            let mut timeout = 100_000u32;
            while self.port.is_transmitting() {
                timeout -= 1;
                if timeout == 0 {
                    return;
                }
            }
            // Burst up to FIFO_SIZE bytes into the hardware FIFO
            for _ in 0..FIFO_SIZE {
                if self.read_idx == self.write_idx {
                    break;
                }
                self.port.data.write_u8(self.buffer[self.read_idx]);
                self.read_idx = (self.read_idx + 1) % BUFFER_SIZE;
            }
        }
    }

    pub fn has_data(&self) -> bool {
        self.port.has_data()
    }

    pub fn read_byte(&self) -> Option<u8> {
        self.port.read_byte()
    }
}

impl core::fmt::Write for BufferedSerialPort {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.push(s.as_bytes());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Global storage: up to 2 COM ports, indexed 0 (COM1) and 1 (COM2)
// ---------------------------------------------------------------------------

static mut COM_PORTS: [Option<BufferedSerialPort>; 4] = [None, None, None, None];

/// Initialize a COM port and register it in the global table.
/// Must be called before any `with_port` access for this index.
pub fn init_port(index: usize, base_port: u16) {
    assert!(index < 4);
    let port = SerialPort::new(base_port);
    port.init();
    unsafe {
        COM_PORTS[index] = Some(BufferedSerialPort::new(port));
    }
}

/// Access a buffered COM port by index. Returns None if the port has not been
/// initialized. Safe to call from kernel context (no preemption).
pub fn with_port<F, R>(index: usize, f: F) -> Option<R>
where
    F: FnOnce(&mut BufferedSerialPort) -> R,
{
    unsafe { COM_PORTS[index].as_mut().map(f) }
}

/// Returns true if the given port index has been initialized.
pub fn port_exists(index: usize) -> bool {
    unsafe { COM_PORTS.get(index).is_some_and(|p| p.is_some()) }
}
