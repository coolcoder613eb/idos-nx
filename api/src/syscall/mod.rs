pub mod exec;
pub mod io;
pub mod memory;
pub mod pci;
pub mod time;

use core::arch::asm;

pub fn syscall(a: u32, b: u32, c: u32, d: u32) -> u32 {
    let result: u32;
    unsafe {
        asm!(
            "int 0x2b",
            inout("eax") a => result,
            in("ebx") b,
            in("ecx") c,
            in("edx") d,
        );
    }
    result
}

pub fn syscall_2(a: u32, b: u32, c: u32, d: u32) -> (u32, u32) {
    let result: u32;
    let result_2: u32;
    unsafe {
        asm!(
            "int 0x2b",
            inout("eax") a => result,
            inout("ebx") b => result_2,
            in("ecx") c,
            in("edx") d,
        );
    }
    (result, result_2)
}
