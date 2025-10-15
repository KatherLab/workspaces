use std::{collections::HashMap, error::Error};

use prettytable::{
    color,
    format::{Alignment, FormatBuilder},
    Attr, Cell, Row, Table,
};

use crate::{
    cli::{self, FilesystemsColumns},
    config, zfs,
};

pub fn filesystems(
    filesystems: &HashMap<String, config::Filesystem>,
    output: Option<Vec<cli::FilesystemsColumns>>,
) -> Result<(), Box<dyn Error>> {
    // the default columns
    let output = output.unwrap_or(vec![
        FilesystemsColumns::Name,
        FilesystemsColumns::Used,
        FilesystemsColumns::Free,
        FilesystemsColumns::Total,
        FilesystemsColumns::Duration,
        FilesystemsColumns::Retention,
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

    for (name, info) in filesystems {
        let used = zfs::get_property::<usize>(&info.root, "used")?;
        let available = zfs::get_property::<usize>(&info.root, "available")?;
        let total = used + available;
        table.add_row(Row::new(
            output
                .iter()
                .map(|column| match column {
                    FilesystemsColumns::Name => Cell::new(name),
                    FilesystemsColumns::Used => {
                        Cell::new_align(&format!("{}G", used / (1 << 30)), Alignment::RIGHT)
                    }
                    FilesystemsColumns::Free => {
                        Cell::new_align(&format!("{}G", available / (1 << 30)), Alignment::RIGHT)
                    }
                    FilesystemsColumns::Total => {
                        Cell::new_align(&format!("{}G", total / (1 << 30)), Alignment::RIGHT)
                    }
                    FilesystemsColumns::Duration => match info.disabled {
                        true => Cell::new("disabled"),
                        false => {
                            Cell::new(&format!("{}d", info.max_duration.num_days())).style_spec("r")
                        }
                    },
                    FilesystemsColumns::Retention => {
                        Cell::new(&format!("{}d", info.expired_retention.num_days()))
                            .style_spec("r")
                    }
                })
                .map(|c| {
                    // color if almost full
                    if used as f64 > total as f64 * 0.9 {
                        c.with_style(Attr::ForegroundColor(color::RED))
                    } else if used as f64 > total as f64 * 0.75 {
                        c.with_style(Attr::ForegroundColor(color::YELLOW))
                    } else {
                        c
                    }
                })
                .map(|c| {
                    // dim if disabled
                    if info.disabled {
                        c.with_style(Attr::Dim)
                    } else {
                        c
                    }
                })
                .collect(),
        ));
    }

    table.printstd();

    Ok(())
}
