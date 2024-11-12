use chrono::Utc;
use clap::Parser;
use create::create;
use db_schema::{NEWEST_DB_VERSION, UPDATE_DB};
use expire::expire;
use extend::extend;
use filesystems::filesystems;
use list::list;
use maintain::maintain;
use rename::rename;
use rusqlite::{backup, Connection};
use std::{
    collections::HashMap, fs, os::unix::fs::MetadataExt, path::Path, process, time::Duration,
};

mod cli;
mod config;
mod create;
mod db_schema;
mod expire;
mod extend;
mod filesystems;
mod list;
mod maintain;
mod rename;
mod zfs;

enum ExitCodes {
    /// The user tried executing an action they have no rights to do,
    /// i.e. expiring another user's workspace
    InsufficientPrivileges = 1,
    /// The user tried creating / extending a workspace on a disabled filesystem
    FsDisabled,
    /// The user tried creating / extending a workspace with too long a duration
    TooHighDuration,
    /// The workspace specified by a user does not exist
    UnknownWorkspace,
    /// The user tried to create a workspace that already exists
    WorkspaceExists,
    /// No filesystem given and no default specified in configuration file
    NoFilesystemSpecified,
}

fn to_volume_string(root: &str, user: &str, name: &str) -> String {
    format!("{}/{}/{}", root, user, name)
}

fn main() {
    // Read config
    let config_file =
        fs::File::open(config::CONFIG_PATH).expect("could not find configuration file");
    if (config_file.metadata().unwrap().mode() & 0o077) != 0 {
        panic!("config file permissions too liberal: should be 600");
    }
    let toml_str =
        fs::read_to_string(config::CONFIG_PATH).expect("could not find configuration file");
    let config: config::Config =
        toml::from_str(&toml_str).expect("error parsing configuration file");

    let args = cli::Args::parse();

    let mut conn = Connection::open(&config.db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();

    update_database_schema_if_necessary(&mut conn);

    match args.command {
        cli::Command::Create {
            filesystem_name,
            workspace_name: name,
            duration,
            user,
        } => {
            let filesystem_name = filesystem_or_default_or_exit(
                &filesystem_name,
                &config.filesystems,
                &config.default_filesystem,
            );
            create(
                &mut conn,
                &filesystem_name,
                &config.filesystems[&filesystem_name],
                &user,
                &name,
                &duration,
            )
        }
        cli::Command::List {
            filter_users,
            filter_filesystems,
            output,
        } => list(
            &conn,
            &config.filesystems,
            &filter_users,
            &filter_filesystems,
            &output,
        ),
        cli::Command::Rename {
            src_workspace_name,
            dest_workspace_name,
            user,
            filesystem_name,
        } => {
            let filesystem_name = filesystem_or_default_or_exit(
                &filesystem_name,
                &config.filesystems,
                &config.default_filesystem,
            );
            rename(
                &mut conn,
                &filesystem_name,
                &config.filesystems[&filesystem_name],
                &user,
                &src_workspace_name,
                &dest_workspace_name,
            )
        }
        cli::Command::Extend {
            filesystem_name,
            name,
            user,
            duration,
        } => {
            let filesystem_name = filesystem_or_default_or_exit(
                &filesystem_name,
                &config.filesystems,
                &config.default_filesystem,
            );
            extend(
                &mut conn,
                &filesystem_name,
                &config.filesystems[&filesystem_name],
                &user,
                &name,
                &duration,
            )
        }
        cli::Command::Expire {
            filesystem_name,
            name,
            user,
            delete_on_next_clean,
        } => {
            let filesystem_name = filesystem_or_default_or_exit(
                &filesystem_name,
                &config.filesystems,
                &config.default_filesystem,
            );
            expire(
                &mut conn,
                &filesystem_name,
                &config.filesystems[&filesystem_name],
                &user,
                &name,
                delete_on_next_clean,
            )
        }
        cli::Command::Filesystems { output } => filesystems(&config.filesystems, output),
        cli::Command::Maintain => maintain(&mut conn, &config.filesystems, &config.smtp),
    }
}

/// Horrible stateful filesystem name validation function
///
/// Returns with this order of preference:
/// - the given filesystem name if it exists
/// - the default filesystem, if specified in the config
/// - the only filesystem if there is only one
///
/// Otherwise, it terminates the program
fn filesystem_or_default_or_exit(
    filesystem_name: &Option<String>,
    filesystems: &HashMap<String, config::Filesystem>,
    default: &Option<String>,
) -> String {
    let filesystem_name: String = if let Some(name) = filesystem_name {
        name.clone()
    } else if let Some(name) = default {
        name.clone()
    } else if filesystems.len() == 1 {
        filesystems.keys().next().unwrap().clone()
    } else {
        eprintln!("Please specify a filesystem with `-f <FILESYSTEM>`");
        process::exit(ExitCodes::NoFilesystemSpecified as i32);
    };

    if filesystems.contains_key(&filesystem_name) {
        filesystem_name
    } else {
        eprint!("Invalid filesystem name. Please use one of the following:");
        for name in filesystems.keys() {
            eprint!(" {}", name);
        }
        eprintln!();
        process::exit(ExitCodes::UnknownWorkspace as i32);
    }
}

fn update_database_schema_if_necessary(source_db_conn: &mut Connection) {
    let db_path = Path::new(
        source_db_conn
            .path()
            .expect("database should be file backed"),
    );

    let db_version: usize = source_db_conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();

    assert!(
        db_version <= NEWEST_DB_VERSION,
        "database seems to be from a more current version of workspaces"
    );

    if db_version == NEWEST_DB_VERSION {
        return;
    }

    // Back up current database in case we need it for roll-backs later
    let backup_path = db_path.with_file_name(format!(
        "{}-{}.db.bak",
        db_path.file_stem().unwrap().to_string_lossy(),
        Utc::now().format("%Y%m%dT%H%M%S")
    ));

    let mut backup_dest_db = Connection::open(backup_path).unwrap();
    backup::Backup::new(source_db_conn, &mut backup_dest_db)
        .unwrap()
        .run_to_completion(4, Duration::from_millis(250), None)
        .unwrap();

    // Iteratively apply necessary database updates
    UPDATE_DB[db_version..]
        .iter()
        .for_each(|f| f(source_db_conn));
}
