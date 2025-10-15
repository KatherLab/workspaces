use chrono::Duration;
use lettre::message::Mailbox;
use serde::de::{self, Unexpected};
use serde::de::{self, Unexpected};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::PathBuf;

/// Path of the configuration file
pub const CONFIG_PATH: &str = "/etc/workspaces/workspaces.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Workspaces database location
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,

    #[serde(default)]
    pub smtp: Option<SmtpConfig>,

    /// Default filesystem to use in CLI
    pub default_filesystem: Option<String>,
    /// Workspace filesystem definitions
    #[serde(default)]
    pub filesystems: HashMap<String, Filesystem>,
}

fn default_db_path() -> PathBuf {
    // The >=v0.3 default location.  If such a file exist, we are going to take this one
    let path = PathBuf::from("/usr/local/lib/workspaces/workspaces.db");
    if path.exists() {
        return path;
    }
    // v0.2 database location.  We'll take this one if it exists
    let path = PathBuf::from("/usr/local/share/workspaces/workspaces.db");
    if path.exists() {
        eprintln!(
            "DEPRECATION WARNING: the workspaces default database location has been moved from \
            `/usr/local/share/workspaces/workspaces.db` \
            to `/usr/local/lib/workspaces/workspaces.db`.  \
            Please either move your database to the new location, or manually specify it in `{}`",
            CONFIG_PATH
        );
        return path;
    }

    PathBuf::from("/usr/local/lib/workspaces/workspaces.db")
}

/// A filesystem workspaces can be created in
#[derive(Debug, Deserialize)]
pub struct Filesystem {
    /// ZFS filesystem / volume which will act as the root for the datasets
    pub root: String,

    /// Maximum number of days a workspace may exist
    #[serde(deserialize_with = "from_days")]
    pub max_duration: Duration,
    /// Days after which an expired dataset will be removed
    #[serde(deserialize_with = "from_days")]
    pub expired_retention: Duration,

    /// Days relative to the expiration time the user will be notified.
    /// Negative durations will lead to messages being sent after expiry,
    /// but before deletion.
    #[serde(default = "Vec::new", deserialize_with = "from_days_list")]
    pub expiry_notifications_on_days: Vec<Duration>,

    /// Snapshot
    #[serde(default)]
    pub snapshot: bool,

    /// Whether datasets can be created / extended
    #[serde(default)]
    pub disabled: bool,
}

fn from_days<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let days: i64 = Deserialize::deserialize(deserializer)?;
    Ok(Duration::days(days))
}

fn from_days_list<'de, D>(deserializer: D) -> Result<Vec<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut days: Vec<i64> = Deserialize::deserialize(deserializer)?;
    days.sort();
    Ok(days.iter().map(|days| Duration::days(*days)).collect())
}

#[derive(Deserialize, Debug)]
pub struct SmtpConfig {
    pub relay: String,
    pub username: String,
    pub password: String,
    /// Optional "From" address to use in notification emails.
    /// If omitted, we'll fall back to using `username` (if it parses as an email).
    #[serde(default, deserialize_with = "deserialize_opt_mailbox")]
    pub from: Option<Mailbox>,
}

#[derive(Debug, Deserialize)]
pub struct UserConfig {
    #[serde(deserialize_with = "deserialize_mailbox")]
    pub email: Mailbox,
}

fn deserialize_mailbox<'de, D>(deserializer: D) -> Result<Mailbox, D::Error>
where
    D: Deserializer<'de>,
{
    let email_str: String = Deserialize::deserialize(deserializer)?;
    email_str.parse().map_err(|_| {
        de::Error::invalid_value(Unexpected::Str(&email_str), &"a valid email address string")
    })
}

fn deserialize_opt_mailbox<'de, D>(deserializer: D) -> Result<Option<Mailbox>, D::Error>
where
    D: Deserializer<'de>,
{
    let email_opt: Option<String> = Option::deserialize(deserializer)?;
    match email_opt {
        Some(s) => s.parse().map(Some).map_err(|_| {
            de::Error::invalid_value(Unexpected::Str(&s), &"a valid email address string")
        }),
        None => Ok(None),
    }
}
