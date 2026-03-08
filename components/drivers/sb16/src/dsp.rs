use crate::port::Port;
use idos_api::syscall::exec::yield_coop;

/// SB16 DSP register offsets from the base I/O address
const DSP_RESET: u16 = 0x06;
const DSP_READ: u16 = 0x0A;
const DSP_WRITE: u16 = 0x0C;
const DSP_READ_STATUS: u16 = 0x0E;
const DSP_ACK_16: u16 = 0x0F;

/// DSP commands
const CMD_SET_SAMPLE_RATE: u8 = 0x41;
const CMD_PLAY_16BIT_AUTO: u8 = 0xB6;
const CMD_STOP_16BIT: u8 = 0xD5;
const CMD_RESUME_16BIT: u8 = 0xD6;
const CMD_GET_VERSION: u8 = 0xE1;

pub struct Dsp {
    base: u16,
}

impl Dsp {
    pub fn new(base: u16) -> Self {
        Self { base }
    }

    fn port(&self, offset: u16) -> Port {
        Port::new(self.base + offset)
    }

    /// Reset the DSP. Returns true if a Sound Blaster was detected.
    pub fn reset(&self) -> bool {
        self.port(DSP_RESET).write_u8(1);
        // Wait ~3 microseconds (the DSP needs at least 3µs)
        for _ in 0..100 {
            yield_coop();
        }
        self.port(DSP_RESET).write_u8(0);

        // Wait for ready byte (0xAA) on the read port
        for _ in 0..1000 {
            if self.port(DSP_READ_STATUS).read_u8() & 0x80 != 0 {
                if self.port(DSP_READ).read_u8() == 0xAA {
                    return true;
                }
            }
            yield_coop();
        }
        false
    }

    fn write_command(&self, value: u8) {
        // Wait until the DSP is ready to accept a command
        for _ in 0..1000 {
            if self.port(DSP_WRITE).read_u8() & 0x80 == 0 {
                self.port(DSP_WRITE).write_u8(value);
                return;
            }
            yield_coop();
        }
    }

    pub fn get_version(&self) -> (u8, u8) {
        self.write_command(CMD_GET_VERSION);
        let major = self.read_data();
        let minor = self.read_data();
        (major, minor)
    }

    fn read_data(&self) -> u8 {
        for _ in 0..1000 {
            if self.port(DSP_READ_STATUS).read_u8() & 0x80 != 0 {
                return self.port(DSP_READ).read_u8();
            }
            yield_coop();
        }
        0
    }

    /// Set the output sample rate in Hz
    pub fn set_sample_rate(&self, rate: u16) {
        self.write_command(CMD_SET_SAMPLE_RATE);
        self.write_command((rate >> 8) as u8);
        self.write_command((rate & 0xFF) as u8);
    }

    /// Start auto-init 16-bit DMA playback.
    /// `half_buffer_samples` is the number of 16-bit samples per DMA half
    /// (the DSP will fire an IRQ after this many samples).
    pub fn start_playback_16bit(&self, half_buffer_samples: u16) {
        // 0xB6 = 16-bit output, auto-init, FIFO on
        // Mode byte: 0x10 = signed mono
        self.write_command(CMD_PLAY_16BIT_AUTO);
        self.write_command(0x10); // signed mono
        // Transfer length = samples per IRQ - 1
        let count = half_buffer_samples - 1;
        self.write_command((count & 0xFF) as u8);
        self.write_command((count >> 8) as u8);
    }

    /// Acknowledge a 16-bit IRQ
    pub fn ack_irq_16(&self) {
        self.port(DSP_ACK_16).read_u8();
    }

    pub fn stop(&self) {
        self.write_command(CMD_STOP_16BIT);
    }

    pub fn resume(&self) {
        self.write_command(CMD_RESUME_16BIT);
    }
}
