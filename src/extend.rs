use std::{error::Error, process};

use chrono::{Duration, Utc};
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, to_volume_string, zfs, ExitCodes};

pub fn extend(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    duration: &Duration,
) -> Result<(), Box<dyn Error>> {
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

    conn.transaction()
        .inspect(|transaction| {
            // Get workspace id
            let workspace_id: i64 = match transaction
                .prepare(
                    "SELECT id FROM workspaces \
                        WHERE filesystem = ?1 \
                            AND user = ?2 \
                            AND name = ?3",
                )
                .unwrap()
                .query_row((filesystem_name, user, name), |row| row.get(0))
            {
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    eprintln!(
                        "Could not find a matching filesystem={}, user={}, name={}",
                        filesystem_name, user, name
                    );
                    process::exit(ExitCodes::UnknownWorkspace as i32);
                }
                res @ _ => res,
            }
            .unwrap();

            transaction
                .execute(
                    "UPDATE workspaces \
                        SET expiration_time = MAX(expiration_time, ?2) \
                        WHERE id = ?1",
                    (workspace_id, Utc::now() + *duration),
                )
                .unwrap();

            // `workspaces expire` may have created a faux notification in the future
            // to silence further notifications;
            // Remove those!
            transaction
                .execute(
                    "DELETE FROM notifications \
                        WHERE workspace_id = ?1 AND unixepoch(timestamp) > unixepoch(?2)",
                    (workspace_id, Utc::now()),
                )
                .unwrap();

            if get_current_username().unwrap() == user && get_current_uid() != 0 {
                // The user just acknowledged their workspaces status,
                // so there's no need to notify them for the time being
                transaction
                    .execute(
                        "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
                        (workspace_id, Utc::now()),
                    )
                    .unwrap();
            }
        })?
        .commit()?;

    zfs::set_property(
        &to_volume_string(&filesystem.root, user, name),
        "readonly",
        "off",
    )
    .unwrap();

    Ok(())
}
