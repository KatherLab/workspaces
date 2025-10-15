use crate::{config, to_volume_string, zfs, ExitCodes};
use chrono::{Duration, Utc};
use rusqlite::Connection;
use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{self, Command},
};
use users::{get_current_uid, get_current_username};

/// Creates a new workspace
pub fn create(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    duration: &Duration,
    smtp: &Option<config::SmtpConfig>, // <-- added parameter
) -> Result<(), Box<dyn Error>> {
    if get_current_username().expect("couldn't get username") != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }
    if filesystem.disabled && get_current_uid() != 0 {
        eprintln!("Filesystem is disabled. Please try another filesystem.");
        process::exit(ExitCodes::FsDisabled as i32);
    }
    if duration > &filesystem.max_duration && get_current_uid() != 0 {
        eprintln!(
            "Duration can be at most {} days",
            filesystem.max_duration.num_days()
        );
        process::exit(ExitCodes::TooHighDuration as i32);
    }

    conn.transaction().inspect(
        |transaction| {
            match transaction.execute(
                "INSERT INTO workspaces(filesystem, user, name, expiration_time) \
                    VALUES(?1, ?2, ?3, ?4)",
                (filesystem_name, user, name, Utc::now() + *duration),
            ) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(
                    libsqlite3_sys::Error {
                        code: libsqlite3_sys::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _,
                )) => {
                    eprintln!(
                        "This workspace already exists. You can extend it using `workspaces extend`."
                    );
                    process::exit(ExitCodes::WorkspaceExists as i32);
                }
                Err(_) => unreachable!(),
            };

            // Act like there was a notification sent just now
            // so the user doesn't immediately get spammed with them
            transaction.execute(
                "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
                (transaction.last_insert_rowid(), Utc::now()),
            ).unwrap();
        }
    )?.commit()?;

    let volume = to_volume_string(&filesystem.root, user, name);

    zfs::create(&volume)?;

    // Explicitly request PathBuf so .display() works
    let mountpoint: PathBuf = zfs::get_property::<PathBuf>(&volume, "mountpoint")?;

    let mut permissions = fs::metadata(&mountpoint)?.permissions();
    permissions.set_mode(0o750);
    fs::set_permissions(&mountpoint, permissions)?;

    let status = Command::new("chown")
        .args([&format!("{}:{}", user, user), &mountpoint.to_string_lossy().to_string()])
        .status()?;
    assert!(status.success(), "failed to change owner on dataset");

    println!("Created workspace at {}", mountpoint.display());

    // Send "created" email (best-effort)
    if let Some(smtp_cfg) = smtp.as_ref() {
        let host = hostname::get()?.to_string_lossy().to_string();
        let subject = format!("Workspace {} created on {}", name, host);
        let expiry_days = duration.num_days();
        let body = format!(
            "Hello,\n\nYour workspace \"{}\" has been created on {}.\nFilesystem: {}\nMountpoint: {}\nInitial expiry: in {} days.\n\nYou can extend it with:\n  workspaces extend -f {} -d <days> {}\n",
            name, host, filesystem_name, mountpoint.display(), expiry_days, filesystem_name, name
        );
        if let Err(e) = crate::maintain::notify_event(user, smtp_cfg, subject, body) {
            eprintln!("Failed to send 'created' email: {}", e);
        }
    }

    Ok(())
}
