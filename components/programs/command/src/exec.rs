use crate::{
    env::Environment,
    parser::{CommandComponent, CommandTree, RedirectOutput},
};

use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use idos_api::io::{
    handle::dup_handle,
    sync::{close_sync, open_sync, read_sync, share_sync, write_sync},
};
use idos_api::syscall::io::create_file_handle;
use idos_api::syscall::memory::map_memory;
use idos_api::time::DateTime;
use idos_api::{io::file::FileStatus, syscall::exec::create_task};
use idos_api::{
    io::{
        error::IoError, sync::io_sync, FILE_OP_MKDIR, FILE_OP_RENAME, FILE_OP_RMDIR, FILE_OP_STAT,
        FILE_OP_UNLINK, OPEN_FLAG_CREATE,
    },
    syscall::exec::load_executable,
};

static IO_BUFFER: AtomicPtr<u8> = AtomicPtr::new(0xffff_ffff as *mut u8);

pub fn get_io_buffer() -> &'static mut [u8] {
    let buffer_start = {
        let stored = IO_BUFFER.load(Ordering::SeqCst);
        if stored as u32 == 0xffff_ffff {
            let addr = map_memory(None, 0x1000, None).unwrap();
            unsafe {
                // force a page fault to assign memory
                core::ptr::write_volatile(addr as *mut u8, 0);
            }
            IO_BUFFER.store(addr as *mut u8, Ordering::SeqCst);
            addr as *mut u8
        } else {
            stored
        }
    };
    unsafe { core::slice::from_raw_parts_mut(buffer_start, 0x1000) }
}

/// Parse and execute a single line of input (used by batch file executor).
pub fn exec_line(env: &mut Environment, line: &[u8]) {
    let lexer = crate::lexer::Lexer::new(line);
    let mut parser = crate::parser::Parser::new(lexer);
    parser.parse_input();
    let tree = parser.into_tree();
    exec_command_tree(env, tree);
}

pub fn exec_command_tree(env: &mut Environment, tree: CommandTree) {
    let root = match tree.get_root() {
        Some(component) => component,
        None => return,
    };

    match root {
        CommandComponent::Executable(name, args, redirect) => {
            // Check if this is a builtin that supports redirect
            let is_builtin = matches!(
                name.to_ascii_uppercase().as_str(),
                "CD" | "CHDIR" | "CLS" | "COLOR" | "COPY" | "DEL" | "DIR" | "DRIVES"
                    | "APPEND" | "ECHO" | "ERASE" | "HELP" | "MD" | "MKDIR" | "MOVE"
                    | "PROMPT" | "RD" | "RMDIR" | "REN" | "RENAME" | "TYPE" | "VER"
            );

            // Set up redirect if present
            let saved = setup_redirect(env, redirect, is_builtin, name);

            match name.to_ascii_uppercase().as_str() {
                "CD" | "CHDIR" => cd(env, args),
                "CLS" => cls(env),
                "COLOR" => color(env, args),
                "COPY" => copy(env, args),
                "DEL" | "ERASE" => del(env, args),
                "DIR" => dir(env, args),
                "DRIVES" => drives(env),
                "APPEND" => append(env, args),
                "ECHO" => echo(env, args),
                "HELP" => help(env),
                "MD" | "MKDIR" => mkdir_cmd(env, args),
                "MOVE" => move_cmd(env, args),
                "PROMPT" => prompt(env, args),
                "RD" | "RMDIR" => rmdir_cmd(env, args),
                "REN" | "RENAME" => ren(env, args),
                "TYPE" => type_file(env, args),
                "VER" => ver(env),
                _ => {
                    if is_drive(name.as_bytes()) {
                        let mut cd_args = Vec::new();
                        cd_args.push(String::from(name));
                        cd(env, &cd_args);
                    } else if !try_external(env, name, args) {
                        env.write(b"Unknown command!\n");
                    }
                }
            }

            // Restore redirect
            teardown_redirect(env, saved);
        },
        _ => {
            env.write(b"Unsupported syntax!\n");
        }
    }
}

struct SavedRedirect {
    stdout: idos_api::io::handle::Handle,
    write_offset: u32,
    file_handle: Option<idos_api::io::handle::Handle>,
}

fn setup_redirect(env: &mut Environment, redirect: &RedirectOutput, is_builtin: bool, name: &str) -> Option<SavedRedirect> {
    match redirect {
        RedirectOutput::None => None,
        _ if !is_builtin && !is_drive(name.as_bytes()) => {
            env.write(b"Redirect not supported for external commands\n");
            None
        }
        RedirectOutput::Overwrite(filename) | RedirectOutput::Append(filename) => {
            let file_path = env.full_file_path(&String::from(filename.as_str()));
            let handle = create_file_handle();
            match open_sync(handle, file_path.as_str(), OPEN_FLAG_CREATE) {
                Ok(_) => {}
                Err(_) => {
                    env.write(b"Failed to open file for redirect\n");
                    return None;
                }
            }

            let saved = SavedRedirect {
                stdout: env.stdout,
                write_offset: env.write_offset,
                file_handle: Some(handle),
            };

            env.stdout = handle;

            match redirect {
                RedirectOutput::Append(_) => {
                    // STAT to get file size
                    let mut file_status = FileStatus::new();
                    let file_status_ptr = &mut file_status as *mut FileStatus;
                    let _ = io_sync(
                        handle,
                        FILE_OP_STAT,
                        file_status_ptr as u32,
                        core::mem::size_of::<FileStatus>() as u32,
                        0,
                    );
                    env.write_offset = file_status.byte_size;
                }
                _ => {
                    env.write_offset = 0;
                }
            }

            Some(saved)
        }
    }
}

fn teardown_redirect(env: &mut Environment, saved: Option<SavedRedirect>) {
    if let Some(saved) = saved {
        if let Some(file_handle) = saved.file_handle {
            let _ = close_sync(file_handle);
        }
        env.stdout = saved.stdout;
        env.write_offset = saved.write_offset;
    }
}

fn echo(env: &mut Environment, args: &Vec<String>) {
    // ECHO with no args could toggle echo state, but for now just print a blank line
    if args.is_empty() {
        env.write(b"\n");
        return;
    }
    // Rejoin args with spaces
    let mut buf = [0u8; 256];
    let mut len = 0;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 && len < buf.len() {
            buf[len] = b' ';
            len += 1;
        }
        let bytes = arg.as_bytes();
        let n = bytes.len().min(buf.len() - len);
        buf[len..len + n].copy_from_slice(&bytes[..n]);
        len += n;
    }
    if len < buf.len() {
        buf[len] = b'\n';
        len += 1;
    }
    env.write(&buf[..len]);
}

fn append(env: &mut Environment, args: &Vec<String>) {
    if args.len() < 2 {
        env.write(b"Usage: APPEND <filename> <text>\n");
        return;
    }

    let file_path = env.full_file_path(&args[0]);

    // Rejoin remaining args as the text to append
    let mut text_buf = [0u8; 256];
    let mut text_len = 0;
    for (i, arg) in args[1..].iter().enumerate() {
        if i > 0 && text_len < text_buf.len() {
            text_buf[text_len] = b' ';
            text_len += 1;
        }
        let bytes = arg.as_bytes();
        let n = bytes.len().min(text_buf.len() - text_len);
        text_buf[text_len..text_len + n].copy_from_slice(&bytes[..n]);
        text_len += n;
    }

    let handle = create_file_handle();
    match open_sync(handle, file_path.as_str(), OPEN_FLAG_CREATE) {
        Ok(_) => {}
        Err(_) => {
            env.write(b"Failed to open file\n");
            return;
        }
    }

    // Get file size via STAT so we know where to append
    let mut file_status = FileStatus::new();
    let file_status_ptr = &mut file_status as *mut FileStatus;
    let _ = io_sync(
        handle,
        FILE_OP_STAT,
        file_status_ptr as u32,
        core::mem::size_of::<FileStatus>() as u32,
        0,
    );

    match write_sync(handle, &text_buf[..text_len], file_status.byte_size) {
        Ok(n) => {
            let s = alloc::format!("Appended {} bytes\n", n);
            env.write(s.as_bytes());
        }
        Err(_) => {
            env.write(b"Write failed\n");
        }
    }

    let _ = close_sync(handle);
}

fn drives(env: &mut Environment) {
    let handle = create_file_handle();
    match open_sync(handle, "SYS:\\DRIVES", 0) {
        Ok(_) => {}
        Err(_) => {
            env.write(b"Failed to read drive list\n");
            return;
        }
    }
    let buffer = get_io_buffer();
    let mut read_offset = 0;
    loop {
        let len = match read_sync(handle, buffer, read_offset) {
            Ok(len) => len as usize,
            Err(_) => {
                env.write(b"Error reading drive list\n");
                break;
            }
        };
        read_offset += len as u32;
        env.write(&buffer[..len]);
        if len < buffer.len() {
            break;
        }
    }
    let _ = close_sync(handle);
}

fn ver(env: &mut Environment) {
    env.write(b"\n");
    let handle = create_file_handle();
    match open_sync(handle, "SYS:\\KERNINFO", 0) {
        Ok(_) => {
            let buffer = get_io_buffer();
            let mut read_offset = 0;
            loop {
                let len = match read_sync(handle, buffer, read_offset) {
                    Ok(len) => len as usize,
                    Err(_) => break,
                };
                read_offset += len as u32;
                env.write(&buffer[..len]);
                if len < buffer.len() {
                    break;
                }
            }
            let _ = close_sync(handle);
        }
        Err(_) => {
            env.write(b"IDOS-NX Version Unknown\n");
        }
    }
    env.write(b"\n");
}

fn cls(env: &mut Environment) {
    // ESC[2J clears the screen, ESC[H moves cursor to top-left
    env.write(b"\x1b[2J\x1b[H");
}

fn color(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        // Reset to defaults (light gray on black)
        env.write(b"\x1b[0m\x1b[2J\x1b[H");
        return;
    }

    let arg = args[0].as_bytes();
    if arg.len() != 2 {
        env.write(b"Usage: COLOR [bg_fg]\n  Two hex digits (0-F): background, foreground\n  Example: COLOR 0A (green on black)\n");
        return;
    }

    let bg = match hex_digit(arg[0]) {
        Some(v) => v,
        None => {
            env.write(b"Invalid hex digit\n");
            return;
        }
    };
    let fg = match hex_digit(arg[1]) {
        Some(v) => v,
        None => {
            env.write(b"Invalid hex digit\n");
            return;
        }
    };

    if fg == bg {
        env.write(b"Foreground and background cannot be the same\n");
        return;
    }

    // Map CGA color index to ANSI SGR codes
    // CGA indices 0-7 map to normal, 8-15 map to bright
    let mut buf = [0u8; 32];
    let mut len = 0;

    // Reset first
    buf[len..len + 4].copy_from_slice(b"\x1b[0m");
    len += 4;

    // Foreground
    len += write_sgr_fg(&mut buf[len..], fg);

    // Background
    len += write_sgr_bg(&mut buf[len..], bg);

    // Clear screen with new colors
    buf[len..len + 7].copy_from_slice(b"\x1b[2J\x1b[H");
    len += 7;

    env.write(&buf[..len]);
}

/// CGA index to ANSI color code offset. CGA and ANSI have different orderings
/// for blue/red, cyan/yellow, etc.
const CGA_TO_ANSI: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

fn write_sgr_fg(buf: &mut [u8], cga: u8) -> usize {
    let base = CGA_TO_ANSI[(cga & 7) as usize];
    if cga >= 8 {
        // Bright: ESC[9Xm
        buf[0..2].copy_from_slice(b"\x1b[");
        buf[2] = b'9';
        buf[3] = b'0' + base;
        buf[4] = b'm';
        5
    } else {
        // Normal: ESC[3Xm
        buf[0..2].copy_from_slice(b"\x1b[");
        buf[2] = b'3';
        buf[3] = b'0' + base;
        buf[4] = b'm';
        5
    }
}

fn write_sgr_bg(buf: &mut [u8], cga: u8) -> usize {
    let base = CGA_TO_ANSI[(cga & 7) as usize];
    if cga >= 8 {
        // Bright: ESC[10Xm
        buf[0..3].copy_from_slice(b"\x1b[1");
        buf[3] = b'0';
        buf[4] = b'0' + base;
        buf[5] = b'm';
        6
    } else {
        // Normal: ESC[4Xm
        buf[0..2].copy_from_slice(b"\x1b[");
        buf[2] = b'4';
        buf[3] = b'0' + base;
        buf[4] = b'm';
        5
    }
}

fn prompt(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        // No argument resets to default
        env.set_prompt(b"$P$G");
        return;
    }
    // Rejoin args with spaces to reconstruct the format string
    let mut buf = [0u8; 128];
    let mut len = 0;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 && len < buf.len() {
            buf[len] = b' ';
            len += 1;
        }
        let bytes = arg.as_bytes();
        let n = bytes.len().min(buf.len() - len);
        buf[len..len + n].copy_from_slice(&bytes[..n]);
        len += n;
    }
    env.set_prompt(&buf[..len]);
}

fn hex_digit(ch: u8) -> Option<u8> {
    match ch {
        b'0'..=b'9' => Some(ch - b'0'),
        b'A'..=b'F' => Some(ch - b'A' + 10),
        b'a'..=b'f' => Some(ch - b'a' + 10),
        _ => None,
    }
}

fn is_drive(name: &[u8]) -> bool {
    for i in 0..(name.len() - 1) {
        if name[i] < b'A' {
            return false;
        }
        if name[i] > b'Z' && name[i] < b'a' {
            return false;
        }
        if name[i] > b'z' {
            return false;
        }
    }
    if name[name.len() - 1] != b':' {
        return false;
    }
    true
}

fn cd(env: &mut Environment, args: &Vec<String>) {
    let change_to = args.get(0).cloned();
    match change_to {
        Some(ref arg) => {
            if arg.starts_with("\\") {
                // absolute path
            } else if is_drive(arg.as_bytes()) {
                // drive switch
                env.reset_drive(arg.as_bytes());
            } else {
                // relative path
                let mut split_iter = arg.split("\\");
                loop {
                    match split_iter.next() {
                        Some(chunk) => match chunk {
                            "." => (),
                            ".." => env.popd(),
                            dir => env.pushd(dir.as_bytes()),
                        },
                        None => break,
                    }
                }
            }
        }
        None => {
            // no argument, change to root
            env.pop_to_root();
        }
    }
}

struct DirEntry {
    name: String,
    size: u32,
    mod_timestamp: u32,
    is_dir: bool,
}

fn dir(env: &mut Environment, args: &Vec<String>) {
    let file_read_buffer = get_io_buffer();

    let mut output = String::from(
        " Volume in drive is UNKNOWN\n Volume Serial Number is UNKNOWN\n Directory of ",
    );
    output.push_str(env.cwd_string());
    output.push_str("\n\n");
    env.write(output.as_bytes());

    let dir_handle = create_file_handle();
    match open_sync(dir_handle, env.cwd_string(), 0) {
        Ok(_) => (),
        Err(_) => {
            env.write(b"Failed to open directory...\n");
            return;
        }
    }
    let mut entries: Vec<DirEntry> = Vec::new();
    let mut read_offset = 0;
    loop {
        let bytes_read = read_sync(dir_handle, file_read_buffer, read_offset).unwrap() as usize;
        read_offset += bytes_read as u32;
        let mut name_start = 0;
        for i in 0..bytes_read {
            if file_read_buffer[i] == 0 {
                let name = String::from_utf8_lossy(&file_read_buffer[name_start..i]);
                entries.push(DirEntry {
                    name: name.to_string(),
                    size: 0,
                    mod_timestamp: 0,
                    is_dir: false,
                });
                name_start = i + 1;
            }
        }
        if bytes_read < file_read_buffer.len() {
            break;
        }
    }
    let _ = close_sync(dir_handle);

    for entry in entries.iter_mut() {
        let stat_handle = create_file_handle();
        let mut file_status = FileStatus::new();
        let file_status_ptr = &mut file_status as *mut FileStatus;
        let mut file_path = String::from(env.cwd_string());
        file_path.push_str(entry.name.as_str());
        match open_sync(stat_handle, file_path.as_str(), 0) {
            Ok(_) => {
                let op = io_sync(
                    stat_handle,
                    FILE_OP_STAT,
                    file_status_ptr as u32,
                    core::mem::size_of::<FileStatus>() as u32,
                    0,
                );
                entry.size = file_status.byte_size;
                entry.mod_timestamp = file_status.modification_time;
                entry.is_dir = file_status.file_type & 2 != 0;
                let _ = close_sync(stat_handle);
            }
            Err(_) => {}
        }
    }

    for entry in entries.iter() {
        let mut row = String::from("");
        row.push_str(&entry.name);
        for _ in entry.name.len()..13 {
            row.push(' ');
        }
        if entry.is_dir {
            row.push_str("<DIR>     ");
        } else {
            row.push_str(&alloc::format!("{:>9} ", entry.size));
        }
        let datetime = DateTime::from_timestamp(entry.mod_timestamp);
        let day = datetime.date.day;
        let month = datetime.date.month;
        let year = datetime.date.year;
        row.push_str(&alloc::format!("{:02}-{:02}-{:04}", day, month, year));
        row.push(' ');
        let hours = datetime.time.hours;
        let minutes = datetime.time.minutes;
        let seconds = datetime.time.seconds;
        row.push_str(&alloc::format!(
            "{:02}:{:02}:{:02}",
            hours,
            minutes,
            seconds,
        ));
        row.push('\n');

        env.write(row.as_bytes());
    }

    let mut summary = String::new();
    for _ in 0..13 {
        summary.push(' ');
    }
    summary.push_str(&alloc::format!("{} file(s)\n", entries.len()));
    env.write(summary.as_bytes());
}

fn help(env: &mut Environment) {
    env.write(b"\
APPEND <file> <text>  Append text to a file
CD/CHDIR [dir]        Change directory
CLS                   Clear the screen
COLOR [bg_fg]         Set console colors
COPY <src> <dest>     Copy a file
DEL/ERASE <file>      Delete a file
DIR [dir]             List directory contents
DRIVES                List available drives
ECHO [text]           Display a message
HELP                  Show this help
MKDIR/MD <dir>        Create a directory
MOVE <src> <dest>     Move a file (works across drives)
PROMPT [format]       Change the command prompt
REN/RENAME <old> <new>  Rename a file or directory
RMDIR/RD <dir>        Remove an empty directory
TYPE <file>           Display file contents
VER                   Display version info
");
}

fn del(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        env.write(b"Usage: DEL <filename>\n");
        return;
    }
    let file_path = env.full_file_path(&args[0]);
    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_UNLINK,
        file_path.as_ptr() as u32,
        file_path.len() as u32,
        0,
    );
    let _ = close_sync(handle);
    match result {
        Ok(_) => {}
        Err(IoError::NotFound) => env.write(b"File not found\n"),
        Err(_) => env.write(b"Failed to delete file\n"),
    }
}

fn mkdir_cmd(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        env.write(b"Usage: MKDIR <dirname>\n");
        return;
    }
    let dir_path = env.full_file_path(&args[0]);
    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_MKDIR,
        dir_path.as_ptr() as u32,
        dir_path.len() as u32,
        0,
    );
    let _ = close_sync(handle);
    match result {
        Ok(_) => {}
        Err(_) => env.write(b"Failed to create directory\n"),
    }
}

fn rmdir_cmd(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        env.write(b"Usage: RMDIR <dirname>\n");
        return;
    }
    let dir_path = env.full_file_path(&args[0]);
    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_RMDIR,
        dir_path.as_ptr() as u32,
        dir_path.len() as u32,
        0,
    );
    let _ = close_sync(handle);
    match result {
        Ok(_) => {}
        Err(IoError::NotFound) => env.write(b"Directory not found\n"),
        Err(IoError::ResourceInUse) => env.write(b"Directory is not empty\n"),
        Err(_) => env.write(b"Failed to remove directory\n"),
    }
}

fn do_rename(env: &mut Environment, src_path: &str, dest_path: &str) -> Result<(), IoError> {
    let total_len = src_path.len() + dest_path.len();
    let mut combined = [0u8; 512];
    combined[..src_path.len()].copy_from_slice(src_path.as_bytes());
    combined[src_path.len()..total_len].copy_from_slice(dest_path.as_bytes());
    let packed_lens = (src_path.len() as u32) | ((dest_path.len() as u32) << 16);

    let handle = create_file_handle();
    let result = io_sync(
        handle,
        FILE_OP_RENAME,
        combined.as_ptr() as u32,
        total_len as u32,
        packed_lens,
    );
    let _ = close_sync(handle);
    result.map(|_| ())
}

fn ren(env: &mut Environment, args: &Vec<String>) {
    if args.len() < 2 {
        env.write(b"Usage: REN <old> <new>\n");
        return;
    }
    let src_path = env.full_file_path(&args[0]);
    let dest_path = env.full_file_path(&args[1]);
    match do_rename(env, &src_path, &dest_path) {
        Ok(_) => {}
        Err(IoError::CrossDeviceLink) => {
            env.write(b"Cannot rename across drives. Use MOVE instead.\n");
        }
        Err(IoError::NotFound) => env.write(b"File not found\n"),
        Err(_) => env.write(b"Failed to rename\n"),
    }
}

fn copy_file(env: &mut Environment, src_path: &str, dest_path: &str) -> Result<u32, ()> {
    let src_handle = create_file_handle();
    match open_sync(src_handle, src_path, 0) {
        Ok(_) => {}
        Err(_) => {
            env.write(b"Failed to open source file\n");
            return Err(());
        }
    }

    let dest_handle = create_file_handle();
    match open_sync(dest_handle, dest_path, OPEN_FLAG_CREATE) {
        Ok(_) => {}
        Err(_) => {
            let _ = close_sync(src_handle);
            env.write(b"Failed to create destination file\n");
            return Err(());
        }
    }

    let buffer = get_io_buffer();
    let mut offset: u32 = 0;
    loop {
        let bytes_read = match read_sync(src_handle, buffer, offset) {
            Ok(n) => n,
            Err(_) => {
                env.write(b"Error reading source file\n");
                let _ = close_sync(src_handle);
                let _ = close_sync(dest_handle);
                return Err(());
            }
        };
        if bytes_read == 0 {
            break;
        }
        match write_sync(dest_handle, &buffer[..bytes_read as usize], offset) {
            Ok(_) => {}
            Err(_) => {
                env.write(b"Error writing destination file\n");
                let _ = close_sync(src_handle);
                let _ = close_sync(dest_handle);
                return Err(());
            }
        }
        offset += bytes_read;
        if (bytes_read as usize) < buffer.len() {
            break;
        }
    }

    let _ = close_sync(src_handle);
    let _ = close_sync(dest_handle);
    Ok(offset)
}

fn copy(env: &mut Environment, args: &Vec<String>) {
    if args.len() < 2 {
        env.write(b"Usage: COPY <source> <dest>\n");
        return;
    }
    let src_path = env.full_file_path(&args[0]);
    let dest_path = env.full_file_path(&args[1]);
    match copy_file(env, &src_path, &dest_path) {
        Ok(bytes) => {
            let msg = alloc::format!("        {} file(s) copied ({} bytes)\n", 1, bytes);
            env.write(msg.as_bytes());
        }
        Err(_) => {}
    }
}

fn move_cmd(env: &mut Environment, args: &Vec<String>) {
    if args.len() < 2 {
        env.write(b"Usage: MOVE <source> <dest>\n");
        return;
    }
    let src_path = env.full_file_path(&args[0]);
    let dest_path = env.full_file_path(&args[1]);

    // Try rename first (fast path, same filesystem)
    match do_rename(env, &src_path, &dest_path) {
        Ok(_) => {
            env.write(b"        1 file(s) moved.\n");
            return;
        }
        Err(IoError::CrossDeviceLink) => {
            // Fall through to copy + delete
        }
        Err(IoError::NotFound) => {
            env.write(b"File not found\n");
            return;
        }
        Err(_) => {
            env.write(b"Failed to move file\n");
            return;
        }
    }

    // Cross-filesystem move: copy then delete source
    match copy_file(env, &src_path, &dest_path) {
        Ok(_) => {
            // Delete source
            let handle = create_file_handle();
            let result = io_sync(
                handle,
                FILE_OP_UNLINK,
                src_path.as_ptr() as u32,
                src_path.len() as u32,
                0,
            );
            let _ = close_sync(handle);
            match result {
                Ok(_) => {
                    env.write(b"        1 file(s) moved.\n");
                }
                Err(_) => {
                    env.write(b"File copied but failed to delete source\n");
                }
            }
        }
        Err(_) => {}
    }
}

fn type_file(env: &mut Environment, args: &Vec<String>) {
    if args.is_empty() {
        env.write(b"No file specified!\n");
        return;
    }
    for arg in args {
        type_file_inner(env, arg);
    }
}

fn type_file_inner(env: &mut Environment, arg: &String) -> Result<(), ()> {
    let handle = create_file_handle();
    let file_path = env.full_file_path(arg);
    let _ = open_sync(handle, file_path.as_str(), 0).map_err(|_| ());
    let mut read_offset = 0;

    let buffer = get_io_buffer();
    loop {
        let len = match read_sync(handle, buffer, read_offset) {
            Ok(len) => len as usize,
            Err(_) => {
                env.write(b"Error reading file\n");
                return Err(());
            }
        };
        read_offset += len as u32;
        env.write(&buffer[..len]);

        if len < buffer.len() {
            break;
        }
    }

    let _ = close_sync(handle).map_err(|_| ())?;
    Ok(())
}

/// Try to resolve a command name to an external file. Checks in order:
/// exact name, name.ELF, name.BAT. Dispatches to the appropriate executor.
fn try_external(env: &mut Environment, name: &String, args: &Vec<String>) -> bool {
    // If the name already has an extension, try it directly
    if has_extension(name.as_bytes()) {
        let path = env.full_file_path(name);
        if ends_with_ignore_case(path.as_bytes(), b".BAT") {
            if file_exists(&path) {
                set_console_title(env, path.as_bytes());
                crate::batch::exec_batch(env, path.as_str(), args);
                set_console_title(env, b"C:\\COMMAND.ELF");
                return true;
            }
        } else {
            return try_exec(env, &path, args);
        }
        return false;
    }

    // Try name.ELF
    let mut elf_name = name.clone();
    elf_name.push_str(".ELF");
    let elf_path = env.full_file_path(&elf_name);
    if try_exec(env, &elf_path, args) {
        return true;
    }

    // Try name.BAT
    let mut bat_name = name.clone();
    bat_name.push_str(".BAT");
    let bat_path = env.full_file_path(&bat_name);
    if file_exists(&bat_path) {
        set_console_title(env, bat_path.as_bytes());
        crate::batch::exec_batch(env, bat_path.as_str(), args);
        set_console_title(env, b"C:\\COMMAND.ELF");
        return true;
    }

    // Try exact name as-is (maybe it has no extension but is an ELF)
    let exact_path = env.full_file_path(name);
    try_exec(env, &exact_path, args)
}

fn has_extension(name: &[u8]) -> bool {
    name.iter().any(|&c| c == b'.')
}

fn ends_with_ignore_case(s: &[u8], suffix: &[u8]) -> bool {
    if s.len() < suffix.len() {
        return false;
    }
    let start = s.len() - suffix.len();
    for i in 0..suffix.len() {
        let a = if s[start + i].is_ascii_alphabetic() { s[start + i] | 0x20 } else { s[start + i] };
        let b = if suffix[i].is_ascii_alphabetic() { suffix[i] | 0x20 } else { suffix[i] };
        if a != b {
            return false;
        }
    }
    true
}

fn file_exists(path: &str) -> bool {
    let handle = create_file_handle();
    match open_sync(handle, path, 0) {
        Ok(_) => {
            let _ = close_sync(handle);
            true
        }
        Err(_) => false,
    }
}

fn set_console_title(env: &Environment, title: &[u8]) {
    use idos_api::io::sync::ioctl_sync;
    use idos_api::io::termios::TSETTITLE;
    let _ = ioctl_sync(env.stdout, TSETTITLE, title.as_ptr() as u32, title.len() as u32);
}

fn try_exec(env: &Environment, exec_path: &str, args: &Vec<String>) -> bool {
    let exec_handle = create_file_handle();
    match open_sync(exec_handle, exec_path, 0) {
        Ok(_) => {
            let _ = close_sync(exec_handle);
        }
        Err(_) => return false,
    }
    let (child_handle, child_id) = create_task();

    // Build arg structure: argv[0] = program path, then any additional args
    // Format: [u16 len][bytes][u16 len][bytes]...
    let arg_structure_size: usize =
        exec_path.len() + 2 + args.iter().map(|s| s.len() + 2).sum::<usize>();
    let mut arg_structure_buffer = Vec::with_capacity(arg_structure_size);
    // argv[0] = program path
    let len_low = (exec_path.len() & 0xFF) as u8;
    let len_high = ((exec_path.len() >> 8) & 0xFF) as u8;
    arg_structure_buffer.push(len_low);
    arg_structure_buffer.push(len_high);
    arg_structure_buffer.extend_from_slice(exec_path.as_bytes());
    // argv[1..] = additional args
    for arg in args {
        let len_low = (arg.len() & 0xFF) as u8;
        let len_high = ((arg.len() >> 8) & 0xFF) as u8;
        arg_structure_buffer.push(len_low);
        arg_structure_buffer.push(len_high);
        arg_structure_buffer.extend_from_slice(arg.as_bytes());
    }
    idos_api::syscall::exec::add_args(
        child_id,
        arg_structure_buffer.as_ptr(),
        arg_structure_size as u32,
    );

    let stdin_dup = dup_handle(env.stdin).unwrap();
    let stdout_dup = dup_handle(env.stdout).unwrap();

    // Share handles BEFORE load_executable, because load_executable makes
    // the child runnable immediately. If we share after, the child may start
    // running before its stdin/stdout handles exist (race condition).
    share_sync(stdin_dup, child_id).unwrap();
    share_sync(stdout_dup, child_id).unwrap();

    if !load_executable(child_id, exec_path) {
        // exec failed — clean up the handles we created
        // TODO: the shares already completed, would need to revoke them
        let _ = close_sync(child_handle);
        return false;
    }

    set_console_title(env, exec_path.as_bytes());
    let _ = read_sync(child_handle, &mut [0u8], 0);
    let _ = close_sync(child_handle);
    set_console_title(env, b"C:\\COMMAND.ELF");
    true
}
