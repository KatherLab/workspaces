use std::{error::Error, process};

use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, errors, errors::Context, to_volume_string, zfs, ExitCodes};

/// Renames an existing workspace
pub fn rename(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    src_name: &str,
    dest_name: &str,
) -> Result<(), Box<dyn Error>> {
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }
    if filesystem.disabled && get_current_uid() != 0 {
        eprintln!("Filesystem is disabled. Please try another filesystem.");
        process::exit(ExitCodes::FsDisabled as i32);
    }

    let transaction = conn
        .transaction()
        .context("could not start a database transaction")?;
    match transaction.execute(
        "UPDATE workspaces \
            SET name = ?1 \
            WHERE filesystem = ?2 \
                AND user = ?3 \
                AND name = ?4",
        (dest_name, filesystem_name, user, src_name),
    ) {
        // Nothing was renamed, so there is no such workspace
        Ok(0) => {
            eprintln!(
                "Could not find a matching filesystem={}, user={}, name={}",
                filesystem_name, user, src_name
            );
            process::exit(ExitCodes::UnknownWorkspace as i32);
        }
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
        Err(error) => {
            return Err(errors::with_context(
                "could not rename the workspace in the database",
                error,
            ));
        }
    }

    let src_volume = to_volume_string(&filesystem.root, user, src_name);
    let dest_volume = to_volume_string(&filesystem.root, user, dest_name);
    // Renaming the dataset before committing keeps the database and the
    // datasets in sync should this fail: the transaction is rolled back when
    // it goes out of scope
    zfs::rename(&src_volume, &dest_volume).context(format!(
        "could not rename the dataset {} to {}",
        src_volume, dest_volume
    ))?;
    transaction
        .commit()
        .context("could not save the new workspace name in the database")?;

    Ok(())
}
