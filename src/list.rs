use std::{collections::HashMap, error::Error, path::PathBuf};

use chrono::{DateTime, Duration, Utc};
use prettytable::{
    color,
    format::{Alignment, FormatBuilder},
    Attr, Cell, Row, Table,
};
use rusqlite::Connection;

use crate::{cli, config, format, share, to_volume_string, zfs};

#[derive(Debug)]
struct WorkspacesRow {
    id: i64,
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
    format: &cli::OutputFormat,
) -> Result<(), Box<dyn Error>> {
    use cli::WorkspacesColumns;
    // the default columns
    let output = output.clone().unwrap_or(vec![
        WorkspacesColumns::Name,
        WorkspacesColumns::User,
        WorkspacesColumns::Fs,
        WorkspacesColumns::Size,
        WorkspacesColumns::Expiry,
        WorkspacesColumns::Mountpoint,
        WorkspacesColumns::Shares,
    ]);

    let mut statement =
        conn.prepare("SELECT id, filesystem, user, name, expiration_time FROM workspaces")?;
    let workspace_iter = statement.query_map([], |row| {
        Ok(WorkspacesRow {
            id: row.get(0)?,
            filesystem_name: row.get(1)?,
            user: row.get(2)?,
            name: row.get(3)?,
            expiration_time: row.get(4)?,
        })
    })?;

    // Collect all rows first: we need a stable order for CSV/JSON, and the
    // prettytable path renders directly. Collecting once keeps them in sync.
    let mut rows: Vec<WorkspacesRow> = workspace_iter.filter_map(Result::ok).collect();
    rows.sort_by(|a, b| {
        a.filesystem_name
            .cmp(&b.filesystem_name)
            .then(a.user.cmp(&b.user))
            .then(a.name.cmp(&b.name))
    });

    // Machine-readable path: emit raw values (bytes, ISO timestamps, etc.)
    if !matches!(format, cli::OutputFormat::Table) {
        let mut records = Vec::with_capacity(rows.len());
        for workspace in &rows {
            if !filter_users
                .as_ref()
                .map_or(true, |us| us.contains(&workspace.user))
                || !filter_filesystems
                    .as_ref()
                    .map_or(true, |fs| fs.contains(&workspace.filesystem_name))
            {
                continue;
            }
            let Some(fs) = filesystems.get(&workspace.filesystem_name) else {
                eprintln!(
                    "Skipping workspace {}: its filesystem {} is not configured",
                    workspace.name, workspace.filesystem_name
                );
                continue;
            };
            let volume = to_volume_string(&fs.root, &workspace.user, &workspace.name);
            let referenced = zfs::get_property::<usize>(&volume, "referenced").ok();
            let mountpoint = zfs::get_property::<PathBuf>(&volume, "mountpoint").ok();

            let mut record = Vec::new();
            for column in &output {
                let field = match column {
                    WorkspacesColumns::Name => {
                        format::Field::new("name", &workspace.name)
                    }
                    WorkspacesColumns::User => format::Field::new("user", &workspace.user),
                    WorkspacesColumns::Fs => format::Field::new("fs", &workspace.filesystem_name),
                    WorkspacesColumns::Size => format::Field::new(
                        "size_bytes",
                        referenced.unwrap_or(0),
                    ),
                    WorkspacesColumns::Expiry => {
                        format::Field::new("expiry", workspace.expiration_time.to_rfc3339())
                    }
                    WorkspacesColumns::Mountpoint => format::Field::new(
                        "mountpoint",
                        mountpoint
                            .as_ref()
                            .map(|m| m.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    ),
                    WorkspacesColumns::Shares => {
                        format::Field::new("shares", share::shares_string(conn, workspace.id))
                    }
                };
                record.push(field);
            }
            records.push(record);
        }
        format::write_records(format, &records)?;
        return Ok(());
    }

    // Human-readable table path
    let mut table = Table::new();
    table.set_format(FormatBuilder::new().padding(0, 2).build());

    // bold title row
    table.set_titles(Row::new(
        output
            .iter()
            .map(|h| Cell::new(&h.to_string()).with_style(Attr::Bold))
            .collect(),
    ));

    for workspace in &rows {
        if !filter_users
            .as_ref()
            .map_or(true, |us| us.contains(&workspace.user))
            || !filter_filesystems
                .as_ref()
                .map_or(true, |fs| fs.contains(&workspace.filesystem_name))
        {
            continue;
        }
        let Some(filesystem) = filesystems.get(&workspace.filesystem_name) else {
            eprintln!(
                "Skipping workspace {}: its filesystem {} is not configured",
                workspace.name, workspace.filesystem_name
            );
            continue;
        };
        let volume = to_volume_string(&filesystem.root, &workspace.user, &workspace.name);
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
                        if Utc::now() > workspace.expiration_time + filesystem.expired_retention {
                            Cell::new("deleted soon")
                                .with_style(Attr::Bold)
                                .with_style(Attr::ForegroundColor(color::RED))
                        } else if Utc::now() > workspace.expiration_time {
                            Cell::new_align(
                                &format!(
                                    "deleted in {:>2}d",
                                    (workspace.expiration_time + filesystem.expired_retention
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
                    WorkspacesColumns::Shares => {
                        Cell::new(&share::shares_string(conn, workspace.id))
                    }
                })
                .collect(),
        ));
    }

    table.printstd();
    Ok(())
}
