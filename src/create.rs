use crate::{config, to_volume_string, zfs, ExitCodes};
use chrono::{Duration, Utc};
use rusqlite::Connection;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
) {
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
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

    let transaction = conn.transaction().unwrap();
    match transaction.execute(
        "INSERT INTO workspaces (filesystem, user, name, expiration_time)
            VALUES (?1, ?2, ?3, ?4)",
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

    let volume = to_volume_string(&filesystem.root, user, name);

    zfs::create(&volume).unwrap();

    let mountpoint = zfs::get_property(&volume, "mountpoint").unwrap();

    let mut permissions = fs::metadata(&mountpoint).unwrap().permissions();
    permissions.set_mode(0o750);
    fs::set_permissions(&mountpoint, permissions).unwrap();

    let status = Command::new("chown")
        .args([&format!("{}:{}", user, user), &mountpoint])
        .status()
        .unwrap();
    assert!(status.success(), "failed to change owner on dataset");
    transaction.commit().unwrap();

    println!("Created workspace at {}", mountpoint);
}
