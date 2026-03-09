#![no_std]
#![no_main]

extern crate idos_api;
extern crate idos_sdk;

use idos_api::{
    compat::VMRegisters,
    io::{
        read_message_op,
        sync::{read_sync, write_sync},
        Handle, Message,
    },
    syscall::{
        exec::enter_8086,
        io::{
            append_io_op, block_on_wake_set, create_message_queue_handle, create_wake_set,
            futex_wake,
        },
        memory::map_memory,
    },
};

#[no_mangle]
pub extern "C" fn main() {
    let message_queue = create_message_queue_handle();
    let mut incoming_message = Message::empty();

    // identity-map the bottom page
    idos_api::syscall::memory::map_memory(
        Some(0x0000_0000),
        0x1000, // 4KB
        Some(0x0000_0000),
    )
    .unwrap();

    // identity-map BIOS code (0xA0000 - 0xFFFFF)
    let bios_area_size = 0x0010_0000 - 0x000a_0000;
    map_memory(Some(0x000a_0000), bios_area_size, Some(0x000a_0000)).unwrap();

    // map a page to store the stack in 8086 mode
    let stack_frame = map_memory(Some(0x0000_8000), 0x1000, None).unwrap();
    let stack_top = stack_frame + 0x1000;

    let mut vm_regs = VMRegisters {
        eax: 0x00,
        ebx: 0x00,
        ecx: 0x00,
        edx: 0x00,
        esi: 0x00,
        edi: 0x00,
        ebp: 0x00,
        eip: 0x00,
        esp: stack_top,
        eflags: 0x2,
        cs: 0,
        ss: 0,
        es: 0,
        ds: 0,
        fs: 0,
        gs: 0,
    };

    let wake_set = create_wake_set();
    let mut message_read = read_message_op(&mut incoming_message);
    append_io_op(message_queue, &message_read, Some(wake_set));

    loop {
        if message_read.is_complete() {
            match incoming_message.message_type {
                0x01 => {
                    // set VGA mode
                    let mode = incoming_message.args[0] as u8;
                    if let Some(signal_ptr) = map_signal(incoming_message.args[1]) {
                        set_vga_mode(mode, &mut vm_regs, stack_top);
                        unsafe {
                            *signal_ptr = 1;
                        }
                        futex_wake(signal_ptr as u32, 1);
                        unmap_signal(signal_ptr);
                    }
                }
                0x10 => {
                    // get VBE modes
                }
                0x11 => {
                    // get VBE mode info
                    let mode = incoming_message.args[0] as u16;
                    if let Some(signal_ptr) = map_signal(incoming_message.args[1]) {
                        get_vbe_mode_info(mode, incoming_message.args[2], &mut vm_regs, stack_top);

                        unsafe {
                            *signal_ptr = 1;
                        }
                        futex_wake(signal_ptr as u32, 1);
                        unmap_signal(signal_ptr);
                    }
                }
                0x12 => {
                    // set VBE mode
                    let mode = incoming_message.args[0] as u16;
                    if let Some(signal_ptr) = map_signal(incoming_message.args[1]) {
                        set_vbe_mode(mode, &mut vm_regs, stack_top);
                        unsafe {
                            *signal_ptr = 1;
                        }
                        futex_wake(signal_ptr as u32, 1);
                        unmap_signal(signal_ptr);
                    }
                }
                0x13 => {
                    // get current VBE mode
                }

                0x17 => {
                    // set display start point
                    let x = incoming_message.args[0] as u16;
                    let y = incoming_message.args[1] as u16;

                    if let Some(signal_ptr) = map_signal(incoming_message.args[2]) {
                        vm_regs.ecx = x as u32;
                        vm_regs.edx = y as u32;
                        video_bios_interrupt(0x4F07, &mut vm_regs, stack_top);

                        unsafe {
                            *signal_ptr = 1;
                        }
                        futex_wake(signal_ptr as u32, 1);
                        unmap_signal(signal_ptr);
                    }
                }
                _ => (),
            }

            message_read = read_message_op(&mut incoming_message);
            append_io_op(message_queue, &message_read, Some(wake_set));
        }

        block_on_wake_set(wake_set, None);
    }
}

fn map_signal(signal_paddr: u32) -> Option<*mut u32> {
    let signal_offset = signal_paddr & 0xfff;
    if let Ok(signal_page) = map_memory(None, 0x1000, Some(signal_paddr & 0xfffff000)) {
        Some((signal_page + signal_offset) as *mut u32)
    } else {
        None
    }
}

fn unmap_signal(signal_ptr: *mut u32) {
    let signal_addr = signal_ptr as u32;
    let signal_page = signal_addr & 0xfffff000;
    // unmap_memory(signal_page)
}

fn set_vga_mode(mode: u8, regs: &mut VMRegisters, stack_top: u32) {
    video_bios_interrupt(mode as u16, regs, stack_top)
}

fn set_vbe_mode(mode: u16, regs: &mut VMRegisters, stack_top: u32) {
    regs.ebx = (mode as u32) | 0x4000; // bit 14 sets linear framebuffer
    video_bios_interrupt(0x4F02, regs, stack_top)
}

struct VbeModeInfo {
    width: u16,
    height: u16,
    pitch: u16,
    bpp: u8,
    framebuffer: u32,
}

#[repr(C, packed)]
struct ModeInfoStruct {
    attributes: u16,
    win_a: u8,
    win_b: u8,
    granularity: u16,
    window_size: u16,
    segment_a: u16,
    segment_b: u16,
    win_func_ptr: u32,
    pitch: u16,
    width: u16,
    height: u16,
    w_char: u8,
    y_char: u8,
    planes: u8,
    bpp: u8,
    banks: u8,
    mem_model: u8,
    bank_size: u8,
    image_pages: u8,
    reserved0: u8,

    red_mask: u8,
    red_position: u8,
    green_mask: u8,
    green_position: u8,
    blue_mask: u8,
    blue_position: u8,
    reserved_mask: u8,
    reserved_position: u8,
    direct_color_attributes: u8,
    framebuffer: u32,
    off_screen_mem_offset: u32,
    off_screen_mem_size: u16,
    reserved1: [u8; 206],
}

fn get_vbe_mode_info(mode: u16, info_paddr: u32, regs: &mut VMRegisters, stack_top: u32) {
    let data_frame = map_memory(Some(0x0000_9000), 0x1000, None).unwrap();

    regs.ecx = mode as u32;
    regs.es = 0;
    regs.edi = data_frame as u32;

    video_bios_interrupt(0x4F01, regs, stack_top);

    let mode_info = unsafe { &mut *(data_frame as *mut ModeInfoStruct) };

    let info_offset = info_paddr & 0xfff;
    let info_page = map_memory(None, 0x1000, Some(info_paddr & 0xfffff000)).unwrap();
    let info_ptr = (info_page + info_offset) as *mut VbeModeInfo;
    let vbe_info = unsafe { &mut *info_ptr };

    vbe_info.width = mode_info.width;
    vbe_info.height = mode_info.height;
    vbe_info.pitch = mode_info.pitch;
    vbe_info.bpp = mode_info.bpp;
    vbe_info.framebuffer = mode_info.framebuffer;
}

fn video_bios_interrupt(function: u16, regs: &mut VMRegisters, stack_top: u32) {
    let int_10_ip: u16 = unsafe { core::ptr::read_volatile(0x0000_0040 as *const u16) };
    let int_10_segment: u16 = unsafe { core::ptr::read_volatile(0x0000_0042 as *const u16) };
    regs.eax = function as u32;
    regs.eip = int_10_ip as u32;
    regs.cs = int_10_segment as u32;

    regs.esp = stack_top;
    unsafe {
        // push flags
        regs.esp -= 2;
        *(regs.esp as *mut u16) = 0;
        // push cs
        regs.esp -= 2;
        *(regs.esp as *mut u16) = 0;
        // push ip
        regs.esp -= 2;
        *(regs.esp as *mut u16) = 0;
    }

    loop {
        enter_8086(regs, 0);

        unsafe {
            let mut op_ptr = ((regs.cs << 4) + regs.eip) as *const u8;
            match *op_ptr {
                0x9c => {
                    // PUSHF
                    regs.esp = regs.esp.wrapping_sub(2) & 0xffff;
                    *(regs.esp as *mut u16) = regs.eflags as u16;
                    regs.eip += 1;
                }
                0x9d => {
                    // POPF
                    let flags = *(regs.esp as *mut u16);
                    regs.esp = regs.esp.wrapping_add(2) & 0xffff;
                    regs.eflags = (flags as u32) | 0x20200;
                    regs.eip += 1;
                }
                0xcf => {
                    // IRET
                    // exit the loop, this marks the end of the interrupt
                    break;
                }
                0xfa => {
                    // CLI
                    regs.eip += 1;
                }
                0xfb => {
                    // STI
                    regs.eip += 1;
                }
                _ => panic!("Unhandled 8086 instruction"),
            }
        }
    }
}
