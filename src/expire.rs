use std::{error::Error, process};

use chrono::Utc;
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, errors::Context, to_volume_string, zfs, ExitCodes};

pub fn expire(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    delete_on_next_clean: bool,
    smtp: &Option<config::SmtpConfig>,
) -> Result<(), Box<dyn Error>> {
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

    let transaction = conn
        .transaction()
        .context("could not start a database transaction")?;
    // Get workspace id
    let workspace_id: i64 = match transaction
        .prepare(
            "SELECT id FROM workspaces \
                WHERE filesystem = ?1 \
                    AND user = ?2 \
                    AND name = ?3",
        )
        .context("could not query the workspaces database")?
        .query_row((filesystem_name, user, name), |row| row.get(0))
    {
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            eprintln!(
                "Could not find a matching filesystem={}, user={}, name={}",
                filesystem_name, user, name
            );
            process::exit(ExitCodes::UnknownWorkspace as i32);
        }
        res => res.context("could not look up the workspace in the database")?,
    };

    transaction
        .execute(
            "UPDATE workspaces \
                SET expiration_time = MIN(expiration_time, ?2) \
                WHERE id = ?1",
            (workspace_id, expiration_time),
        )
        .context("could not update the workspace's expiration date in the database")?;

    if get_current_username().unwrap() == user && get_current_uid() != 0 {
        // The user just expired their workspace, so they don't want deletion notices.
        // We disable them by creating a faux notification in the future.
        // TODO refactor this into a separate column in workspaces.
        transaction
            .execute(
                "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
                (workspace_id, expiration_time),
            )
            .context("could not record the workspace's notification state in the database")?;
    }

    transaction
        .commit()
        .context("could not save the workspace's expiration in the database")?;

    let volume = to_volume_string(&filesystem.root, user, name);
    zfs::set_property(&volume, "readonly", "on")
        .context(format!("could not make the dataset {} read-only", volume))?;

    if let Some(smtp_cfg) = smtp.as_ref() {
        let host = hostname::get()
            .context("could not determine this host's name")?
            .to_string_lossy()
            .to_string();
        let subject = if delete_on_next_clean {
            format!("Workspace {} scheduled for deletion on {}", name, host)
        } else {
            format!("Workspace {} marked expired on {}", name, host)
        };
        let body = if delete_on_next_clean {
            format!(
                "Hello,\n\nYour workspace \"{}\" on {} was marked for deletion on the next cleanup.\nFilesystem: {}\nIt will be removed during the next 'workspaces maintain' run.\n",
                name, host, filesystem_name
            )
        } else {
            format!(
                "Hello,\n\nYour workspace \"{}\" on {} has been marked expired and set read-only.\nFilesystem: {}\nYou can still re-enable it by extending:\n  workspaces extend -f {} -d <days> {}\n",
                name, host, filesystem_name, filesystem_name, name
            )
        };
        if let Err(e) = crate::maintain::notify_event(user, smtp_cfg, subject, body) {
            eprintln!("Failed to send 'expired' email: {}", e);
        }
    }

    Ok(())
}
