use crate::{config, to_volume_string, zfs};
use chrono::{DateTime, Duration, Local, Utc};
use lettre::{
    address::AddressError, message::header::ContentType,
    transport::smtp::authentication::Credentials, Message, SmtpTransport, Transport,
};
use rusqlite::Connection;
use std::{collections::HashMap, fs, io};
use users::{get_user_by_name, os::unix::UserExt};

pub fn maintain(
    conn: &mut Connection,
    filesystems: &HashMap<String, config::Filesystem>,
    smtp_config: &Option<config::SmtpConfig>,
) {
    let transaction = conn.transaction().unwrap();
    {
        let mut statement = transaction
            .prepare("SELECT id, filesystem, user, name, expiration_time FROM workspaces")
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let workspace_id: i32 = row.get(0).unwrap();
            let filesystem_name: String = row.get(1).unwrap();
            let username: String = row.get(2).unwrap();
            let workspace_name: String = row.get(3).unwrap();
            let expiration_time: DateTime<Utc> = row.get(4).unwrap();

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
                transaction
                    .execute(
                        "DELETE FROM workspaces
                            WHERE id = ?1",
                        [workspace_id],
                    )
                    .unwrap();
            } else if expiration_time < Local::now() {
                // Set recently expired workspaces to read-only
                zfs::set_property(&volume, "readonly", "on").unwrap();
            }
        }
    }
    transaction.commit().unwrap();

    // Snapshot all remaining filesystems for which this is desired
    for filesystem in filesystems.values() {
        if filesystem.snapshot {
            zfs::snapshot(&filesystem.root).unwrap()
        }
    }
}

#[derive(Debug)]
#[allow(unused)]
enum NotificationError {
    UserNotFoundError(String),
    UserConfigReadError(io::Error),
    UserConfigParseError(toml::de::Error),
    SmtpError(lettre::transport::smtp::Error),
    MailboxParseError(AddressError),
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

    let mailer = SmtpTransport::relay(&smtp_config.relay)
        .map_err(NotificationError::SmtpError)?
        .credentials(creds)
        .build();

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
            let email = Message::builder()
                .from(
                    smtp_config
                        .username
                        .parse()
                        .map_err(NotificationError::MailboxParseError)?,
                )
                .to(user_config.email)
                .header(ContentType::TEXT_PLAIN);

            let subject = if duration_until_expiry > Duration::days(0) {
                format!(
                    "Your workspace {} on {} will expire in {} days.",
                    workspace_name,
                    hostname::get().unwrap().to_string_lossy(),
                    duration_until_expiry.num_days()
                )
            } else {
                format!(
                    "Your workspace {} on {} will be deleted in {} days.",
                    workspace_name,
                    hostname::get().unwrap().to_string_lossy(),
                    (filesystem.expired_retention + duration_until_expiry).num_days()
                )
            };

            let email = email.subject(&subject).body(
                format!(
                    "{}\n\nYou can extend it by logging into {} and running\n`workspaces extend -d <duration in days> {}`.",
                    &subject,
                    hostname::get().unwrap().to_string_lossy(),
                    workspace_name,
                )
            ).unwrap();

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
