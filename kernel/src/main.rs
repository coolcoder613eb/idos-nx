#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(adt_const_params)]
#![feature(alloc_error_handler)]
#![feature(custom_test_frameworks)]
#![feature(map_try_insert)]
#![feature(naked_functions)]
#![feature(new_range_api)]
#![feature(vec_into_raw_parts)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::arch::asm;

extern crate alloc;

pub mod acpi;
pub mod arch;
pub mod cleanup;
pub mod collections;
pub mod config;
pub mod conman;
pub mod console;
pub mod exec;
pub mod executor;
pub mod files;
pub mod graphics;
pub mod hardware;
pub mod init;
pub mod interrupts;
pub mod io;
pub mod log;
pub mod memory;
pub mod net;
pub mod panic;
pub mod pipes;
pub mod random;
pub mod sync;
pub mod task;
pub mod time;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        init::init_cpu_tables();
        init::init_memory();
    }

    // Read COM port base addresses from the BIOS Data Area (0x400-0x407).
    // The identity mapping from init_memory still covers this region.
    for i in 0..4u16 {
        let base_port = unsafe { core::ptr::read_volatile((0x400 + i * 2) as *const u16) };
        if base_port != 0 {
            hardware::com::serial::init_port(i as usize, base_port);
        }
    }

    kprint!("\nKernel Memory initialized.\n");

    let initial_pagedir = memory::virt::page_table::get_current_pagedir();
    let bsp_cpu_scheduler = task::switching::init(initial_pagedir);

    acpi::init();

    init::init_hardware();

    task::actions::lifecycle::create_kernel_task(cleanup::cleanup_resident, Some("CLEANUPR"));

    init::init_device_drivers();

    io::init_async_io_system();

    #[cfg(test)]
    {
        task::actions::lifecycle::create_kernel_task(run_tests, Some("TESTS"));
    }

    #[cfg(not(test))]
    {
        task::actions::lifecycle::create_kernel_task(init_system, Some("INIT"));
    }

    loop {
        unsafe {
            // Disable interrupts because task switching is not safe to interrupt
            asm!("cli");
            task::scheduling::switch();
            // When this is reached, it means the BSP has run out of available
            // work -- all available tasks are blocked.
            // Resuming interrupts and halting the CPU saves power until something
            // interesting happens.
            asm!("sti", "hlt",);
        }
    }
}

fn init_system() -> ! {
    let mut logger = log::BufferedLogger::new();

    let id = task::switching::get_current_id();
    crate::kprintln!("INIT task: {:?}", id);

    // Bootstrap: these must be hardcoded because they're needed to read
    // the config file from C:\.
    hardware::ps2::install_drivers();

    logger.log("Installing ATA Drivers...\n");
    hardware::ata::install();

    // Check if a block device driver was loaded by the bootloader (floppy boot)
    let blkdrv_info = exec::get_blkdrv_boot_info();
    let floppy_boot = blkdrv_info.0 != 0 && blkdrv_info.1 != 0;
    exec::set_floppy_boot(floppy_boot);

    if floppy_boot {
        logger.log("Floppy boot: launching block device driver...\n");
        io::filesystem::fatfs::launch_block_driver(blkdrv_info);
        logger.log("Mounting A:\\ on FD1...\n");
        io::filesystem::fatfs::mount_fat_fs_single("A", "FD1");
    }

    logger.log("Mounting C:\\ ...\n");
    io::filesystem::fatfs::mount_fat_fs_single("C", "ATA1");

    // Read config file now that filesystems are available
    let config_path = if floppy_boot {
        "A:\\DRIVERS.CFG"
    } else {
        "C:\\DRIVERS.CFG"
    };
    logger.log("Reading DRIVERS.CFG...\n");
    let directives = config::read_config(config_path);

    let mut exec_path: Option<alloc::string::String> = None;

    if directives.is_empty() {
        logger.log("Warning: no directives found in DRIVERS.CFG, using defaults\n");
        graphics::register_graphics_driver("C:\\GFX.ELF");
        console::init_console();
    } else {
        for directive in &directives {
            if let config::Directive::Exec(path) = directive {
                exec_path = Some(path.clone());
            } else {
                execute_directive(&mut logger, directive);
            }
        }
    }

    if let Some(path) = exec_path {
        // Direct exec mode: launch program without console manager
        logger.log("Exec: ");
        logger.log(path.as_str());
        logger.log("\n");
        drop(logger);

        let (_, child_task) = task::actions::handle::create_task();
        task::actions::lifecycle::add_args(child_task, [path.as_str()]);
        exec::exec_program(child_task, path.as_str()).unwrap();
    } else {
        let con = task::actions::handle::create_file_handle();
        task::actions::io::open_sync(con, "DEV:\\CON1", 0).unwrap();

        logger.log("\nSystem ready! Welcome to IDOS\n\n");
        logger.flush_to_file(con);
        console::console_ready();
    }

    let wake_set = task::actions::sync::create_wake_set();
    loop {
        task::actions::sync::block_on_wake_set(wake_set, None);
    }
}

fn execute_directive(logger: &mut log::BufferedLogger, directive: &config::Directive) {
    use config::Directive;
    match directive {
        Directive::Driver(name) => {
            match name.as_str() {
                "ps2" => {
                    // PS2 is already installed in bootstrap, skip
                    logger.log("Driver ps2 already installed (bootstrap)\n");
                }
                "ata" => {
                    // ATA is already installed in bootstrap, skip
                    logger.log("Driver ata already installed (bootstrap)\n");
                }
                "floppy" => {
                    logger.log("Warning: floppy is now a userspace driver, use 'isa' directive\n");
                }
                _ => {
                    logger.log("Unknown driver: ");
                    logger.log(name.as_str());
                    logger.log("\n");
                }
            }
        }
        Directive::Isa { path, irq } => {
            use crate::task::actions::handle::{create_pipe_handles, create_task, transfer_handle};
            use crate::task::actions::io::{close_sync, read_sync};
            use crate::task::actions::lifecycle::add_args;

            logger.log("ISA driver: ");
            logger.log(path.as_str());
            logger.log("\n");

            let (response_reader, response_writer) = create_pipe_handles();

            let (_, driver_task) = create_task();
            transfer_handle(response_writer, driver_task);

            let irq_str = alloc::format!("{}", irq);
            add_args(driver_task, [path.as_str(), irq_str.as_str()]);

            crate::exec::exec_program(driver_task, path.as_str()).unwrap();

            // Wait for ready signal
            let _ = read_sync(response_reader, &mut [0u8], 0);
            let _ = close_sync(response_reader);
        }
        Directive::Pci {
            vendor_id,
            device_id,
            path,
            busmaster,
        } => {
            use crate::hardware::pci::{devices::PciDevice, get_bus_devices};
            use crate::task::actions::handle::{create_pipe_handles, create_task, transfer_handle};
            use crate::task::actions::io::{close_sync, read_sync, write_sync};
            use idos_api::syscall::pci::PciDeviceQuery;

            logger.log("PCI driver: ");
            logger.log(path.as_str());
            logger.log("\n");

            let devices = get_bus_devices();
            let found = devices
                .iter()
                .find(|dev| dev.vendor_id == *vendor_id && dev.device_id == *device_id);
            let pci_dev = match found {
                Some(dev) => dev.clone(),
                None => {
                    logger.log("  PCI device not found\n");
                    return;
                }
            };

            if *busmaster {
                let dev = PciDevice::read_from_bus(pci_dev.bus, pci_dev.device, pci_dev.function);
                dev.enable_bus_master();
            }

            // Build PciDeviceQuery to send to the driver
            let mut query = PciDeviceQuery::new(*vendor_id, *device_id);
            query.bus = pci_dev.bus;
            query.device = pci_dev.device;
            query.function = pci_dev.function;
            query.irq = pci_dev.irq.unwrap_or(0);
            for i in 0..6 {
                query.bar[i] = match pci_dev.bar[i] {
                    Some(bar) => bar.0,
                    None => 0,
                };
            }

            let (args_reader, args_writer) = create_pipe_handles();
            let (response_reader, response_writer) = create_pipe_handles();

            let (_, driver_task) = create_task();
            transfer_handle(args_reader, driver_task);
            transfer_handle(response_writer, driver_task);

            crate::exec::exec_program(driver_task, path.as_str()).unwrap();

            // Send PciDeviceQuery to the driver
            let query_bytes = unsafe {
                core::slice::from_raw_parts(
                    &query as *const PciDeviceQuery as *const u8,
                    core::mem::size_of::<PciDeviceQuery>(),
                )
            };
            let _ = write_sync(args_writer, query_bytes, 0);
            let _ = close_sync(args_writer);

            // Wait for ready signal
            let _ = read_sync(response_reader, &mut [0u8], 0);
            let _ = close_sync(response_reader);
        }
        Directive::Mount {
            drive_letter,
            fs_type,
            device,
        } => match fs_type.as_str() {
            "FAT" => {
                logger.log("Mounting ");
                logger.log(drive_letter.as_str());
                logger.log(":\\ ...\n");
                io::filesystem::fatfs::mount_fat_fs_single(drive_letter.as_str(), device.as_str());
            }
            _ => {
                logger.log("Unknown filesystem type: ");
                logger.log(fs_type.as_str());
                logger.log("\n");
            }
        },
        Directive::Graphics(path) => {
            logger.log("Initializing Graphics Driver...\n");
            graphics::register_graphics_driver(path.as_str());
        }
        Directive::Console => {
            console::init_console();
        }
        Directive::Net => {
            logger.log("Initializing Net Stack...\n");
            net::start_net_stack();
        }
        Directive::Timezone(offset) => {
            logger.log("Setting timezone offset\n");
            time::system::set_timezone_offset(*offset);
        }
        Directive::Exec(_) => {
            // Handled separately in init_system, should not reach here
        }
    }
}

#[cfg(test)]
fn run_tests() -> ! {
    test_main();
    loop {}
}

#[cfg(test)]
fn test_runner(tests: &[&dyn Fn()]) -> ! {
    kprint!("Running {} tests\n", tests.len());
    for test in tests {
        kprint!("... ");
        test();
        kprint!("[ok]\n");
    }
    kprint!("All tests passed!\n");
    kprint!("Exiting in 5 seconds\n");
    task::actions::sleep(5000);
    hardware::qemu::debug_exit(0);
}
