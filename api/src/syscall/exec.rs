use core::sync::atomic::AtomicU32;

use crate::{compat::VMRegisters, io::handle::Handle};

pub fn terminate(code: u32) -> ! {
    super::syscall(0, code, 0, 0);
    unreachable!();
}
pub fn yield_coop() {
    super::syscall(1, 0, 0, 0);
}

pub fn futex_wait_u32(atomic: &AtomicU32, value: u32, timeout_opt: Option<u32>) -> u32 {
    let timeout = timeout_opt.unwrap_or(0xffff_ffff);
    super::syscall(0x13, atomic.as_ptr() as u32, value, timeout)
}

pub fn create_task() -> (Handle, u32) {
    let (handle, task_id) = super::syscall_2(0x20, 0, 0, 0);
    (Handle::new(handle), task_id)
}

pub fn add_args(task_id: u32, args_ptr: *const u8, args_len: u32) {
    super::syscall(0x05, task_id, args_ptr as u32, args_len);
}

pub fn load_executable(task_id: u32, path: &str) -> bool {
    let path_ptr = path.as_ptr() as u32;
    let path_len = path.len() as u32;
    let result = super::syscall(0x06, task_id, path_ptr, path_len);
    result != 0xffff_ffff
}

pub fn enter_8086(regs: &mut VMRegisters, flags: u32) -> u32 {
    super::syscall(0x07, regs as *mut VMRegisters as u32, flags, 0)
}
