use crate::{config, to_volume_string, zfs};
use chrono::{DateTime, Duration, Local, Utc};
use lettre::{
    address::AddressError,
    message::header::ContentType,
    message::Mailbox,
    transport::smtp::authentication::{Credentials, Mechanism},
    transport::smtp::client::{Tls, TlsParameters},
    Message, SmtpTransport, Transport,
};
use rusqlite::Connection;
use std::{collections::HashMap, error::Error, fmt, fs, io};
use users::{get_user_by_name, os::unix::UserExt};

pub fn maintain(
    conn: &mut Connection,
    filesystems: &HashMap<String, config::Filesystem>,
    smtp_config: &Option<config::SmtpConfig>,
) -> Result<(), Box<dyn Error>> {
    let transaction = conn.transaction()?;
    {
        let mut statement = transaction
            .prepare("SELECT id, filesystem, user, name, expiration_time FROM workspaces")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let workspace_id: i32 = row.get(0)?;
            let filesystem_name: String = row.get(1)?;
            let username: String = row.get(2)?;
            let workspace_name: String = row.get(3)?;
            let expiration_time: DateTime<Utc> = row.get(4)?;

            let filesystem = &filesystems
                .get(&filesystem_name)
                .expect("unknown filesystem name");

            if let Some(smtp_config) = smtp_config {
                match notify_if_necessary_(
                    workspace_id,
                    &workspace_name,
                    &username,
                    smtp_config,
                    filesystem,
                    expiration_time,
                    &transaction,
                ) {
                    user_error @ Err(
                        NotificationError::UserConfigReadError(..)
                            | NotificationError::UserConfigParseError(..)
                            | NotificationError::MailboxParseError(..),
                    ) => {
                        eprintln!("User error while notifying {}: {:?}", username, user_error);
                    }
                    res => {
                        res.expect("non-recoverable error during notification process");
                    }
                }
            }

            let volume = to_volume_string(&filesystem.root, &username, &workspace_name);

            if expiration_time < Local::now() - filesystem.expired_retention {
                // Delete workspaces expired beyond their retention date
                if zfs::destroy(&volume).is_err() {
                    continue;
                }
                transaction.execute(
                    "DELETE FROM workspaces
                            WHERE id = ?1",
                    [workspace_id],
                )?;
            } else if expiration_time < Local::now() {
                // Set recently expired workspaces to read-only
                zfs::set_property(&volume, "readonly", "on")?;
            }
        }
    }
    transaction.commit()?;

    // Snapshot all remaining filesystems for which this is desired
    for filesystem in filesystems.values() {
        if filesystem.snapshot {
            zfs::snapshot(&filesystem.root)?
        }
    }

    Ok(())
}

#[derive(Debug)]
#[allow(unused)]
enum NotificationError {
    UserNotFoundError(String),
    UserConfigReadError(io::Error),
    UserConfigParseError(toml::de::Error),
    SmtpError(lettre::transport::smtp::Error),
    MailboxParseError(AddressError),
    /// Failed to build TLS parameters for the given relay host
    TlsParametersInvalid(String),
}

impl std::error::Error for NotificationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UserNotFoundError(..) => None,
            Self::UserConfigReadError(err) => Some(err),
            Self::UserConfigParseError(err) => Some(err),
            Self::SmtpError(err) => Some(err),
            Self::MailboxParseError(err) => Some(err),
            Self::TlsParametersInvalid(..) => None,
        }
    }
}

impl std::fmt::Display for NotificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserNotFoundError(username) => write!(f, "User not found: {}", username),
            Self::UserConfigReadError(err) => write!(f, "User configuration read error: {}", err),
            Self::UserConfigParseError(err) => {
                write!(f, "User configuration parsing error: {}", err)
            }
            Self::SmtpError(err) => write!(f, "SMTP error: {}", err),
            Self::MailboxParseError(err) => write!(f, "Mailbox parse error: {}", err),
            Self::TlsParametersInvalid(host) => write!(
                f,
                "TLS parameters could not be constructed for relay host: {}",
                host
            ),
        }
    }
}

impl From<io::Error> for NotificationError {
    fn from(value: io::Error) -> Self {
        NotificationError::UserConfigReadError(value)
    }
}

impl From<toml::de::Error> for NotificationError {
    fn from(value: toml::de::Error) -> Self {
        NotificationError::UserConfigParseError(value)
    }
}

impl From<lettre::transport::smtp::Error> for NotificationError {
    fn from(value: lettre::transport::smtp::Error) -> Self {
        NotificationError::SmtpError(value)
    }
}

impl From<AddressError> for NotificationError {
    fn from(value: AddressError) -> Self {
        NotificationError::MailboxParseError(value)
    }
}

/// Parses "host", "host:port", or "[IPv6]:port" into (host, Some(port)) or (host, None)
fn split_host_port(input: &str) -> (&str, Option<u16>) {
    if let Some(rest) = input.strip_prefix('[') {
        if let Some(idx) = rest.find("]:") {
            let host = &rest[..idx];
            let port_str = &rest[idx + 2..];
            if let Ok(port) = port_str.parse::<u16>() {
                return (host, Some(port));
            }
            return (host, None);
        }
    }
    if let Some((host, port_str)) = input.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return (host, Some(port));
        }
    }
    (input, None)
}

fn notify_if_necessary_(
    workspace_id: i32,
    workspace_name: &str,
    username: &str,
    smtp_config: &config::SmtpConfig,
    filesystem: &config::Filesystem,
    expiration_time: DateTime<Utc>,
    connection: &Connection,
) -> Result<(), NotificationError> {
    // Get user config
    let user = get_user_by_name(username)
        .ok_or(NotificationError::UserNotFoundError(username.to_owned()))?;
    let user_config_path = user.home_dir().join(".config/workspaces.toml");
    let toml_str =
        fs::read_to_string(user_config_path).map_err(NotificationError::UserConfigReadError)?;
    let user_config: config::UserConfig =
        toml::from_str(&toml_str).map_err(NotificationError::UserConfigParseError)?;

    // Send out email notifications
    let creds = Credentials::new(
        smtp_config.username.to_owned(),
        smtp_config.password.to_owned(),
    );

    // Support relay as "host" or "host:port" (and "[IPv6]:port")
    let (relay_host, relay_port) = split_host_port(&smtp_config.relay);
    let mut builder = SmtpTransport::relay(relay_host).map_err(NotificationError::SmtpError)?;

    // TLS mode: default STARTTLS; if WRAPPER and no port given, default to 465
    let tls_mode = smtp_config.tls.unwrap_or(config::TlsMode::Starttls);
    match (tls_mode, relay_port) {
        (config::TlsMode::Wrapper, Some(p)) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(p).tls(Tls::Wrapper(params));
        }
        (config::TlsMode::Wrapper, None) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(465).tls(Tls::Wrapper(params));
        }
        (config::TlsMode::Starttls, Some(p)) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(p).tls(Tls::Required(params));
        }
        (config::TlsMode::Starttls, None) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.tls(Tls::Required(params));
        }
    }

    // Optional auth mechanism override
    if let Some(method) = smtp_config.auth {
        let mech = match method {
            config::AuthMethod::Plain => Mechanism::Plain,
            config::AuthMethod::Login => Mechanism::Login,
        };
        builder = builder.authentication(vec![mech]);
    }

    let mailer = builder.credentials(creds).build();

    let last_notification_time = connection
        .prepare(
            "SELECT timestamp \
                FROM notifications \
                WHERE workspace_id = ?1 \
                ORDER BY timestamp DESC \
                LIMIT 1",
        )
        .map_or(None, |mut res| {
            res.query_row((workspace_id,), |row| row.get::<_, DateTime<Utc>>(0))
                .ok()
        });

    let duration_since_last_notification = last_notification_time.map(|t| Utc::now() - t);
    let duration_until_expiry = expiration_time - Utc::now();
    // Find the most recent passed notification deadline ...
    if let Some(duration_from_expiry_when_notification_should_have_been_issued) = filesystem
        .expiry_notifications_on_days
        .iter()
        .filter(|d| d > &&duration_until_expiry)
        .next()
    {
        // ... and check if our last message is more recent ...
        if duration_since_last_notification.map_or(true, |d| {
            (Utc::now() - d)
                < (expiration_time
                    - *duration_from_expiry_when_notification_should_have_been_issued)
        }) {
            // if not, we have to notify the user!
            let from_mailbox: Mailbox = if let Some(mb) = smtp_config.from.clone() {
                mb
            } else {
                smtp_config
                    .username
                    .parse()
                    .map_err(NotificationError::MailboxParseError)?
            };

            let email = Message::builder()
                .from(from_mailbox)
                .to(user_config.email)
                .header(ContentType::TEXT_PLAIN);

            let subject = if duration_until_expiry > Duration::days(0) {
                format!(
                    "Your workspace {} on {} will expire in {} days.",
                    workspace_name,
                    hostname::get()?.to_string_lossy(),
                    duration_until_expiry.num_days()
                )
            } else {
                format!(
                    "Your workspace {} on {} will be deleted in {} days.",
                    workspace_name,
                    hostname::get()?.to_string_lossy(),
                    (filesystem.expired_retention + duration_until_expiry).num_days()
                )
            };

            let email = email
                .subject(&subject)
                .body(format!(
                    "{}

You can extend it by logging into {} and running
`workspaces extend -d <duration in days> {}`.

\
                    To disable notifications for this workspace, manually mark this workspace as expired by running
\
                    `workspaces expire {}`.",
                    &subject,
                    hostname::get()?.to_string_lossy(),
                    workspace_name,
                    workspace_name,
                ))
                .unwrap();

            mailer.send(&email).map_err(NotificationError::SmtpError)?;
            connection
                .execute(
                    "INSERT INTO notifications(workspace_id, timestamp) VALUES(?1, ?2)",
                    (workspace_id, Utc::now()),
                )
                .unwrap();
        }
    }
    Ok(())
}

/// Admin-only: send a one-off test email using SMTP config.
/// If `to_override` is Some, send to that address; otherwise look up the
/// target user's `~/.config/workspaces.toml` (UserConfig.email).
pub fn notify_test(
    target_username: &str,
    to_override: Option<String>,
    smtp_config: &config::SmtpConfig,
) -> Result<(), Box<dyn Error>> {
    // Resolve recipient
    let to_mailbox: Mailbox = if let Some(to) = to_override {
        to.parse().map_err(NotificationError::MailboxParseError)?
    } else {
        let user = get_user_by_name(target_username)
            .ok_or(NotificationError::UserNotFoundError(target_username.to_owned()))?;
        let user_config_path = user.home_dir().join(".config/workspaces.toml");
        let toml_str = fs::read_to_string(user_config_path)?;
        let user_config: config::UserConfig = toml::from_str(&toml_str)?;
        user_config.email
    };

    // Build SMTP transport
    let creds = Credentials::new(
        smtp_config.username.to_owned(),
        smtp_config.password.to_owned(),
    );
    let (relay_host, relay_port) = split_host_port(&smtp_config.relay);
    let mut builder = SmtpTransport::relay(relay_host)?;

    // TLS mode: mirror the logic above
    let tls_mode = smtp_config.tls.unwrap_or(config::TlsMode::Starttls);
    match (tls_mode, relay_port) {
        (config::TlsMode::Wrapper, Some(p)) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(p).tls(Tls::Wrapper(params));
        }
        (config::TlsMode::Wrapper, None) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(465).tls(Tls::Wrapper(params));
        }
        (config::TlsMode::Starttls, Some(p)) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.port(p).tls(Tls::Required(params));
        }
        (config::TlsMode::Starttls, None) => {
            let params = TlsParameters::new(relay_host.to_string())
                .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))?;
            builder = builder.tls(Tls::Required(params));
        }
    }

    if let Some(method) = smtp_config.auth {
        let mech = match method {
            config::AuthMethod::Plain => Mechanism::Plain,
            config::AuthMethod::Login => Mechanism::Login,
        };
        builder = builder.authentication(vec![mech]);
    }
    let mailer = builder.credentials(creds).build();

    // Determine From
    let from_mailbox: Mailbox = if let Some(mb) = smtp_config.from.clone() {
        mb
    } else {
        match smtp_config.username.parse() {
            Ok(mb) => mb,
            // Propagate the real parse error if username isn't an email
            Err(e) => return Err(Box::new(e)),
        }
    };

    let host = hostname::get()?.to_string_lossy().to_string();
    let subject = format!("Workspaces test email from {}", host);
    let body = format!(
        "Hello,\n\nThis is a test email sent by Workspaces on {}.\n\
If you can read this, SMTP is configured correctly.\n",
        host
    );

    let msg = Message::builder()
        .from(from_mailbox)
        .to(to_mailbox.clone())
        .header(ContentType::TEXT_PLAIN)
        .subject(subject)
        .body(body)?;

    mailer.send(&msg)?;
    println!("Sent test email to {}", to_mailbox);
    Ok(())
}
