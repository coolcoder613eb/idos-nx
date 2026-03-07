#![no_std]
#![no_main]

extern crate alloc;
extern crate idos_api;
extern crate idos_sdk;

use idos_api::io::sync::{read_sync, write_sync};

mod batch;
mod env;
mod exec;
mod lexer;
mod parser;

#[no_mangle]
pub extern "C" fn main() {
    let mut input_buffer: [u8; 256] = [0; 256];
    let mut prompt: [u8; 256] = [0; 256];

    let mut env = self::env::Environment::new("C:");

    // If a console device path is passed as argv[1], open it for stdin/stdout.
    // This allows the console manager to spawn new terminals without needing
    // to pre-share handles (which would deadlock from within its own task).
    let mut args = idos_sdk::env::args();
    let _ = args.next(); // skip argv[0] (program path)
    if let Some(dev_path) = args.next() {
        use idos_api::io::sync::open_sync;
        use idos_api::syscall::io::create_file_handle;

        let stdin = create_file_handle();
        if open_sync(stdin, dev_path, 0).is_ok() {
            let stdout = create_file_handle();
            if open_sync(stdout, dev_path, 0).is_ok() {
                env.stdin = stdin;
                env.stdout = stdout;
            }
        }
    }

    loop {
        let prompt_len = env.expand_prompt(&mut prompt);

        let _ = write_sync(env.stdout, &prompt[..prompt_len], 0);
        match read_sync(env.stdin, &mut input_buffer, 0) {
            Ok(read_len) => {
                self::exec::exec_line(&mut env, &input_buffer[..(read_len as usize)]);
            }
            Err(_) => (),
        }
    }
}
