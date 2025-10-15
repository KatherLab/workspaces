use chrono::Utc;
use std::{
    io,
    process::{self, Command},
    str::FromStr,
};

#[derive(Debug)]
#[allow(unused)]
pub enum Error {
    /// An error occurring while running a command
    Command(io::Error),
    /// The ZFS invocation completed, but returned a non-zero code
    ZfsStatus(process::ExitStatus),
    /// Error while parsing ZFS's output
    PropertyParse(Box<dyn std::error::Error>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Command(err) => {
                write!(f, "Command error: {}", err)
            }
            Error::ZfsStatus(err) => {
                write!(f, "ZFS status error: {}", err)
            }
            Error::PropertyParse(err) => {
                write!(f, "ZFS property parsing error: {}", err)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Command(err) => err.source(),
            Error::PropertyParse(err) => err.source(),
            Error::ZfsStatus(..) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error::Command(value)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Creates a new ZFS volume
pub fn create(volume: &str) -> Result<()> {
    let status = Command::new("zfs")
        .args(["create", "-p", volume])
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::ZfsStatus(status)),
    }
}

/// Destroys a ZFS volume
pub fn destroy(volume: &str) -> Result<()> {
    let status = Command::new("zfs")
        .args(["destroy", "-r", volume])
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::ZfsStatus(status)),
    }
}

/// Renames a ZFS volume
pub fn rename(src_volume: &str, dest_volume: &str) -> Result<()> {
    let status = Command::new("zfs")
        .args(["rename", src_volume, dest_volume])
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::ZfsStatus(status)),
    }
}

/// Retrieves a ZFS property
pub fn get_property<F: FromStr>(volume: &str, property: &str) -> Result<F>
where
    <F as FromStr>::Err: std::error::Error + 'static,
{
    let output = Command::new("zfs")
        .args([
            "get", "-Hp", // make zfs output easily parsable
            "-o", "value", // output only desired value
            property, volume,
        ])
        .output()?;
    if !output.status.success() {
        return Err(Error::ZfsStatus(output.status));
    }
    let mut info_line = String::from_utf8(output.stdout).unwrap();
    info_line.pop(); // remove trailing newline
    info_line
        .parse()
        .map_err(|e| Error::PropertyParse(Box::new(e)))
}

/// Sets a ZFS property
pub fn set_property(volume: &str, property: &str, value: &str) -> Result<()> {
    let status: process::ExitStatus = Command::new("zfs")
        .args(["set", &format!("{}={}", property, value), volume])
        .status()?;

    match status.success() {
        true => Ok(()),
        false => Err(Error::ZfsStatus(status)),
    }
}

/// Recursively snapshot a volume
pub fn snapshot(volume: &str) -> Result<()> {
    let status = Command::new("zfs")
        .args([
            "snapshot",
            "-r",
            &format!(
                "{}@{}",
                volume,
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        ])
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(Error::ZfsStatus(status)),
    }
}
