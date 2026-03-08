#![no_std]
#![no_main]

extern crate idos_sdk;

use idos_api::io::sync::{close_sync, open_sync, write_sync};
use idos_api::io::Handle;
use idos_api::syscall::io::create_file_handle;

/// Sample rate matching the SB16 driver
const SAMPLE_RATE: u32 = 22050;

/// Frequency of the tone in Hz
const TONE_HZ: u32 = 440;

/// Duration in seconds
const DURATION_SECS: u32 = 3;

/// Total samples to generate
const TOTAL_SAMPLES: u32 = SAMPLE_RATE * DURATION_SECS;

/// Samples per write chunk
const CHUNK_SAMPLES: usize = 512;

/// Fixed-point sine approximation using a parabolic curve.
/// Input: phase in range [0, PERIOD), output: [-32767, 32767]
fn sine_i16(phase: u32, period: u32) -> i16 {
    // Normalize phase to [0, 65536)
    let p = ((phase as u64 * 65536) / period as u64) as u32;

    // Map to [-32768, 32767] using a piecewise parabola
    // Split into quadrants
    let quadrant = p / 16384;
    let frac = (p % 16384) as i32;

    let half = match quadrant {
        0 => {
            // Rising: 0 to peak
            (frac * (16384 - frac)) >> 13
        }
        1 => {
            // Falling: peak to 0
            let f = 16384 - frac;
            (f * (16384 - f)) >> 13
        }
        2 => {
            // Falling: 0 to -peak
            -((frac * (16384 - frac)) >> 13)
        }
        _ => {
            // Rising: -peak to 0
            let f = 16384 - frac;
            -((f * (16384 - f)) >> 13)
        }
    };

    // Scale to i16 range (the parabola peaks at ~8192, scale to ~32767)
    (half * 4).max(-32768).min(32767) as i16
}

#[no_mangle]
pub extern "C" fn main() {
    let stdout = Handle::new(1);

    let audio = create_file_handle();
    match open_sync(audio, "DEV:\\AUDIO", 0) {
        Ok(_) => (),
        Err(_) => {
            let msg = b"Failed to open DEV:\\AUDIO\r\n";
            let _ = write_sync(stdout, msg, 0);
            return;
        }
    }

    let msg = b"Playing 440Hz tone for 3 seconds...\r\n";
    let _ = write_sync(stdout, msg, 0);

    let period = SAMPLE_RATE / TONE_HZ;
    let mut samples_written: u32 = 0;
    let mut buf = [0i16; CHUNK_SAMPLES];

    while samples_written < TOTAL_SAMPLES {
        let remaining = (TOTAL_SAMPLES - samples_written) as usize;
        let chunk = remaining.min(CHUNK_SAMPLES);

        for i in 0..chunk {
            let phase = (samples_written + i as u32) % period;
            buf[i] = sine_i16(phase, period);
        }

        let byte_buf = unsafe {
            core::slice::from_raw_parts(buf.as_ptr() as *const u8, chunk * 2)
        };

        match write_sync(audio, byte_buf, 0) {
            Ok(_) => (),
            Err(_) => {
                let msg = b"Write error\r\n";
                let _ = write_sync(stdout, msg, 0);
                break;
            }
        }

        samples_written += chunk as u32;
    }

    let _ = close_sync(audio);

    let msg = b"Done!\r\n";
    let _ = write_sync(stdout, msg, 0);
}
