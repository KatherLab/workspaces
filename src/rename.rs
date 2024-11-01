use std::process;

use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, to_volume_string, zfs, ExitCodes};

/// Renames an existing workspace
pub fn rename(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    src_name: &str,
    dest_name: &str,
) {
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }
    if filesystem.disabled && get_current_uid() != 0 {
        eprintln!("Filesystem is disabled. Please try another filesystem.");
        process::exit(ExitCodes::FsDisabled as i32);
    }

    let transaction = conn.transaction().unwrap();
    match transaction.execute(
        "UPDATE workspaces
                SET name = ?1
                WHERE filesystem = ?2
                    AND user = ?3
                    AND name = ?4",
        (dest_name, filesystem_name, user, src_name),
    ) {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(
            libsqlite3_sys::Error {
                code: libsqlite3_sys::ErrorCode::ConstraintViolation,
                ..
            },
            _,
        )) => {
            eprintln!("The target workspace already exists");
            process::exit(ExitCodes::WorkspaceExists as i32);
        }
        Err(_) => unreachable!(),
    }

    let src_volume = to_volume_string(&filesystem.root, user, src_name);
    let dest_volume = to_volume_string(&filesystem.root, user, dest_name);
    zfs::rename(&src_volume, &dest_volume).unwrap();
    transaction.commit().unwrap();
}
