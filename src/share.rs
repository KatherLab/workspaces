use std::{error::Error, path::PathBuf, process};

use rusqlite::Connection;
use users::{get_current_uid, get_current_username};

use crate::{config, errors::Context, to_volume_string, zfs, ExitCodes};

/// Share a workspace with another user via POSIX ACLs, or revoke their access.
///
/// Both the filesystem ACL (`setfacl`) and a row in the `shares` table are
/// kept in sync, so `workspaces list` can show who a workspace has been shared
/// with. Requires `acltype=posixacl` on the dataset (see `zfs set acltype=posixacl`).
pub fn share(
    conn: &mut Connection,
    filesystem_name: &str,
    filesystem: &config::Filesystem,
    user: &str,
    name: &str,
    other_user: &str,
    read_write: bool,
    revoke: bool,
) -> Result<(), Box<dyn Error>> {
    // Only the owner (or root) may share a workspace
    if get_current_username().unwrap() != user && get_current_uid() != 0 {
        eprintln!("You are not allowed to execute this operation");
        process::exit(ExitCodes::InsufficientPrivileges as i32);
    }

    let transaction = conn
        .transaction()
        .context("could not start a database transaction")?;

    // Get workspace id, exiting if it doesn't exist
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

    let volume = to_volume_string(&filesystem.root, user, name);
    let mountpoint: PathBuf = zfs::get_property::<PathBuf>(&volume, "mountpoint")
        .context(format!("could not find out where {} is mounted", volume))?;
    let mp = mountpoint.to_string_lossy().to_string();

    if revoke {
        // Remove access entries everywhere and default entries on directories
        run_setfacl(&["-R", "-x", &format!("u:{}", other_user), &mp])?;
        run_find_setfacl(&[
            mp.as_str(),
            "-type",
            "d",
            "-exec",
            "setfacl",
            "-d",
            "-x",
            &format!("u:{}", other_user),
            "{}",
            "+",
        ])?;
        // Forget the share in the database
        transaction
            .execute(
                "DELETE FROM shares WHERE workspace_id = ?1 AND user = ?2",
                (workspace_id, other_user),
            )
            .context("could not remove the share from the database")?;
        println!(
            "Revoked access for {} on workspace at {}",
            other_user,
            mountpoint.display()
        );
    } else {
        // r-x / rwx for the root and as the inherited default;
        // capital X for the recursive access pass so data files stay non-executable.
        let (perms, recurse_perms) = if read_write {
            ("rwx", "rwx")
        } else {
            ("r-x", "rX")
        };
        let entry = format!("u:{}:{}", other_user, perms);

        // Access + default ACL on the workspace root
        run_setfacl(&["-m", &entry, &mp])?;
        run_setfacl(&["-d", "-m", &entry, &mp])?;
        // Apply the access ACL to already-existing content recursively
        run_setfacl(&[
            "-R",
            "-m",
            &format!("u:{}:{}", other_user, recurse_perms),
            &mp,
        ])?;
        // Default ACL on existing subdirectories so future files inherit access
        run_find_setfacl(&[
            mp.as_str(),
            "-type",
            "d",
            "-exec",
            "setfacl",
            "-d",
            "-m",
            &entry,
            "{}",
            "+",
        ])?;
        // Ensure the ACL mask permits the granted permissions
        if read_write {
            run_setfacl(&["-m", "m::rwx", &mp])?;
        }
        // Remember the share in the database
        // (INSERT OR REPLACE so re-sharing with a different mode updates the row)
        transaction
            .execute(
                "INSERT INTO shares(workspace_id, user, read_write) \
                VALUES(?1, ?2, ?3) \
                ON CONFLICT(workspace_id, user) DO UPDATE SET read_write = ?3",
                (workspace_id, other_user, i64::from(read_write)),
            )
            .context("could not record the share in the database")?;
        println!(
            "Shared workspace at {} with {} ({})",
            mountpoint.display(),
            other_user,
            if read_write { "read-write" } else { "read-only" }
        );
    }

    transaction
        .commit()
        .context("could not save the share in the database")?;
    Ok(())
}

/// Fetch the shares for a workspace as a comma-separated string, e.g. `bob:ro, alice:rw`.
/// Returns an empty string if there are no shares.
pub fn shares_string(conn: &Connection, workspace_id: i64) -> String {
    let mut statement = match conn.prepare(
        "SELECT user, read_write FROM shares WHERE workspace_id = ?1 ORDER BY user",
    ) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let rows = statement
        .query_map([workspace_id], |row| {
            Ok(format!(
                "{}:{}",
                row.get::<_, String>(0)?,
                if row.get::<_, i64>(1)? == 1 { "rw" } else { "ro" }
            ))
        })
        .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>());
    rows.unwrap_or_default().join(", ")
}

/// Run `setfacl` with the given arguments, returning an error on failure.
fn run_setfacl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = process::Command::new("setfacl")
        .args(args)
        .status()
        .map_err(|e| {
            format!(
                "failed to run `setfacl` (is the `acl` package installed? \
                 and is `acltype=posixacl` set on the dataset?): {}",
                e
            )
        })?;
    if !status.success() {
        return Err(format!("setfacl {:?} failed with status {}", args, status).into());
    }
    Ok(())
}

/// Run `find <root> -type d -exec setfacl ... {} +` to apply default ACLs to
/// every directory beneath the workspace (so new files inherit access).
fn run_find_setfacl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = process::Command::new("find")
        .args(args)
        .status()
        .map_err(|e| format!("failed to run `find`: {}", e))?;
    if !status.success() {
        return Err(format!("find/setfacl pass failed with status {}", status).into());
    }
    Ok(())
}
