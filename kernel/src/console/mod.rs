use core::sync::atomic::Ordering;

use idos_api::io::AsyncOp;

use crate::conman::{register_console_manager, InputBuffer};
use crate::console::graphics::font::Font;
use crate::graphics::{get_vbe_mode_info, set_display_start_point, set_vbe_mode, VbeModeInfo};
use crate::io::async_io::ASYNC_OP_READ;
use crate::io::handle::Handle;
use crate::memory::address::{PhysicalAddress, VirtualAddress};
use crate::task::actions::handle::{
    create_file_handle, create_pipe_handles, create_task, open_message_queue, transfer_handle,
};
use crate::task::actions::io::{
    close_sync, driver_io_complete, open_sync, read_sync, send_io_op, share_sync, write_sync,
};
use crate::task::actions::lifecycle::{create_kernel_task, terminate};
use crate::task::actions::memory::map_memory;
use crate::task::actions::sync::{block_on_wake_set, create_wake_set};
use crate::task::id::TaskID;
use crate::task::memory::MemoryBacking;
use crate::time::system::get_system_ticks;
use idos_api::ipc::Message;

use self::graphics::framebuffer::Framebuffer;
use self::input::{AltAction, KeyAction};
use self::manager::ConsoleManager;
use self::manager::topbar::TOP_BAR_HEIGHT;

struct DragState {
    window_index: u8,
    offset_x: i16,
    offset_y: i16,
}

pub mod buffers;
pub mod console;
pub mod driver;
pub mod input;
pub mod manager;

pub mod graphics;

type ConsoleInputBuffer = InputBuffer<{ crate::conman::INPUT_BUFFER_SIZE }>;

pub fn manager_task() -> ! {
    let response_writer = Handle::new(0);

    let wake_set = create_wake_set();

    let input_buffer_addr = match register_console_manager(wake_set) {
        Ok(addr) => addr,
        Err(_) => {
            crate::kprintln!("Failed to register CONMAN");
            terminate(0);
        }
    };

    let keyboard_buffer_ptr = input_buffer_addr.as_ptr::<ConsoleInputBuffer>();
    let keyboard_buffer = unsafe { &*keyboard_buffer_ptr };
    let mouse_buffer = unsafe { &*(keyboard_buffer_ptr.add(1)) };

    let mut vbe_mode_info: VbeModeInfo = VbeModeInfo::default();
    get_vbe_mode_info(&mut vbe_mode_info, 0x0115);
    set_vbe_mode(0x0115);

    let framebuffer_bytes = (vbe_mode_info.pitch as u32) * (vbe_mode_info.height as u32);
    let framebuffer_pages = (framebuffer_bytes + 0xfff) / 0x1000;

    let graphics_buffer_base = map_memory(
        None,
        0x1000 * framebuffer_pages,
        MemoryBacking::Direct(PhysicalAddress::new(vbe_mode_info.framebuffer)),
    )
    .unwrap();

    let mut fb = Framebuffer {
        width: vbe_mode_info.width,
        height: vbe_mode_info.height,
        stride: vbe_mode_info.pitch,
        buffer: graphics_buffer_base,
    };

    let bytes_per_pixel = (vbe_mode_info.bpp / 8) as usize;
    let bytes_per_pixel = if bytes_per_pixel == 0 {
        1
    } else {
        bytes_per_pixel
    };

    {
        let buffer = fb.get_buffer_mut();
        for row in 0..vbe_mode_info.height as usize {
            let offset = row * vbe_mode_info.pitch as usize;
            for col in 0..vbe_mode_info.width as usize {
                let color: u32 = if (row ^ col) & 2 == 0 {
                    0x000000
                } else {
                    0xFFFFFF
                };
                graphics::write_pixel(
                    buffer,
                    offset + col * bytes_per_pixel,
                    color,
                    bytes_per_pixel,
                );
            }
        }
    }

    let console_font =
        graphics::font::psf::PsfFont::from_file("C:\\TERM14.PSF").expect("Failed to load font");

    let mut mouse_x = vbe_mode_info.width as u32 / 2;
    let mut mouse_y = vbe_mode_info.height as u32 / 2;
    let mut mouse_read: [u8; 3] = [0, 0, 0];
    let mut mouse_read_index = 0;

    let mut conman = ConsoleManager::new();
    let con1 = conman.add_console(); // create the first console (CON1)
    let con2 = conman.add_console(); // create the second console (CON2)

    let _ = write_sync(response_writer, &[0], 0);
    let _ = close_sync(response_writer);

    let messages_handle = open_message_queue();
    let mut incoming_message = Message::empty();

    let mut message_read = AsyncOp::new(
        ASYNC_OP_READ,
        &mut incoming_message as *mut Message as u32,
        core::mem::size_of::<Message>() as u32,
        0,
    );
    let _ = send_io_op(messages_handle, &message_read, Some(wake_set));

    let mut compositor =
        manager::compositor::Compositor::<{ manager::compositor::ColorDepth::Color888 }>::new(fb);

    compositor.add_window(con1);
    compositor.add_window(con2);

    // Set initial window name from the first console's title
    compositor.topbar_state.set_window_name(conman.consoles[0].title.as_bytes());

    // Initialize clock
    {
        let dt = crate::time::system::Timestamp::now().to_datetime();
        dt.time.print_short_to_buffer(&mut compositor.topbar_state.clock_text);
    }
    let mut last_clock_update = crate::time::system::get_monotonic_ms();

    let mut last_action_type: u8 = 0;
    let mut prev_mouse_x = mouse_x;
    let mut prev_mouse_y = mouse_y;
    let mut prev_hover: Option<manager::hit::HitTarget> = None;
    let mut mouse_left_down = false;
    let mut mouse_left_was_down = false;
    let mut drag_state: Option<DragState> = None;
    loop {
        let frame_start = crate::time::system::get_monotonic_ms();

        loop {
            // read input actions and pass them to the current console
            let next_action = match keyboard_buffer.read() {
                Some(action) => action,
                None => break,
            };
            if last_action_type == 0 {
                last_action_type = next_action;
            } else {
                match KeyAction::from_raw(last_action_type, next_action) {
                    Some(action) => {
                        if let Some(alt) = conman.handle_key_action(action) {
                            match alt {
                                AltAction::CloseWindow => {
                                    let idx = compositor.focused_window;
                                    if let Some(console_id) = compositor.remove_window(idx) {
                                        if let Some(console) = conman.consoles.get_mut(console_id) {
                                            console.terminate_all_tasks();
                                        }
                                        conman.current_console = compositor.focused_console();
                                    }
                                }
                                AltAction::NewTerminal => {
                                    let con = conman.add_console();
                                    compositor.add_window(con);
                                    let new_idx = compositor.window_count() - 1;
                                    compositor.focused_window = new_idx;
                                    conman.current_console = compositor.focused_console();
                                    spawn_command_for_console(con);
                                }
                                AltAction::CycleFocus => {
                                    let count = compositor.window_count();
                                    if count > 0 {
                                        compositor.focused_window = (compositor.focused_window + 1) % count;
                                        conman.current_console = compositor.focused_console();
                                        compositor.force_redraw = true;
                                    }
                                }
                                AltAction::ToggleFloat => {
                                    compositor.try_toggle_window_mode(compositor.focused_window);
                                }
                            }
                        }
                    }
                    None => (),
                }
                last_action_type = 0;
            }
        }

        loop {
            let next_mouse_byte = match mouse_buffer.read() {
                Some(byte) => byte,
                None => break,
            };
            mouse_read[mouse_read_index] = next_mouse_byte;
            mouse_read_index += 1;
            if mouse_read_index == 1 {
                if next_mouse_byte & 0x08 == 0 {
                    mouse_read_index = 0; // first byte is not a valid mouse packet
                }
            } else if mouse_read_index == 3 {
                // we have a complete mouse packet
                mouse_left_down = mouse_read[0] & 0x01 != 0;
                let mut dx = mouse_read[1] as u32;
                let mut dy = mouse_read[2] as u32;
                if mouse_read[0] & 0x10 != 0 {
                    dx |= 0xffffff00;
                }
                if mouse_read[0] & 0x20 != 0 {
                    dy |= 0xffffff00;
                }
                let mouse_x_next = mouse_x as i32 + dx as i32;
                let mouse_y_next = mouse_y as i32 - dy as i32;
                if mouse_x_next < 0 {
                    mouse_x = 0;
                } else if mouse_x_next >= vbe_mode_info.width as i32 {
                    mouse_x = vbe_mode_info.width as u32 - 1;
                } else {
                    mouse_x = mouse_x_next as u32;
                }
                if mouse_y_next < 0 {
                    mouse_y = 0;
                } else if mouse_y_next >= vbe_mode_info.height as i32 {
                    mouse_y = vbe_mode_info.height as u32 - 1;
                } else {
                    mouse_y = mouse_y_next as u32;
                }
                mouse_read_index = 0; // reset for the next packet
            }
        }

        // Mouse hover/click handling
        let current_hover = compositor.hit_map.test(mouse_x as u16, mouse_y as u16);
        if current_hover != prev_hover {
            // Force window redraw when hovering over/off window buttons
            let was_win_btn = matches!(prev_hover, Some(manager::hit::HitTarget::WindowButton(..)));
            let is_win_btn = matches!(current_hover, Some(manager::hit::HitTarget::WindowButton(..)));
            if was_win_btn || is_win_btn {
                compositor.force_redraw = true;
            }
            compositor.topbar_state.hover = current_hover;
            prev_hover = current_hover;
        }

        // Detect left click (rising edge: was up, now down)
        let mouse_left_clicked = mouse_left_down && !mouse_left_was_down;
        mouse_left_was_down = mouse_left_down;

        // Drag handling (every frame while left held)
        if mouse_left_down {
            if let Some(ref ds) = drag_state {
                let new_x = (mouse_x as i16 - ds.offset_x).max(0) as u16;
                let new_y = (mouse_y as i16 - ds.offset_y).max(TOP_BAR_HEIGHT as i16) as u16;
                compositor.move_window(ds.window_index as usize, new_x, new_y);
            }
        } else {
            if drag_state.is_some() {
                compositor.end_drag();
            }
            drag_state = None;
        }

        if mouse_left_clicked {
            // Click-to-focus: any click on a window focuses it
            let focus_idx = match current_hover {
                Some(manager::hit::HitTarget::WindowTitleBar(idx))
                | Some(manager::hit::HitTarget::WindowButton(idx, _))
                | Some(manager::hit::HitTarget::WindowContent(idx)) => Some(idx),
                _ => None,
            };
            if let Some(idx) = focus_idx {
                compositor.focused_window = idx as usize;
                conman.current_console = compositor.focused_console();
                compositor.raise_window(idx as usize);
                compositor.force_redraw = true;
            }

            match current_hover {
                Some(manager::hit::HitTarget::DesktopTab(n)) => {
                    compositor.topbar_state.active_desktop = n;
                }
                Some(manager::hit::HitTarget::WindowButton(idx, 0)) => {
                    compositor.try_toggle_window_mode(idx as usize);
                    compositor.topbar_state.hover = None;
                    prev_hover = None;
                }
                Some(manager::hit::HitTarget::WindowButton(idx, 2)) => {
                    if let Some(console_id) = compositor.remove_window(idx as usize) {
                        if let Some(console) = conman.consoles.get_mut(console_id) {
                            console.terminate_all_tasks();
                        }
                        conman.current_console = compositor.focused_console();
                    }
                    compositor.topbar_state.hover = None;
                    prev_hover = None;
                }
                Some(manager::hit::HitTarget::WindowTitleBar(idx)) => {
                    if compositor.is_window_floating(idx as usize) {
                        let win_x = compositor.get_window_x(idx as usize);
                        let win_y = compositor.get_window_y(idx as usize);
                        drag_state = Some(DragState {
                            window_index: idx,
                            offset_x: mouse_x as i16 - win_x as i16,
                            offset_y: mouse_y as i16 - win_y as i16,
                        });
                        compositor.begin_drag(idx as usize, &conman, &console_font);
                    }
                }
                _ => {}
            }
        }

        if message_read.is_complete() {
            let sender = TaskID::new(message_read.return_value.load(Ordering::SeqCst));
            let request_id = incoming_message.unique_id;
            match conman.handle_request(sender, &incoming_message) {
                Some(result) => driver_io_complete(request_id, result),
                None => (),
            }

            message_read = AsyncOp::new(
                ASYNC_OP_READ,
                &mut incoming_message as *mut Message as u32,
                core::mem::size_of::<Message>() as u32,
                0,
            );
            let _ = send_io_op(messages_handle, &message_read, Some(wake_set));
        }

        // Update clock every ~10 seconds
        let now_ms = crate::time::system::get_monotonic_ms();
        if now_ms - last_clock_update >= 10_000 {
            let dt = crate::time::system::Timestamp::now().to_datetime();
            dt.time.print_short_to_buffer(&mut compositor.topbar_state.clock_text);
            last_clock_update = now_ms;
        }

        // Check if any console is in graphics mode
        let any_graphics = conman.consoles.iter().any(|c| c.terminal.graphics_buffer.is_some());

        // Update topbar with the focused console's title
        if let Some(console) = conman.consoles.get(conman.current_console) {
            compositor.topbar_state.set_window_name(console.title.as_bytes());
        }

        if compositor.is_dragging() {
            // Fast drag path: just composite cached window over cached background
            let mouse_moved = mouse_x != prev_mouse_x || mouse_y != prev_mouse_y;
            if mouse_moved {
                compositor.render_drag(mouse_x as u16, mouse_y as u16);
                prev_mouse_x = mouse_x;
                prev_mouse_y = mouse_y;
            }
        } else {
            // Normal render path
            let mouse_moved = mouse_x != prev_mouse_x || mouse_y != prev_mouse_y;
            let any_gfx_dirty = conman.consoles.iter().any(|c| {
                c.terminal.graphics_buffer.as_ref()
                    .map_or(false, |gb| gb.read_dirty_rect().is_some())
            });
            let any_dirty = mouse_moved || any_gfx_dirty || compositor.force_redraw || conman.consoles.iter().any(|c| c.dirty);

            if any_dirty {
                compositor.render(mouse_x as u16, mouse_y as u16, &conman, &console_font);

                // Clear dirty flags after rendering
                for console in conman.consoles.iter_mut() {
                    console.dirty = false;
                }
                prev_mouse_x = mouse_x;
                prev_mouse_y = mouse_y;
            }
        }

        // In graphics mode, poll at ~60fps so we notice dirty rects promptly.
        // In text mode, block indefinitely until an event wakes us.
        let timeout = if any_graphics {
            let elapsed = crate::time::system::get_monotonic_ms() - frame_start;
            let remaining = 16u64.saturating_sub(elapsed);
            Some(remaining as u32)
        } else {
            None
        };
        block_on_wake_set(wake_set, timeout);
    }
}

pub fn init_console() {
    let (response_reader, response_writer) = create_pipe_handles();
    let driver_task = create_kernel_task(manager_task, Some("CONMAN"));
    transfer_handle(response_writer, driver_task);

    let _ = read_sync(response_reader, &mut [0u8], 0);
    let _ = close_sync(response_reader);
}

pub fn console_ready() {
    let _cmd1 = start_command(0);
    let _cmd2 = start_command(1);
}

/// Spawn a COMMAND.ELF for a console without doing any sync I/O on
/// console devices. Instead, passes the device path as an argument so the
/// program opens it itself. Safe to call from the console manager task.
fn spawn_command_for_console(console_index: usize) {
    let dev_path = alloc::format!("DEV:\\CON{}", console_index + 1);

    let (_task_handle, task_id) = create_task();

    crate::task::actions::lifecycle::add_args(task_id, ["C:\\COMMAND.ELF", dev_path.as_str()]);

    let _ = crate::exec::exec_program(task_id, "C:\\COMMAND.ELF");
}

fn start_command(console_index: usize) -> Handle {
    let path = alloc::format!("DEV:\\CON{}", console_index + 1);

    let stdin = create_file_handle();
    open_sync(stdin, path.as_str(), 0).unwrap();
    let stdout = create_file_handle();
    open_sync(stdout, path.as_str(), 0).unwrap();

    let _ = write_sync(stdout, b"Loading COMMAND.ELF from C:\n\n", 0);

    let (task_handle, task_id) = create_task();

    let _ = share_sync(stdin, task_id);
    let _ = share_sync(stdout, task_id);

    let _ = crate::exec::exec_program(task_id, "C:\\COMMAND.ELF");

    task_handle
}
