use std::process;

use chrono::{Duration, Utc};
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, to_volume_string, zfs, ExitCodes};


pub fn extend(
    conn: &Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    duration: &Duration,
) {
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }
    if filesystem.disabled && get_current_uid() != 0 {
        eprintln!("Filesystem is disabled. Please recreate workspace on another filesystem.");
        process::exit(ExitCodes::FsDisabled as i32);
    }
    if duration > &filesystem.max_duration && get_current_uid() != 0 {
        eprintln!(
            "Duration can be at most {} days",
            filesystem.max_duration.num_days()
        );
        process::exit(ExitCodes::TooHighDuration as i32);
    }

    let rows_updated = conn
        .execute(
            "UPDATE workspaces
            SET expiration_time = MAX(expiration_time, ?1)
            WHERE filesystem = ?2
                AND user = ?3
                AND name = ?4",
            (Utc::now() + *duration, filesystem_name, user, name),
        )
        .unwrap();
    match rows_updated {
        0 => {
            eprintln!(
                "Could not find a matching filesystem={}, user={}, name={}",
                filesystem_name, user, name
            );
            process::exit(ExitCodes::UnknownWorkspace as i32);
        }
        1 => {}
        _ => unreachable!(),
    };

    zfs::set_property(
        &to_volume_string(&filesystem.root, user, name),
        "readonly",
        "off",
    )
    .unwrap();
}
