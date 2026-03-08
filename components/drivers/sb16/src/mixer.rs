use idos_api::syscall::memory::{map_memory, unmap_memory};

/// Number of samples in each stream's ring buffer.
/// At 22050Hz mono, 8192 samples ≈ 370ms of audio.
const RING_SAMPLES: usize = 8192;

/// Size of the ring buffer in bytes (16-bit samples)
const RING_BYTES: usize = RING_SAMPLES * 2;

/// Pages needed for one ring buffer
const RING_PAGES: usize = (RING_BYTES + 0xFFF) / 0x1000;

pub struct AudioStream {
    /// Pointer to mmap'd ring buffer of i16 samples
    buffer_ptr: *mut i16,
    buffer_vaddr: u32,
    write_pos: usize,
    read_pos: usize,
    /// Pending write that blocked because the ring buffer was full
    pub pending_write: Option<PendingWrite>,
}

pub struct PendingWrite {
    pub request_id: u32,
    pub buffer_ptr: *const u8,
    pub buffer_len: usize,
    pub bytes_written: usize,
}

impl AudioStream {
    pub fn new() -> Option<Self> {
        let size = (RING_PAGES * 0x1000) as u32;
        let vaddr = map_memory(None, size, None).ok()?;
        let buffer_ptr = vaddr as *mut i16;

        // Zero the buffer
        unsafe {
            let bytes = core::slice::from_raw_parts_mut(vaddr as *mut u8, size as usize);
            bytes.fill(0);
        }

        Some(Self {
            buffer_ptr,
            buffer_vaddr: vaddr,
            write_pos: 0,
            read_pos: 0,
            pending_write: None,
        })
    }

    fn buffer(&self) -> &mut [i16] {
        unsafe { core::slice::from_raw_parts_mut(self.buffer_ptr, RING_SAMPLES) }
    }

    /// Number of samples available to read
    pub fn available(&self) -> usize {
        self.write_pos.wrapping_sub(self.read_pos) % RING_SAMPLES
    }

    /// Space available for writing (in samples)
    pub fn space(&self) -> usize {
        RING_SAMPLES - 1 - self.available()
    }

    /// Write i16 samples into the ring buffer.
    /// Returns the number of samples actually written.
    pub fn write_samples(&mut self, samples: &[i16]) -> usize {
        let space = self.space();
        let count = samples.len().min(space);
        let ptr = self.buffer_ptr;

        for i in 0..count {
            unsafe { *ptr.add(self.write_pos) = samples[i] };
            self.write_pos = (self.write_pos + 1) % RING_SAMPLES;
        }
        count
    }

    /// Read one sample for mixing. Returns 0 (silence) if empty.
    pub fn read_sample(&mut self) -> i16 {
        if self.read_pos == self.write_pos {
            return 0;
        }
        let sample = unsafe { *self.buffer_ptr.add(self.read_pos) };
        self.read_pos = (self.read_pos + 1) % RING_SAMPLES;
        sample
    }

    pub fn is_empty(&self) -> bool {
        self.read_pos == self.write_pos
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        let size = (RING_PAGES * 0x1000) as u32;
        let _ = unmap_memory(self.buffer_vaddr, size);
    }
}

/// Mix samples from all active streams into the output buffer.
/// Adds all streams together and clamps to i16 range.
pub fn mix_into(streams: &mut [Option<AudioStream>], output: &mut [i16]) {
    for sample in output.iter_mut() {
        let mut mixed: i32 = 0;
        for stream_slot in streams.iter_mut() {
            if let Some(stream) = stream_slot {
                mixed += stream.read_sample() as i32;
            }
        }
        // Clamp to i16 range
        *sample = mixed.max(-32768).min(32767) as i16;
    }
}
