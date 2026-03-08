#![no_std]
#![no_main]

extern crate alloc;
extern crate idos_sdk;

mod dma;
mod dsp;
mod mixer;
mod port;

use core::sync::atomic::{AtomicU32, Ordering};

use dsp::Dsp;
use mixer::{mix_into, AudioStream, PendingWrite};
use idos_api::{
    io::{
        driver::DriverCommand,
        error::{IoError, IoResult},
        read_message_op,
        sync::{close_sync, write_sync},
        AsyncOp, Handle, Message, ASYNC_OP_READ,
    },
    syscall::{
        io::{
            append_io_op, block_on_wake_set, create_message_queue_handle, create_wake_set,
            driver_io_complete, open_irq_handle, register_dev,
        },
        memory::map_dma_memory,
    },
};
use idos_sdk::log::SysLogger;

/// Sample rate in Hz
const SAMPLE_RATE: u16 = 22050;

/// DMA buffer size in bytes. Must be a power of two for auto-init DMA.
/// 4096 bytes = 2048 i16 samples = two halves of 1024 samples each.
/// At 22050Hz, each half ≈ 46ms.
const DMA_BUFFER_BYTES: usize = 4096;
const DMA_HALF_BYTES: usize = DMA_BUFFER_BYTES / 2;
const DMA_HALF_SAMPLES: usize = DMA_HALF_BYTES / 2;

/// Maximum number of simultaneous audio streams
const MAX_STREAMS: usize = 8;

/// ISA DMA channel for 16-bit SB16 audio
const DMA_CHANNEL_16BIT: u8 = 5;

struct SB16Driver {
    dsp: Dsp,
    dma_vaddr: u32,
    dma_paddr: u32,
    current_half: bool, // false = first half just played, true = second half
    playing: bool,

    interrupt_handle: Handle,
    irq_wake_set: Handle,
    irq_buf: [u8; 1],

    streams: [Option<AudioStream>; MAX_STREAMS],
    next_instance: AtomicU32,
}

impl SB16Driver {
    fn new(dsp_base: u16, interrupt_handle: Handle) -> Self {
        let (dma_vaddr, dma_paddr) = map_dma_memory(DMA_BUFFER_BYTES as u32).unwrap();
        let irq_wake_set = create_wake_set();

        // Zero the DMA buffer
        let dma_buf = unsafe {
            core::slice::from_raw_parts_mut(dma_vaddr as *mut u8, DMA_BUFFER_BYTES)
        };
        dma_buf.fill(0);

        Self {
            dsp: Dsp::new(dsp_base),
            dma_vaddr,
            dma_paddr,
            current_half: false,
            playing: false,

            interrupt_handle,
            irq_wake_set,
            irq_buf: [0],

            streams: Default::default(),
            next_instance: AtomicU32::new(1),
        }
    }

    /// Get the DMA half that just finished playing and needs to be refilled.
    /// Returns a raw slice to avoid borrowing self.
    fn finished_half(&self) -> &mut [i16] {
        unsafe {
            let base = self.dma_vaddr as *mut i16;
            let ptr = if self.current_half {
                base
            } else {
                base.add(DMA_HALF_SAMPLES)
            };
            core::slice::from_raw_parts_mut(ptr, DMA_HALF_SAMPLES)
        }
    }

    fn init(&mut self, log: &mut SysLogger) -> bool {
        if !self.dsp.reset() {
            log.log("SB16: DSP reset failed");
            return false;
        }

        let (major, minor) = self.dsp.get_version();
        log.log_fmt(format_args!("SB16: DSP version {}.{}", major, minor));

        if major < 4 {
            log.log("SB16: need DSP version 4+ for SB16 features");
            return false;
        }

        self.dsp.set_sample_rate(SAMPLE_RATE);
        true
    }

    fn start_playback(&mut self) {
        if self.playing {
            return;
        }

        // Program 16-bit DMA channel 5
        // DMA channel 5 uses ports on the second DMA controller (16-bit)
        self.setup_dma_16bit();

        // Start auto-init 16-bit playback
        self.dsp.start_playback_16bit(DMA_HALF_SAMPLES as u16);
        self.playing = true;
    }

    fn setup_dma_16bit(&self) {
        // 16-bit DMA uses the second controller (channels 4-7)
        // Channel 5 registers:
        //   Base+count: 0xC4/0xC6
        //   Page: 0x8B
        //   Mask: 0xD4
        //   Mode: 0xD6
        //   Clear flip-flop: 0xD8

        let mask = port::Port::new(0xD4);
        let mode = port::Port::new(0xD6);
        let flip_flop = port::Port::new(0xD8);
        let base_addr = port::Port::new(0xC4);
        let count = port::Port::new(0xC6);
        let page = port::Port::new(0x8B);

        // Mask channel 5 (bit 0 = channel select, bit 2 = mask)
        mask.write_u8(0x05); // mask channel 5 (4 + 1)

        // Mode: channel 1 (of the second controller), auto-init, read (memory→device), single
        // Channel 5 is channel 1 on the second DMA controller
        // 0x59 = 01 01 10 01 = single / auto-init / read / channel 1
        mode.write_u8(0x59);

        // 16-bit DMA addresses are in 16-bit words, shifted right by 1
        let addr_words = self.dma_paddr >> 1;
        let count_words = (DMA_BUFFER_BYTES / 2) as u32 - 1; // in 16-bit transfers

        flip_flop.write_u8(0xFF);
        base_addr.write_u8((addr_words & 0xFF) as u8);
        base_addr.write_u8(((addr_words >> 8) & 0xFF) as u8);

        page.write_u8(((self.dma_paddr >> 16) & 0xFF) as u8);

        flip_flop.write_u8(0xFF);
        count.write_u8((count_words & 0xFF) as u8);
        count.write_u8(((count_words >> 8) & 0xFF) as u8);

        // Unmask channel 5
        mask.write_u8(0x01); // unmask channel 1 on second controller
    }

    fn handle_irq(&mut self) {
        self.dsp.ack_irq_16();

        // Get the raw pointer to the finished half before borrowing streams
        let half_ptr = if self.current_half {
            self.dma_vaddr as *mut i16
        } else {
            unsafe { (self.dma_vaddr as *mut i16).add(DMA_HALF_SAMPLES) }
        };
        let half = unsafe { core::slice::from_raw_parts_mut(half_ptr, DMA_HALF_SAMPLES) };

        mix_into(&mut self.streams, half);
        self.current_half = !self.current_half;

        // Try to complete any pending writes that were blocked
        for slot in self.streams.iter_mut() {
            let stream = match slot {
                Some(s) => s,
                None => continue,
            };
            if let Some(pending) = stream.pending_write.take() {
                let remaining_len = pending.buffer_len - pending.bytes_written;
                if remaining_len == 0 {
                    driver_io_complete(pending.request_id, Ok(pending.buffer_len as u32));
                    continue;
                }

                let samples = unsafe {
                    let ptr = pending.buffer_ptr.add(pending.bytes_written) as *const i16;
                    core::slice::from_raw_parts(ptr, remaining_len / 2)
                };

                let written = stream.write_samples(samples);
                let new_bytes_written = pending.bytes_written + written * 2;

                if new_bytes_written >= pending.buffer_len {
                    driver_io_complete(pending.request_id, Ok(pending.buffer_len as u32));
                } else {
                    stream.pending_write = Some(PendingWrite {
                        request_id: pending.request_id,
                        buffer_ptr: pending.buffer_ptr,
                        buffer_len: pending.buffer_len,
                        bytes_written: new_bytes_written,
                    });
                }
            }
        }
    }

    fn has_active_streams(&self) -> bool {
        self.streams.iter().any(|s| s.is_some())
    }

    fn open(&mut self) -> IoResult {
        // Find a free stream slot
        let slot_index = self.streams.iter().position(|s| s.is_none());
        let slot_index = match slot_index {
            Some(i) => i,
            None => return Err(IoError::OperationFailed),
        };

        let stream = AudioStream::new().ok_or(IoError::OperationFailed)?;
        self.streams[slot_index] = Some(stream);

        let instance = self.next_instance.fetch_add(1, Ordering::SeqCst);
        Ok(instance | ((slot_index as u32) << 16))
    }

    fn write_audio(
        &mut self,
        instance: u32,
        buffer: &[u8],
        request_id: u32,
    ) -> Option<IoResult> {
        let slot_index = (instance >> 16) as usize;

        // Validate the stream exists
        if !matches!(self.streams.get(slot_index), Some(Some(_))) {
            return Some(Err(IoError::FileHandleInvalid));
        }

        // Start playback on first write (before borrowing streams)
        if !self.playing {
            self.start_playback();
        }

        let stream = self.streams[slot_index].as_mut().unwrap();

        // Interpret the buffer as i16 samples
        let samples = unsafe {
            let ptr = buffer.as_ptr() as *const i16;
            core::slice::from_raw_parts(ptr, buffer.len() / 2)
        };

        let written = stream.write_samples(samples);
        let bytes_written = written * 2;

        if bytes_written >= buffer.len() {
            Some(Ok(buffer.len() as u32))
        } else {
            // Ring buffer is full — block the write
            stream.pending_write = Some(PendingWrite {
                request_id,
                buffer_ptr: buffer.as_ptr(),
                buffer_len: buffer.len(),
                bytes_written,
            });
            None // Don't complete yet
        }
    }

    fn close_stream(&mut self, instance: u32) -> IoResult {
        let slot_index = (instance >> 16) as usize;
        match self.streams.get_mut(slot_index) {
            Some(slot @ Some(_)) => {
                *slot = None;
                if !self.has_active_streams() {
                    self.stop_playback();
                }
                Ok(1)
            }
            _ => Err(IoError::FileHandleInvalid),
        }
    }

    fn stop_playback(&mut self) {
        if !self.playing {
            return;
        }
        self.dsp.stop();
        self.playing = false;
    }
}

fn parse_u8(s: &str) -> u8 {
    let mut result: u8 = 0;
    for b in s.bytes() {
        if b >= b'0' && b <= b'9' {
            result = result.wrapping_mul(10).wrapping_add(b - b'0');
        }
    }
    result
}

fn parse_u16_hex(s: &str) -> u16 {
    let mut result: u16 = 0;
    let s = if s.starts_with("0x") || s.starts_with("0X") {
        &s[2..]
    } else {
        s
    };
    for b in s.bytes() {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => continue,
        };
        result = result.wrapping_mul(16).wrapping_add(digit as u16);
    }
    result
}

#[no_mangle]
pub extern "C" fn main() {
    // Handle 0 = response pipe writer (transferred by kernel)
    let response_writer = Handle::new(0);

    let mut log = SysLogger::new("SB16");

    // argv: path, irq, [base_port]
    let mut args = idos_sdk::env::args();
    let _path = args.next();
    let irq_str = args.next().unwrap_or("5");
    let irq = parse_u8(irq_str);
    let base_str = args.next().unwrap_or("0x220");
    let base_port = parse_u16_hex(base_str);

    log.log_fmt(format_args!("SB16: IRQ={} base={:#X}", irq, base_port));

    let interrupt_handle = open_irq_handle(irq);
    let mut driver = SB16Driver::new(base_port, interrupt_handle);

    if !driver.init(&mut log) {
        log.log("SB16: initialization failed, exiting");
        let _ = write_sync(response_writer, &[0], 0);
        let _ = close_sync(response_writer);
        return;
    }

    register_dev("AUDIO");
    log.log("SB16: registered DEV:\\AUDIO");

    // Signal ready
    let _ = write_sync(response_writer, &[1], 0);
    let _ = close_sync(response_writer);

    // Event loop: listen for IRQs and driver messages
    let messages_handle = create_message_queue_handle();
    let wake_set = create_wake_set();

    let mut incoming_message = Message::empty();
    let mut interrupt_ready: [u8; 1] = [0];

    let mut message_read = read_message_op(&mut incoming_message);
    append_io_op(messages_handle, &message_read, Some(wake_set));
    let mut interrupt_read =
        AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
    append_io_op(interrupt_handle, &interrupt_read, Some(wake_set));

    loop {
        if interrupt_read.is_complete() {
            let _ = write_sync(interrupt_handle, &[], 0);
            driver.handle_irq();

            interrupt_read =
                AsyncOp::new(ASYNC_OP_READ, interrupt_ready.as_mut_ptr() as u32, 1, 0);
            append_io_op(interrupt_handle, &interrupt_read, Some(wake_set));
        } else if message_read.is_complete() {
            let request_id = incoming_message.unique_id;

            match DriverCommand::from_u32(incoming_message.message_type) {
                DriverCommand::OpenRaw => {
                    let result = driver.open();
                    driver_io_complete(request_id, result);
                }
                DriverCommand::Write => {
                    let instance = incoming_message.args[0];
                    let buffer_ptr = incoming_message.args[1] as *const u8;
                    let buffer_len = incoming_message.args[2] as usize;
                    let buffer =
                        unsafe { core::slice::from_raw_parts(buffer_ptr, buffer_len) };

                    if let Some(result) = driver.write_audio(instance, buffer, request_id) {
                        driver_io_complete(request_id, result);
                    }
                    // If None, the write is pending — will be completed on IRQ
                }
                DriverCommand::Close => {
                    let instance = incoming_message.args[0];
                    let result = driver.close_stream(instance);
                    driver_io_complete(request_id, result);
                }
                _ => {
                    driver_io_complete(request_id, Err(IoError::UnsupportedOperation));
                }
            }

            message_read = read_message_op(&mut incoming_message);
            append_io_op(messages_handle, &message_read, Some(wake_set));
        } else {
            block_on_wake_set(wake_set, None);
        }
    }
}
