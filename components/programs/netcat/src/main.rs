#![no_std]
#![no_main]

extern crate idos_api;
extern crate idos_sdk;

use core::fmt::Write;

use idos_api::{
    io::{
        sync::{read_sync, write_sync, open_sync},
        Handle, AsyncOp, ASYNC_OP_READ,
    },
    syscall::{
        io::{append_io_op, block_on_wake_set, create_wake_set},
        net::{create_tcp_handle, create_udp_handle},
    },
};

const STDIN: Handle = Handle::new(0);
const STDOUT: Handle = Handle::new(1);

#[no_mangle]
pub extern "C" fn main() {
    let stdout = STDOUT;

    let config = match build_config() {
        Ok(c) => c,
        Err(err) => {
            let _ = write_sync(stdout, err.as_bytes(), 0);
            let _ = write_sync(stdout, b"\n", 0);
            return;
        }
    };

    if config.udp {
        let socket = create_udp_handle();
        run_udp(stdout, socket, &config);
    } else {
        let socket = create_tcp_handle();
        match config.mode {
            Mode::Listen => run_listen(stdout, socket, &config),
            Mode::Connect => run_connect(stdout, socket, &config),
        }
    }
}

struct Config {
    mode: Mode,
    udp: bool,
    host: [u8; 64],
    host_len: usize,
    port: u16,
}

enum Mode {
    Listen,
    Connect,
}

fn build_config() -> Result<Config, &'static str> {
    let mut args = idos_sdk::env::args();
    // skip argv[0]
    args.next();

    let mut config = Config {
        mode: Mode::Connect,
        udp: false,
        host: [0; 64],
        host_len: 0,
        port: 0,
    };

    let mut got_host = false;

    loop {
        match args.next() {
            Some("-l") => {
                config.mode = Mode::Listen;
            }
            Some("-u") => {
                config.udp = true;
            }
            Some(arg) => {
                if !got_host {
                    if let Mode::Connect = config.mode {
                        let bytes = arg.as_bytes();
                        let len = bytes.len().min(64);
                        config.host[..len].copy_from_slice(&bytes[..len]);
                        config.host_len = len;
                        got_host = true;
                        continue;
                    }
                }
                config.port = parse_u16(arg).ok_or("Invalid port number")?;
            }
            None => break,
        }
    }

    if config.port == 0 {
        return Err("Usage: netcat [-l] [host] <port>");
    }

    if let Mode::Connect = config.mode {
        if !got_host {
            return Err("Must specify a host for connect mode");
        }
    }

    Ok(config)
}

fn parse_u16(s: &str) -> Option<u16> {
    let mut result: u32 = 0;
    for b in s.as_bytes() {
        if *b < b'0' || *b > b'9' {
            return None;
        }
        result = result * 10 + (*b - b'0') as u32;
        if result > 65535 {
            return None;
        }
    }
    if result == 0 {
        return None;
    }
    Some(result as u16)
}

fn run_listen(stdout: Handle, socket: Handle, config: &Config) {
    let mut msg_buf = [0u8; 64];
    let msg_len = fmt_to_buf(&mut msg_buf, format_args!("Listening on port {}...\n", config.port));
    let _ = write_sync(stdout, &msg_buf[..msg_len], 0);

    // Bind to local address
    let bind_result = open_sync(socket, "0.0.0.0", config.port as u32);
    if bind_result.is_err() {
        let _ = write_sync(stdout, b"Failed to bind socket\n", 0);
        return;
    }

    // For a TCP listener, the first read blocks until a connection arrives.
    // The return value is the handle ID of the new connection socket.
    let mut conn_buf = [0u8; 4];
    match read_sync(socket, &mut conn_buf, 0) {
        Ok(new_handle_id) => {
            let _ = write_sync(stdout, b"Connection accepted\n", 0);
            let conn = Handle::new(new_handle_id);
            relay_bidirectional(stdout, conn);
        }
        Err(_) => {
            let _ = write_sync(stdout, b"Accept failed\n", 0);
        }
    }
}

fn run_connect(stdout: Handle, socket: Handle, config: &Config) {
    let host = unsafe { core::str::from_utf8_unchecked(&config.host[..config.host_len]) };
    let mut msg_buf = [0u8; 128];
    let msg_len = fmt_to_buf(&mut msg_buf, format_args!("Connecting to {}:{}...\n", host, config.port));
    let _ = write_sync(stdout, &msg_buf[..msg_len], 0);

    // Open/bind initiates the TCP handshake for remote addresses
    match open_sync(socket, host, config.port as u32) {
        Ok(_) => {
            let _ = write_sync(stdout, b"Connected\n", 0);
        }
        Err(_) => {
            let _ = write_sync(stdout, b"Connection failed\n", 0);
            return;
        }
    }

    relay_bidirectional(stdout, socket);
}

fn run_udp(stdout: Handle, socket: Handle, config: &Config) {
    let host = &config.host[..config.host_len];
    let dest_ip = match parse_ipv4(host) {
        Some(ip) => ip,
        None => {
            let _ = write_sync(stdout, b"UDP requires a numeric IP address\n", 0);
            return;
        }
    };

    // Bind to an ephemeral local port
    if open_sync(socket, "0.0.0.0", 0).is_err() {
        let _ = write_sync(stdout, b"Failed to bind UDP socket\n", 0);
        return;
    }

    let mut msg_buf = [0u8; 64];
    let msg_len = fmt_to_buf(&mut msg_buf, format_args!("Sending UDP to {}:{}\n",
        unsafe { core::str::from_utf8_unchecked(host) }, config.port));
    let _ = write_sync(stdout, &msg_buf[..msg_len], 0);

    // Read lines from stdin and send as UDP datagrams
    let mut input_buf = [0u8; 512];
    loop {
        let n = match read_sync(STDIN, &mut input_buf, 0) {
            Ok(n) => n as usize,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }

        // Build UDP write buffer: [dest_ip:4][dest_port:2 BE][payload]
        let mut write_buf = [0u8; 518]; // 6 header + 512 payload
        write_buf[0..4].copy_from_slice(&dest_ip);
        write_buf[4..6].copy_from_slice(&config.port.to_be_bytes());
        write_buf[6..6 + n].copy_from_slice(&input_buf[..n]);
        let _ = write_sync(socket, &write_buf[..6 + n], 0);
    }
}

/// Parse a dotted-decimal IPv4 address (e.g. "10.0.2.2") into 4 bytes.
fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0;
    let mut current: u16 = 0;
    let mut has_digit = false;

    for &b in s {
        if b == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            if current > 255 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            has_digit = false;
        } else if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            has_digit = true;
        } else {
            return None;
        }
    }

    if !has_digit || octet_idx != 3 || current > 255 {
        return None;
    }
    octets[3] = current as u8;
    Some(octets)
}

/// Relay data between stdin and the socket, using a wake set to multiplex.
fn relay_bidirectional(stdout: Handle, socket: Handle) {
    let stdin = STDIN;

    let wake_set = create_wake_set();

    let mut stdin_buf = [0u8; 512];
    let mut net_buf = [0u8; 512];

    let mut stdin_read = AsyncOp::new(ASYNC_OP_READ, stdin_buf.as_mut_ptr() as u32, stdin_buf.len() as u32, 0);
    append_io_op(stdin, &stdin_read, Some(wake_set));

    let mut net_read = AsyncOp::new(ASYNC_OP_READ, net_buf.as_mut_ptr() as u32, net_buf.len() as u32, 0);
    append_io_op(socket, &net_read, Some(wake_set));

    loop {
        block_on_wake_set(wake_set, None);

        if stdin_read.is_complete() {
            let ret = stdin_read.return_value.load(core::sync::atomic::Ordering::SeqCst);
            if ret & 0x80000000 != 0 || ret == 0 {
                break;
            }
            let _ = write_sync(socket, &stdin_buf[..ret as usize], 0);

            stdin_read = AsyncOp::new(ASYNC_OP_READ, stdin_buf.as_mut_ptr() as u32, stdin_buf.len() as u32, 0);
            append_io_op(stdin, &stdin_read, Some(wake_set));
        }

        if net_read.is_complete() {
            let ret = net_read.return_value.load(core::sync::atomic::Ordering::SeqCst);
            if ret & 0x80000000 != 0 || ret == 0 {
                let _ = write_sync(stdout, b"Connection closed\n", 0);
                break;
            }
            let _ = write_sync(stdout, &net_buf[..ret as usize], 0);

            net_read = AsyncOp::new(ASYNC_OP_READ, net_buf.as_mut_ptr() as u32, net_buf.len() as u32, 0);
            append_io_op(socket, &net_read, Some(wake_set));
        }
    }
}

fn fmt_to_buf(buf: &mut [u8], args: core::fmt::Arguments) -> usize {
    let mut writer = BufWriter { buf, pos: 0 };
    let _ = core::fmt::write(&mut writer, args);
    writer.pos
}

struct BufWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> core::fmt::Write for BufWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        let to_write = bytes.len().min(remaining);
        self.buf[self.pos..self.pos + to_write].copy_from_slice(&bytes[..to_write]);
        self.pos += to_write;
        Ok(())
    }
}
