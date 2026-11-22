use crate::{config, errors, errors::Context, to_volume_string, zfs, ExitCodes};
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

    let transaction = conn
        .transaction()
        .context("could not start a database transaction")?;

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
        Err(error) => {
            return Err(errors::with_context(
                "could not record the new workspace in the database",
                error,
            ));
        }
    };

    // Act like there was a notification sent just now
    // so the user doesn't immediately get spammed with them
    transaction
        .execute(
            "INSERT INTO notifications(workspace_id, timestamp) VALUES (?1, ?2)",
            (transaction.last_insert_rowid(), Utc::now()),
        )
        .context("could not record the new workspace's notification state in the database")?;

    let volume = to_volume_string(&filesystem.root, user, name);

    // Create the dataset while the transaction is still open: should this
    // fail, e.g. because the pool is full, the transaction is rolled back
    // instead of leaving behind a workspace which exists in the database but
    // nowhere else (and which would then block creating it again).
    zfs::create(&volume).context(format!("could not create the dataset {}", volume))?;

    if let Err(error) = transaction.commit() {
        eprintln!(
            "Warning: the dataset {} was created, but could not be registered. \
             Please ask your system administrator to remove it.",
            volume
        );
        return Err(errors::with_context(
            "could not record the new workspace in the database",
            error,
        ));
    }

    // Explicitly request PathBuf so .display() works
    let mountpoint: PathBuf = zfs::get_property::<PathBuf>(&volume, "mountpoint")
        .context(format!("could not find out where {} is mounted", volume))?;

    let mut permissions = fs::metadata(&mountpoint)
        .context(format!(
            "could not read the permissions of {}",
            mountpoint.display()
        ))?
        .permissions();
    permissions.set_mode(0o750);
    fs::set_permissions(&mountpoint, permissions).context(format!(
        "could not set the permissions of {}",
        mountpoint.display()
    ))?;

    let status = Command::new("chown")
        .args([
            &format!("{}:{}", user, user),
            &mountpoint.to_string_lossy().to_string(),
        ])
        .status()
        .context("could not run `chown`")?;
    if !status.success() {
        return Err(errors::with_context(
            format!(
                "could not make {} the owner of {}",
                user,
                mountpoint.display()
            ),
            format!("`chown` exited with {}", status),
        ));
    }

    println!("Created workspace at {}", mountpoint.display());

    // Send "created" email (best-effort)
    if let Some(smtp_cfg) = smtp.as_ref() {
        let host = hostname::get()
            .context("could not determine this host's name")?
            .to_string_lossy()
            .to_string();
        let subject = format!("Workspace {} created on {}", name, host);
        let expiry_days = duration.num_days();
        let body = format!(
            "Hello,\n\nYour workspace \"{}\" has been created on {}.\nFilesystem: {}\nMountpoint: {}\nInitial expiry: in {} days.\n\nYou can extend it with:\n  workspaces extend -f {} -d <days> {}\n",
            name, host, filesystem_name, mountpoint.display(), expiry_days, filesystem_name, name
        );
        crate::maintain::notify_event_or_warn(user, smtp_cfg, "created", subject, body);
    }

    Ok(())
}
