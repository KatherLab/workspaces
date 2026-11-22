use std::{error::Error, process};

use chrono::{Duration, Utc};
use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, errors::Context, to_volume_string, zfs, ExitCodes};

pub fn extend(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    duration: &Duration,
    smtp: &Option<config::SmtpConfig>,
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
                SET expiration_time = MAX(expiration_time, ?2) \
                WHERE id = ?1",
            (workspace_id, Utc::now() + *duration),
        )
        .context("could not update the workspace's expiration date in the database")?;

    // `workspaces expire` may have created a faux notification in the future
    // to silence further notifications;
    // Remove those!
    transaction
        .execute(
            "DELETE FROM notifications \
                WHERE workspace_id = ?1 AND unixepoch(timestamp) > unixepoch(?2)",
            (workspace_id, Utc::now()),
        )
        .context("could not remove the workspace's stale notifications from the database")?;

    if get_current_username().unwrap() == user && get_current_uid() != 0 {
        // The user just acknowledged their workspaces status,
        // so there's no need to notify them for the time being
        transaction
            .execute(
                "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
                (workspace_id, Utc::now()),
            )
            .context("could not record the workspace's notification state in the database")?;
    }

    transaction
        .commit()
        .context("could not save the workspace's new expiration date in the database")?;

    let volume = to_volume_string(&filesystem.root, user, name);
    zfs::set_property(&volume, "readonly", "off")
        .context(format!("could not make the dataset {} writable", volume))?;

    // Find the current (post-update) expiration_time
    let new_expiration: chrono::DateTime<chrono::Utc> = conn
        .prepare("SELECT expiration_time FROM workspaces WHERE filesystem=?1 AND user=?2 AND name=?3")
        .context("could not query the workspaces database")?
        .query_row((filesystem_name, user, name), |row| row.get(0))
        .context("could not read the workspace's new expiration date from the database")?;

    if let Some(smtp_cfg) = smtp.as_ref() {
        let host = hostname::get()
            .context("could not determine this host's name")?
            .to_string_lossy()
            .to_string();
        let subject = format!("Workspace {} extended on {}", name, host);
        let body = format!(
            "Hello,\n\nYour workspace \"{}\" on {} was extended.\nFilesystem: {}\nNew expiry date: {}\n(days until expiry: {} days)\n",
            name, host, filesystem_name, new_expiration, (new_expiration - chrono::Utc::now()).num_days()
        );
        crate::maintain::notify_event_or_warn(user, smtp_cfg, "extended", subject, body);
    }

    Ok(())
}
