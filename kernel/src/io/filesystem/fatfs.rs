use crate::log::TaggedLogger;
use crate::task::actions::handle::{create_pipe_handles, transfer_handle};
use crate::task::actions::io::{close_sync, read_sync, write_sync};

const LOGGER: TaggedLogger = TaggedLogger::new("FATFS", 34);

/// Try to mount a FAT filesystem using the userspace driver loaded by the
/// bootloader as a flat binary. Returns true on success, false if the binary
/// wasn't loaded or the task couldn't be started.
fn try_mount_userspace(drive_letter: &str, dev_name: &str) -> bool {
    use crate::exec::{exec_flat_binary, get_fatdrv_boot_info};
    use crate::task::actions::handle::create_task;

    let (phys_addr, file_size) = get_fatdrv_boot_info();
    if phys_addr == 0 || file_size == 0 {
        LOGGER.log(format_args!("No FATDRV binary loaded by bootloader"));
        return false;
    }

    let (args_reader, args_writer) = create_pipe_handles();
    let (response_reader, response_writer) = create_pipe_handles();

    let (_handle, task_id) = create_task();
    transfer_handle(args_reader, task_id);
    transfer_handle(response_writer, task_id);

    match exec_flat_binary(task_id, phys_addr, file_size) {
        Ok(_) => {}
        Err(e) => {
            LOGGER.log(format_args!("Failed to exec FATDRV flat binary: {:?}", e));
            return false;
        }
    }

    // Protocol: [u8 drive_letter_len][drive_letter][u8 dev_name_len][dev_name]
    let _ = write_sync(args_writer, &[drive_letter.len() as u8], 0);
    let _ = write_sync(args_writer, drive_letter.as_bytes(), 0);
    let _ = write_sync(args_writer, &[dev_name.len() as u8], 0);
    let _ = write_sync(args_writer, dev_name.as_bytes(), 0);

    // Wait for driver to signal ready
    let _ = read_sync(response_reader, &mut [0u8], 0);

    LOGGER.log(format_args!(
        "Successfully started userspace FAT driver for {}:\\",
        drive_letter
    ));
    true
}

pub fn mount_fat_fs_single(drive_letter: &str, dev_name: &str) {
    LOGGER.log(format_args!(
        "Mounting {}:\\ on DEV:\\{}",
        drive_letter, dev_name
    ));
    if !try_mount_userspace(drive_letter, dev_name) {
        LOGGER.log(format_args!("Userspace driver unavailable"));
    }
}
