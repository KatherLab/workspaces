use std::{collections::HashMap, path::PathBuf};

use chrono::{DateTime, Duration, Utc};
use prettytable::{
    color,
    format::{Alignment, FormatBuilder},
    Attr, Cell, Row, Table,
};
use rusqlite::Connection;

use crate::{cli, config, to_volume_string, zfs};

#[derive(Debug)]
struct WorkspacesRow {
    filesystem_name: String,
    user: String,
    name: String,
    expiration_time: DateTime<Utc>,
}

pub fn list(
    conn: &Connection,
    filesystems: &HashMap<String, config::Filesystem>,
    filter_users: &Option<Vec<String>>,
    filter_filesystems: &Option<Vec<String>>,
    output: &Option<Vec<cli::WorkspacesColumns>>,
) {
    use cli::WorkspacesColumns;
    // the default columns
    let output = output.clone().unwrap_or(vec![
        WorkspacesColumns::Name,
        WorkspacesColumns::User,
        WorkspacesColumns::Fs,
        WorkspacesColumns::Size,
        WorkspacesColumns::Expiry,
        WorkspacesColumns::Mountpoint,
    ]);

    let mut table = Table::new();
    table.set_format(FormatBuilder::new().padding(0, 2).build());

    // bold title row
    table.set_titles(Row::new(
        output
            .iter()
            .map(|h| Cell::new(&h.to_string()).with_style(Attr::Bold))
            .collect(),
    ));

    let mut statement = conn
        .prepare("SELECT filesystem, user, name, expiration_time FROM workspaces")
        .unwrap();
    let workspace_iter = statement
        .query_map([], |row| {
            Ok(WorkspacesRow {
                filesystem_name: row.get(0)?,
                user: row.get(1)?,
                name: row.get(2)?,
                expiration_time: row.get(3)?,
            })
        })
        .unwrap();

    for workspace in workspace_iter {
        let workspace = workspace.unwrap();
        if !filter_users
            .as_ref()
            .map_or(true, |us| us.contains(&workspace.user))
            || !filter_filesystems
                .as_ref()
                .map_or(true, |fs| fs.contains(&workspace.filesystem_name))
        {
            continue;
        }
        let volume = to_volume_string(
            &filesystems
                .get(&workspace.filesystem_name)
                .expect("found workspace in database without corresponding config entry")
                .root,
            &workspace.user,
            &workspace.name,
        );
        let referenced = zfs::get_property::<usize>(&volume, "referenced");
        let mountpoint = zfs::get_property::<PathBuf>(&volume, "mountpoint");
        if mountpoint.is_err() || referenced.is_err() {
            eprintln!("Failed to get info for {}", volume);
            continue;
        }
        table.add_row(Row::new(
            output
                .iter()
                .map(|column| match column {
                    WorkspacesColumns::Name => Cell::new(&workspace.name),
                    WorkspacesColumns::User => Cell::new(&workspace.user),
                    WorkspacesColumns::Fs => Cell::new(&workspace.filesystem_name),
                    WorkspacesColumns::Expiry => {
                        if Utc::now()
                            > workspace.expiration_time
                                + filesystems[&workspace.filesystem_name].expired_retention
                        {
                            Cell::new("deleted soon")
                                .with_style(Attr::Bold)
                                .with_style(Attr::ForegroundColor(color::RED))
                        } else if Utc::now() > workspace.expiration_time {
                            Cell::new_align(
                                &format!(
                                    "deleted in {:>2}d",
                                    (workspace.expiration_time
                                        + filesystems[&workspace.filesystem_name]
                                            .expired_retention
                                        - Utc::now())
                                    .num_days()
                                ),
                                Alignment::RIGHT,
                            )
                            .with_style(Attr::Bold)
                            .with_style(Attr::ForegroundColor(color::RED))
                        } else if workspace.expiration_time - Utc::now() < Duration::days(30) {
                            Cell::new_align(
                                &format!(
                                    "expires in {:>2}d",
                                    (workspace.expiration_time - Utc::now()).num_days()
                                ),
                                Alignment::RIGHT,
                            )
                            .with_style(Attr::ForegroundColor(color::YELLOW))
                        } else {
                            Cell::new_align(
                                &format!(
                                    "expires in {:>2}d",
                                    (workspace.expiration_time - Utc::now()).num_days()
                                ),
                                Alignment::RIGHT,
                            )
                        }
                    }
                    WorkspacesColumns::Size => Cell::new_align(
                        &format!("{}G", referenced.as_ref().unwrap() / (1 << 30)),
                        Alignment::RIGHT,
                    ),
                    WorkspacesColumns::Mountpoint => {
                        Cell::new(mountpoint.as_ref().unwrap().to_str().unwrap())
                    }
                })
                .collect(),
        ));
    }

    table.printstd();
}
