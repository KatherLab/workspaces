use std::process;

use chrono::Utc;
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, to_volume_string, zfs, ExitCodes};

pub fn expire(
    conn: &Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    delete_on_next_clean: bool,
) {
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }

    let expiration_time = if delete_on_next_clean {
        // set the expiration time sufficiently far in the past
        // for it to get cleaned up soon
        Utc::now() - filesystem.expired_retention
    } else {
        Utc::now()
    };
    let rows_updated = conn
        .execute(
            "UPDATE workspaces
            SET expiration_time = MIN(expiration_time, ?1)
            WHERE filesystem = ?2
                AND user = ?3
                AND name = ?4",
            (expiration_time, filesystem_name, user, name),
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
        "on",
    )
    .unwrap();
}
