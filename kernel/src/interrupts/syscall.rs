use core::arch::global_asm;

use idos_api::{compat::VMRegisters, io::AsyncOp, ipc::Message};

use crate::{
    io::handle::Handle,
    log::TaggedLogger,
    memory::address::{PhysicalAddress, VirtualAddress},
    task::{
        actions::{
            self,
            lifecycle::InMemoryArgsIterator,
            memory::{map_file, map_memory, unmap_memory},
            send_message,
        },
        id::TaskID,
        map::get_task,
        memory::MemoryBacking,
    },
};

global_asm!(
    r#"
.global syscall_handler

syscall_handler:
    push eax
    push ecx
    push edx
    push ebx
    push ebp
    push esi
    push edi
    mov ebx, esp
    push ebx

    call _syscall_inner

    add esp, 4
    pop edi
    pop esi
    pop ebp
    pop ebx
    pop edx
    pop ecx
    pop eax

    iretd
"#
);

#[derive(Clone)]
#[repr(C, packed)]
pub struct SavedRegisters {
    pub edi: u32,
    pub esi: u32,
    pub ebp: u32,
    pub ebx: u32,
    pub edx: u32,
    pub ecx: u32,
    pub eax: u32,
}

#[derive(Clone)]
#[repr(C, packed)]
pub struct FullSavedRegisters {
    pub edi: u32,
    pub esi: u32,
    pub ebp: u32,
    pub ebx: u32,
    pub edx: u32,
    pub ecx: u32,
    pub eax: u32,

    pub eip: u32,
    pub cs: u32,
    pub eflags: u32,
    pub esp: u32,
    pub ss: u32,
}

impl core::fmt::Debug for FullSavedRegisters {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let eax = self.eax;
        let ebx = self.ebx;
        let ecx = self.ecx;
        let edx = self.edx;
        let ebp = self.ebp;
        let esi = self.esi;
        let edi = self.edi;

        let eip = self.eip;
        let cs = self.cs;
        let eflags = self.eflags;
        let esp = self.esp;
        let ss = self.ss;
        f.debug_struct("Saved Registers")
            .field("eax", &format_args!("{:#010X}", eax))
            .field("ebx", &format_args!("{:#010X}", ebx))
            .field("ecx", &format_args!("{:#010X}", ecx))
            .field("edx", &format_args!("{:#010X}", edx))
            .field("ebp", &format_args!("{:#010X}", ebp))
            .field("esi", &format_args!("{:#010X}", esi))
            .field("edi", &format_args!("{:#010X}", edi))
            .field("eip", &format_args!("{:#010X}", eip))
            .field("cs", &format_args!("{:#010X}", cs))
            .field("eflags", &format_args!("{:#010X}", eflags))
            .field("esp", &format_args!("{:#010X}", esp))
            .field("ss", &format_args!("{:#010X}", ss))
            .finish()
    }
}

const LOGGER: TaggedLogger = TaggedLogger::new("SYSCALL", 93);

fn log_syscall(registers: &FullSavedRegisters) {
    let eax = registers.eax;
    let name = match eax {
        0x00 => "exit",
        0x01 => "yield",
        0x02 => "sleep",
        0x03 => "get current task id",
        0x04 => "get parent task id",
        0x05 => "add args to task",
        0x06 => "load executable",
        0x07 => "enter 8086",
        0x10 => "submit async io op",
        0x11 => "send message",
        0x12 => "driver io complete",
        0x13 => "futex wait",
        0x14 => "futex wake",
        0x15 => "create wake set",
        0x16 => "block on wake set",
        0x20 => "create task",
        0x21 => "open message queue",
        0x22 => "open irq handle",
        0x23 => "create file handle",
        0x24 => "create pipe handles",
        0x25 => "create udp socket handle",
        0x26 => "create tcp socket handle",
        0x2b => "dup handle",
        0x30 => "map memory",
        0x31 => "map file",
        0x32 => "unmap memory",
        0x40 => "get monotonic ms",
        0x41 => "get system time",
        0x50 => "register filesystem",
        0x51 => "register device",
        0x52 => "register network device",
        0x60 => "query pci device",
        0x61 => "enable pci bus master",
        0x62 => "map dma memory",
        0xffff => "internal debug",
        _ => "unknown",
    };

    let current_id = crate::task::switching::get_current_id();

    LOGGER.log(format_args!(
        "{:#02X} {} (Task {})\n          {:?}",
        eax, name, current_id, registers
    ));
}

#[no_mangle]
pub extern "C" fn _syscall_inner(registers: &mut FullSavedRegisters) {
    log_syscall(registers);
    let eax = registers.eax;
    match eax {
        // task lifecycle and interop
        0x00 => {
            // exit
            let code = registers.ebx;
            actions::lifecycle::terminate(code);
        }
        0x01 => {
            // yield coop
            actions::yield_coop();
        }
        0x02 => {
            // sleep
            let duration = registers.ebx;
            actions::sleep(duration);
        }
        0x03 => {
            // get current task id
            let current = crate::task::switching::get_current_id();
            registers.eax = current.into();
        }
        0x04 => {
            // get parent task id
            let current = crate::task::switching::get_current_id();
            let parent = get_task(current).unwrap().read().parent_id;
            registers.eax = parent.into();
        }
        0x05 => {
            // Add args to a task
            // We choose to take the pre-parsed, array-of-strings approach of
            // Unix rather than the raw commandline approach of Windows.
            // This means that the syscall caller is responsible for passing
            // a data format that can be split up by the kernel.
            // The format is designed so that:
            //   - Args should be able to contain any character, including null
            //   - A zero-length arg is perfectly valid
            //   - A list of args can be gradually built up (the offset table is
            //     a trailer at the end, and can be moved further back to add more args)
            //
            // The syscall is additive -- it appends args to the task rather than
            // replacing them. Calling this method multiple times will not modify
            // previously-added args.
            //
            // The format is as follows:
            // [u16 len1][arg1][u16 len2][arg2]...[u16 lenN][argN]
            //
            // And three registers are passed to the syscall:
            // ebx: task id to add the args to
            // ecx: virtual address of the start of the buffer
            // edx: length of the buffer in bytes
            let task_id = TaskID::new(registers.ebx);
            match get_task(task_id) {
                Some(task_lock) => {
                    let args_ptr = registers.ecx as *const u8;
                    let args_len = registers.edx as usize;
                    let args_iter = InMemoryArgsIterator::new(args_ptr, args_len);
                    task_lock.write().push_args(args_iter);
                    registers.eax = 1;
                }
                None => {
                    registers.eax = 0xffff_ffff;
                }
            }
        }
        0x06 => {
            // load executable
            let task_id = TaskID::new(registers.ebx);
            let path_ptr = registers.ecx as *const u8;
            let path_len = registers.edx as usize;
            let path = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(path_ptr, path_len))
            };
            match crate::exec::exec_program(task_id, path) {
                Ok(_) => registers.eax = 1,
                Err(_) => registers.eax = 0xffff_ffff,
            }
        }
        0x07 => {
            // enter 8086 VM mode
            // This syscall is more complex than the rest, since it will not
            // return. Instead it will switch to 8086 mode and begin executing
            // somewhere else. The first time that code triggers a GPF, it will
            // appear as though this syscall has returned.
            // In order for that to work, we need to save the registers now.
            // If a GPF is found to have started in the VM, we can restore the
            // registers and IRET, returning to the callsite in userspace.
            let regs_ptr = registers.ebx as *mut VMRegisters;
            crate::task::actions::vm::enter_vm86_mode(registers, regs_ptr);
        }

        // IO Actions
        0x10 => {
            // submit async io op
            let handle = Handle::new(registers.ebx as usize);
            let op_ptr = registers.ecx as *const AsyncOp;
            let op = unsafe { &*op_ptr };
            let wake_set = match registers.edx {
                0xffff_ffff => None,
                edx => Some(Handle::new(edx as usize)),
            };
            match actions::io::send_io_op(handle, op, wake_set) {
                Ok(_) => registers.eax = 1,
                // TODO: error codes
                Err(_e) => registers.eax = 0x8000_0000,
            }
        }
        0x11 => {
            // send message
            let send_to = TaskID::new(registers.ebx);
            let message_ptr = registers.ecx as *const Message;
            let message = unsafe { &*message_ptr };
            let expiration = registers.edx;
            send_message(send_to, *message, expiration);
        }
        0x12 => {
            // driver io complete
            let request_id = registers.ebx;
            let return_value = if registers.ecx & 0x80000000 != 0 {
                // error
                let io_error =
                    idos_api::io::error::IoError::try_from(registers.ecx & 0x7fffffff)
                        .unwrap_or(idos_api::io::error::IoError::Unknown);
                Err(io_error)
            } else {
                Ok(registers.ecx)
            };
            crate::io::driver::pending::request_complete(request_id, return_value);
        }
        0x13 => {
            // futex wait
            let address = VirtualAddress::new(registers.ebx);
            let value = registers.ecx;
            let timeout = match registers.edx {
                0xffff_ffff => None,
                edx => Some(edx),
            };
            crate::sync::futex::futex_wait(address, value, timeout)
        }
        0x14 => {
            // futex wake
            let address = VirtualAddress::new(registers.ebx);
            let count = registers.ecx;
            crate::sync::futex::futex_wake(address, count)
        }
        0x15 => {
            // create wake set
            let handle = actions::sync::create_wake_set();
            registers.eax = *handle as u32;
        }
        0x16 => {
            // block on wake set
            let handle = Handle::new(registers.ebx as usize);
            let timeout = match registers.ecx {
                0xffff_ffff => None,
                edx => Some(edx),
            };
            actions::sync::block_on_wake_set(handle, timeout);
        }

        // handle actions
        0x20 => {
            // create task
            let (handle, task_id) = actions::handle::create_task();
            registers.eax = *handle as u32;
            registers.ebx = task_id.into();
        }
        0x21 => {
            // create message queue handle
            let handle = actions::handle::open_message_queue();
            registers.eax = *handle as u32;
        }
        0x22 => {
            // create irq handle
            let irq = registers.ebx;
            let handle = actions::handle::open_interrupt_handle(irq as u8);
            registers.eax = *handle as u32;
        }
        0x23 => {
            // create file handle
            let handle = actions::handle::create_file_handle();
            registers.eax = *handle as u32;
        }
        0x24 => {
            // create pipe handles
            let (read_handle, write_handle) = actions::handle::create_pipe_handles();
            registers.eax = *read_handle as u32;
            registers.ebx = *write_handle as u32;
        }
        0x25 => {
            // create udp socket handle
            unimplemented!()
        }
        0x26 => {
            // create tcp socket handle
            unimplemented!()
        }

        0x2a => {
            // transfer handle
            let handle = Handle::new(registers.ebx as usize);
            let task_id = TaskID::new(registers.ecx);
            let result = actions::handle::transfer_handle(handle, task_id);
            registers.eax = match result {
                Some(handle) => *handle as u32,
                None => 0xffff_ffff,
            }
        }
        0x2b => {
            // dup handle
            let handle = Handle::new(registers.ebx as usize);
            let result = actions::handle::dup_handle(handle);
            registers.eax = match result {
                Some(handle) => *handle as u32,
                None => 0xffff_ffff,
            }
        }

        // memory actions
        0x30 => {
            // map memory
            let address = match registers.ebx {
                0xffff_ffff => None,
                ebx => Some(VirtualAddress::new(ebx)),
            };
            let size = registers.ecx;
            let backing = match registers.edx {
                0xffff_ffff => MemoryBacking::FreeMemory,
                0xffff_fffe => MemoryBacking::IsaDma,
                address => MemoryBacking::Direct(PhysicalAddress::new(address)),
            };
            match map_memory(address, size, backing) {
                Ok(vaddr) => {
                    // For contiguous (IsaDma) allocations, trigger paging
                    // immediately so all frames are allocated now.
                    if registers.edx == 0xffff_fffe {
                        crate::task::paging::page_on_demand(vaddr);
                    }
                    registers.eax = vaddr.into();
                }
                Err(_e) => {
                    // TODO: we need error codes
                    registers.eax = 0xffff_ffff;
                }
            }
        }
        0x31 => {
            // map file
            use idos_api::syscall::memory::FileMapping;
            let mapping_ptr = registers.ebx as *const FileMapping;
            let mapping = unsafe { &*mapping_ptr };

            let address = match mapping.virtual_address {
                0xffff_ffff => None,
                vaddr => Some(VirtualAddress::new(vaddr)),
            };
            let size = mapping.size;
            let path_slice = unsafe {
                core::slice::from_raw_parts(
                    mapping.path_ptr as *const u8,
                    mapping.path_len as usize,
                )
            };
            let path = match core::str::from_utf8(path_slice) {
                Ok(s) => s,
                Err(_e) => {
                    registers.eax = 0xffff_ffff;
                    return;
                }
            };
            let file_offset = mapping.file_offset;
            let shared = mapping.flags & idos_api::syscall::memory::MMAP_SHARED != 0;
            match map_file(address, size, path, file_offset, shared) {
                Ok(vaddr) => {
                    registers.eax = vaddr.into();
                }
                Err(_e) => {
                    registers.eax = 0xffff_ffff;
                }
            }
        }

        0x32 => {
            // unmap memory
            let address = VirtualAddress::new(registers.ebx);
            let size = registers.ecx;
            match unmap_memory(address, size) {
                Ok(()) => {
                    registers.eax = 0;
                }
                Err(_e) => {
                    registers.eax = 0xffff_ffff;
                }
            }
        }

        // time
        0x40 => {
            // get monotonic ms
            // Returns a u64 millisecond counter split across eax (low) and ebx (high)
            let ms = crate::time::system::get_monotonic_ms();
            registers.eax = ms as u32;
            registers.ebx = (ms >> 32) as u32;
        }

        0x41 => {
            // get system time
            // Returns Timestamp::now().as_u32() (seconds since 1980-01-01) in eax
            let ts = crate::time::system::Timestamp::now();
            registers.eax = ts.as_u32();
        }

        0x60 => {
            // query PCI device
            // ebx = pointer to PciDeviceQuery struct
            // Returns eax = 0 on success, 0xffff_ffff if not found
            let query_ptr = registers.ebx as *mut idos_api::syscall::pci::PciDeviceQuery;
            let query = unsafe { &mut *query_ptr };
            let devices = crate::hardware::pci::get_bus_devices();
            let found = devices.iter().find(|dev| {
                dev.vendor_id == query.vendor_id && dev.device_id == query.device_id
            });
            match found {
                Some(dev) => {
                    query.bus = dev.bus;
                    query.device = dev.device;
                    query.function = dev.function;
                    query.irq = dev.irq.unwrap_or(0);
                    for i in 0..6 {
                        query.bar[i] = match dev.bar[i] {
                            Some(bar) => bar.0,
                            None => 0,
                        };
                    }
                    registers.eax = 0;
                }
                None => {
                    registers.eax = 0xffff_ffff;
                }
            }
        }
        0x61 => {
            // enable PCI bus mastering
            // ebx = bus, ecx = device, edx = function
            let bus = registers.ebx as u8;
            let device = registers.ecx as u8;
            let function = registers.edx as u8;
            use crate::hardware::pci::devices::PciDevice;
            let dev = PciDevice::read_from_bus(bus, device, function);
            dev.enable_bus_master();
            registers.eax = 0;
        }
        0x62 => {
            // map DMA memory (returns both vaddr and paddr)
            // ebx = size
            // Returns: eax = vaddr, ebx = paddr
            let size = registers.ebx;
            match map_memory(None, size, MemoryBacking::IsaDma) {
                Ok(vaddr) => {
                    match crate::task::paging::page_on_demand(vaddr) {
                        Some(paddr) => {
                            registers.eax = vaddr.into();
                            registers.ebx = paddr.into();
                        }
                        None => {
                            registers.eax = 0xffff_ffff;
                        }
                    }
                }
                Err(_e) => {
                    registers.eax = 0xffff_ffff;
                }
            }
        }

        0x50 => {
            // register filesystem driver
            // ebx = pointer to name string, ecx = name length
            let name_ptr = registers.ebx as *const u8;
            let name_len = registers.ecx as usize;
            let name = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len))
            };
            let caller_id = crate::task::switching::get_current_id();
            let driver_id = crate::io::filesystem::install_task_fs(name, caller_id);
            registers.eax = *driver_id;
        }
        0x51 => {
            // register device driver
            // ebx = pointer to name string, ecx = name length
            let name_ptr = registers.ebx as *const u8;
            let name_len = registers.ecx as usize;
            let name = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, name_len))
            };
            let caller_id = crate::task::switching::get_current_id();
            let driver_id = crate::io::filesystem::install_task_dev(name, caller_id, 0);
            registers.eax = *driver_id;
        }
        0x52 => {
            // register network device
            // ebx = path_ptr, ecx = path_len, edx = mac_ptr (6 bytes)
            let path_ptr = registers.ebx as *const u8;
            let path_len = registers.ecx as usize;
            let mac_ptr = registers.edx as *const u8;
            let path = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(path_ptr, path_len))
            };
            let mac: [u8; 6] = unsafe {
                let slice = core::slice::from_raw_parts(mac_ptr, 6);
                [slice[0], slice[1], slice[2], slice[3], slice[4], slice[5]]
            };
            crate::net::resident::register_network_device(path, mac);
            registers.eax = 0;
        }

        0xffff => {
            crate::kprint!("\n\nSyscall: DEBUG\n");
            registers.eax = 0;
        }
        _ => panic!("Unknown Syscall!"),
    }
}
