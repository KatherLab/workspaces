use chrono::Duration;
use lettre::message::Mailbox;
use serde::de::{self, Unexpected};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::PathBuf;

/// Path of the configuration file
pub const CONFIG_PATH: &str = "/etc/workspaces/workspaces.toml";

/// TLS mode for SMTP
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Submission with STARTTLS (typically port 587)
    Starttls,
    /// Implicit/wrapper TLS (typically port 465)
    Wrapper,
}

/// Where to look up the email address a notification goes to
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecipientSource {
    /// `~USER/.config/workspaces.toml`, as workspaces has always done
    #[default]
    File,
    /// The directory (FreeIPA, LDAP, AD), through SSSD
    Sssd,
    /// The directory, falling back to the user's own configuration file
    SssdThenFile,
}

/// Preferred SMTP auth mechanism (optional). If unset, we auto-negotiate.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Plain,
    Login,
}

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
    // The >=v0.3 default location. If such a file exist, we are going to take this one
    let path = PathBuf::from("/usr/local/lib/workspaces/workspaces.db");
    if path.exists() {
        return path;
    }
    // v0.2 database location. We'll take this one if it exists
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
    ///
    /// `expiry_notifications` is accepted as well, since that is the name the
    /// example configuration file used to suggest.
    #[serde(
        default = "Vec::new",
        alias = "expiry_notifications",
        deserialize_with = "from_days_list"
    )]
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
    /// Optional visible From address. If omitted, falls back to `username`
    /// (which must parse as an email address).
    #[serde(default, deserialize_with = "deserialize_opt_mailbox")]
    pub from: Option<Mailbox>,
    /// Optional TLS mode ("starttls" or "wrapper"). Default: "starttls".
    #[serde(default)]
    pub tls: Option<TlsMode>,
    /// Optional auth mechanism override ("plain" or "login"); default: auto-negotiate.
    #[serde(default)]
    pub auth: Option<AuthMethod>,

    /// Where to look up recipients' email addresses: "file" (the default),
    /// "sssd", or "sssd-then-file".
    #[serde(default)]
    pub recipient_source: RecipientSource,

    /// Whether to skip users whose directory account is disabled
    /// (`nsAccountLock = TRUE`). Only the directory knows about this, so this
    /// has no effect with `recipient_source = "file"`.
    #[serde(default)]
    pub skip_inactive_users: bool,

    /// Mail domains which stand in for a missing address, e.g.
    /// `["katherlab.internal"]`. An address in one of them counts as no
    /// address at all. Case is irrelevant.
    #[serde(default)]
    pub placeholder_mail_domains: Vec<String>,

    /// Where to send the summary of the notifications a `workspaces maintain`
    /// run could not deliver. No summary is sent if this is unset.
    #[serde(default, deserialize_with = "deserialize_opt_mailbox")]
    pub admin_report_to: Option<Mailbox>,
}

impl SmtpConfig {
    /// Whether `address` is a placeholder rather than a real address, i.e.
    /// whether its domain is one of `placeholder_mail_domains`.
    pub fn is_placeholder_address(&self, address: &str) -> bool {
        let Some((_, domain)) = address.rsplit_once('@') else {
            return false;
        };
        self.placeholder_mail_domains.iter().any(|placeholder| {
            // Be forgiving about how the domains are written down
            let placeholder = placeholder.trim().trim_start_matches('@');
            !placeholder.is_empty() && domain.trim().eq_ignore_ascii_case(placeholder)
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn smtp_config(placeholder_mail_domains: &[&str]) -> SmtpConfig {
        SmtpConfig {
            relay: "smtp.example.org".to_string(),
            username: "workspaces".to_string(),
            password: "supersecret".to_string(),
            from: None,
            tls: None,
            auth: None,
            recipient_source: RecipientSource::default(),
            skip_inactive_users: false,
            placeholder_mail_domains: placeholder_mail_domains
                .iter()
                .map(|domain| domain.to_string())
                .collect(),
            admin_report_to: None,
        }
    }

    #[test]
    fn the_notification_schedule_is_read_under_either_name() {
        for key in ["expiry_notifications_on_days", "expiry_notifications"] {
            let filesystem: Filesystem = toml::from_str(&format!(
                "root = \"bulk/ws\"\n\
                 max_duration = 90\n\
                 expired_retention = 30\n\
                 {key} = [-29, 0, 14]\n"
            ))
            .unwrap();

            assert_eq!(
                filesystem.expiry_notifications_on_days,
                [Duration::days(-29), Duration::days(0), Duration::days(14)],
                "`{}` should configure the notification schedule",
                key
            );
        }
    }

    #[test]
    fn placeholder_domains_are_recognized_whatever_their_case() {
        let config = smtp_config(&["katherlab.internal"]);

        assert!(config.is_placeholder_address("alice@katherlab.internal"));
        assert!(config.is_placeholder_address("Alice@KatherLab.Internal"));
        assert!(!config.is_placeholder_address("alice@tu-dresden.de"));
        // Only the domain counts, not a lookalike local part
        assert!(!config.is_placeholder_address("katherlab.internal@tu-dresden.de"));
        // … and not a sub- or superdomain of it
        assert!(!config.is_placeholder_address("alice@mail.katherlab.internal"));
        assert!(!config.is_placeholder_address("alice@katherlab.internal.de"));
    }

    #[test]
    fn without_placeholder_domains_every_address_counts() {
        let config = smtp_config(&[]);

        assert!(!config.is_placeholder_address("alice@katherlab.internal"));
        assert!(!config.is_placeholder_address("alice@tu-dresden.de"));
    }

    #[test]
    fn placeholder_domains_may_be_written_with_an_at_sign() {
        let config = smtp_config(&["@katherlab.internal", " ", "example.invalid"]);

        assert!(config.is_placeholder_address("alice@katherlab.internal"));
        assert!(config.is_placeholder_address("alice@example.invalid"));
        // An empty entry must not swallow addresses without a domain
        assert!(!config.is_placeholder_address("alice"));
        assert!(!config.is_placeholder_address("alice@"));
    }

    #[test]
    fn the_recipient_source_defaults_to_the_users_own_file() {
        let config: SmtpConfig = toml::from_str(
            "relay = \"smtp.example.org\"\n\
             username = \"workspaces\"\n\
             password = \"supersecret\"\n",
        )
        .unwrap();

        assert_eq!(config.recipient_source, RecipientSource::File);
        assert!(!config.skip_inactive_users);
        assert!(config.placeholder_mail_domains.is_empty());
        assert!(config.admin_report_to.is_none());
    }

    #[test]
    fn the_recipient_source_is_read_from_the_configuration() {
        let config: SmtpConfig = toml::from_str(
            "relay = \"smtp.example.org\"\n\
             username = \"workspaces\"\n\
             password = \"supersecret\"\n\
             recipient_source = \"sssd-then-file\"\n\
             skip_inactive_users = true\n\
             placeholder_mail_domains = [\"katherlab.internal\"]\n\
             admin_report_to = \"it@example.org\"\n",
        )
        .unwrap();

        assert_eq!(config.recipient_source, RecipientSource::SssdThenFile);
        assert!(config.skip_inactive_users);
        assert_eq!(config.placeholder_mail_domains, ["katherlab.internal"]);
        assert_eq!(
            config.admin_report_to.map(|mailbox| mailbox.to_string()),
            Some("it@example.org".to_string())
        );
    }
}
