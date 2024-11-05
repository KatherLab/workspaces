use std::process;

use chrono::Utc;
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, to_volume_string, zfs, ExitCodes};

pub fn expire(
    conn: &mut Connection,
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
        // Set the expiration time sufficiently far in the past
        // for it to get cleaned up soon
        Utc::now() - filesystem.expired_retention
    } else {
        Utc::now()
    };

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
                        SET expiration_time = MIN(expiration_time, ?2) \
                        WHERE id = ?1",
                    (workspace_id, expiration_time),
                )
                .unwrap();

            // The user just expired their workspace,
            // so they probably don't need notifications right away
            transaction
                .execute(
                    "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
                    (workspace_id, Utc::now()),
                )
                .unwrap();
        })
        .unwrap()
        .commit()
        .unwrap();

    zfs::set_property(
        &to_volume_string(&filesystem.root, user, name),
        "readonly",
        "on",
    )
    .unwrap();
}
