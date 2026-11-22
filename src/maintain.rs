use crate::{config, errors::Context, to_volume_string, userinfo, zfs};
use chrono::{DateTime, Duration, Local, Utc};
use lettre::{
    Message, SmtpTransport, Transport,
    address::AddressError,
    message::Mailbox,
    message::header::ContentType,
    transport::smtp::authentication::{Credentials, Mechanism},
    transport::smtp::client::{Tls, TlsParameters},
};
use rusqlite::Connection;
use std::{collections::HashMap, error::Error, fmt, fs, io};
use users::{get_user_by_name, os::unix::UserExt};

pub fn maintain(
    conn: &mut Connection,
    filesystems: &HashMap<String, config::Filesystem>,
    smtp_config: &Option<config::SmtpConfig>,
) -> Result<(), Box<dyn Error>> {
    // Problems with a single workspace shouldn't stop us from maintaining the
    // others, so they are counted and reported once we're done
    let mut failures: usize = 0;
    // Notifications nobody could be reached with, for the admin's report
    let mut skipped: Vec<SkippedNotification> = Vec::new();

    let transaction = conn
        .transaction()
        .context("could not start a database transaction")?;
    {
        let mut statement = transaction
            .prepare("SELECT id, filesystem, user, name, expiration_time FROM workspaces")
            .context("could not query the workspaces database")?;
        let mut rows = statement
            .query([])
            .context("could not query the workspaces database")?;
        while let Some(row) = rows
            .next()
            .context("could not read a workspace from the database")?
        {
            let workspace_id: i32 = row.get(0)?;
            let filesystem_name: String = row.get(1)?;
            let username: String = row.get(2)?;
            let workspace_name: String = row.get(3)?;
            let expiration_time: DateTime<Utc> = row.get(4)?;

            let Some(filesystem) = filesystems.get(&filesystem_name) else {
                eprintln!(
                    "Workspace {} of {} lies on filesystem {}, which is not configured; \
                     skipping it",
                    workspace_name, username, filesystem_name
                );
                failures += 1;
                continue;
            };

            if let Some(smtp_config) = smtp_config {
                let notification = notify_if_necessary_(
                    workspace_id,
                    &workspace_name,
                    &username,
                    smtp_config,
                    filesystem,
                    expiration_time,
                    &transaction,
                );
                match notification {
                    Ok(NotificationOutcome::Sent | NotificationOutcome::NotDue) => {}
                    Ok(NotificationOutcome::Skipped(reason)) => {
                        eprintln!(
                            "No expiry reminder was sent to {} about workspace {}: {}",
                            username, workspace_name, reason
                        );
                        // A directory we cannot reach is a problem of this
                        // host, not of the user
                        if matches!(reason, SkipReason::LookupFailed(..)) {
                            failures += 1;
                        }
                        // Before expiry the reminder is about the expiration
                        // date, afterwards about the deletion date
                        let (kind, deadline) = if expiration_time > Utc::now() {
                            (NotificationKind::ExpiryReminder, expiration_time)
                        } else {
                            (
                                NotificationKind::DeletionReminder,
                                expiration_time + filesystem.expired_retention,
                            )
                        };
                        skipped.push(SkippedNotification {
                            user: username.clone(),
                            reason,
                            workspace: workspace_name.clone(),
                            filesystem: filesystem_name.clone(),
                            kind,
                            deadline,
                        });
                    }
                    Err(error) => {
                        eprintln!(
                            "Could not notify {} about workspace {}: {}",
                            username, workspace_name, error
                        );
                        failures += 1;
                    }
                }
            }

            let volume = to_volume_string(&filesystem.root, &username, &workspace_name);

            if expiration_time < Local::now() - filesystem.expired_retention {
                // Delete workspaces expired beyond their retention date
                if let Err(error) = zfs::destroy(&volume) {
                    eprintln!("Could not delete the dataset {}: {}", volume, error);
                    failures += 1;
                    continue;
                }
                transaction
                    .execute(
                        "DELETE FROM workspaces
                            WHERE id = ?1",
                        [workspace_id],
                    )
                    .context("could not remove the deleted workspace from the database")?;
                if let Some(smtp) = smtp_config {
                    let host = hostname::get()
                        .context("could not determine this host's name")?
                        .to_string_lossy()
                        .to_string();
                    let subject =
                        format!("Your workspace {} on {} was deleted.", workspace_name, host);
                    let body = format!(
                        "Hello,\n\nYour workspace \"{}\" on {} has been permanently deleted.\nFilesystem: {}\nOwner: {}\n",
                        workspace_name, host, filesystem_name, username
                    );
                    // Best-effort: if email fails, don't abort the cleanup run
                    match notify_event(&username, smtp, subject, body) {
                        Ok(NotificationOutcome::Sent | NotificationOutcome::NotDue) => {}
                        Ok(NotificationOutcome::Skipped(reason)) => {
                            eprintln!(
                                "No deletion notice was sent to {} about workspace {}: {}",
                                username, workspace_name, reason
                            );
                            if matches!(reason, SkipReason::LookupFailed(..)) {
                                failures += 1;
                            }
                            skipped.push(SkippedNotification {
                                user: username.clone(),
                                reason,
                                workspace: workspace_name.clone(),
                                filesystem: filesystem_name.clone(),
                                kind: NotificationKind::Deleted,
                                deadline: Utc::now(),
                            });
                        }
                        Err(error) => {
                            eprintln!(
                                "Failed to send deletion notice for {}: {}",
                                workspace_name, error
                            );
                            failures += 1;
                        }
                    }
                }
            } else if expiration_time < Local::now() {
                // Set recently expired workspaces to read-only
                if let Err(error) = zfs::set_property(&volume, "readonly", "on") {
                    eprintln!("Could not make the dataset {} read-only: {}", volume, error);
                    failures += 1;
                }
            }
        }
    }
    transaction
        .commit()
        .context("could not save this maintenance run's changes to the database")?;

    // Snapshot all remaining filesystems for which this is desired
    for filesystem in filesystems.values() {
        if filesystem.snapshot {
            if let Err(error) = zfs::snapshot(&filesystem.root) {
                eprintln!("Could not snapshot {}: {}", filesystem.root, error);
                failures += 1;
            }
        }
    }

    // Tell the admin about everyone we could not reach, in a single mail
    if let Some(smtp_config) = smtp_config
        && let Err(error) = send_admin_report(&mut skipped, smtp_config)
    {
        eprintln!(
            "Could not report the {} skipped notifications: {}",
            skipped.len(),
            error
        );
        failures += 1;
    }

    if failures > 0 {
        return Err(format!(
            "{} maintenance {} failed; see the messages above",
            failures,
            if failures == 1 { "step" } else { "steps" }
        )
        .into());
    }

    Ok(())
}

#[derive(Debug)]
enum NotificationError {
    SmtpError(lettre::transport::smtp::Error),
    MailboxParseError(AddressError),
    /// Failed to build TLS parameters for the given relay host
    TlsParametersInvalid(String),
    /// The name of this host could not be determined
    HostnameError(io::Error),
    /// The notification could not be assembled into an email
    MessageBuildError(lettre::error::Error),
    /// The notification could not be recorded in the database
    DatabaseError(rusqlite::Error),
}

impl std::error::Error for NotificationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SmtpError(err) => Some(err),
            Self::MailboxParseError(err) => Some(err),
            Self::TlsParametersInvalid(..) => None,
            Self::HostnameError(err) => Some(err),
            Self::MessageBuildError(err) => Some(err),
            Self::DatabaseError(err) => Some(err),
        }
    }
}

impl std::fmt::Display for NotificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SmtpError(err) => write!(f, "SMTP error: {}", err),
            Self::MailboxParseError(err) => write!(f, "Mailbox parse error: {}", err),
            Self::TlsParametersInvalid(host) => write!(
                f,
                "TLS parameters could not be constructed for relay host: {}",
                host
            ),
            Self::HostnameError(err) => write!(f, "could not determine this host's name: {}", err),
            Self::MessageBuildError(err) => write!(f, "could not build the email: {}", err),
            Self::DatabaseError(err) => write!(f, "database error: {}", err),
        }
    }
}

impl From<io::Error> for NotificationError {
    fn from(value: io::Error) -> Self {
        NotificationError::HostnameError(value)
    }
}

impl From<lettre::error::Error> for NotificationError {
    fn from(value: lettre::error::Error) -> Self {
        NotificationError::MessageBuildError(value)
    }
}

impl From<rusqlite::Error> for NotificationError {
    fn from(value: rusqlite::Error) -> Self {
        NotificationError::DatabaseError(value)
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

/// Why a user could not be notified.
///
/// The order of the variants is the order the admin report groups them in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    /// The user is not known to this system (any more)
    UserNotFound,
    /// The user's account is disabled in the directory
    AccountLocked,
    /// No email address is known for the user
    NoAddress(String),
    /// The only address known for the user stands in for a real one
    PlaceholderAddress(String),
    /// The user's address could not be looked up at all
    LookupFailed(String),
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserNotFound => write!(f, "the user does not exist on this system (any more)"),
            Self::AccountLocked => write!(f, "the user's account is disabled in the directory"),
            Self::NoAddress(detail) => {
                write!(f, "no email address is known for the user ({})", detail)
            }
            Self::PlaceholderAddress(address) => write!(
                f,
                "only the placeholder address {} is known for the user",
                address
            ),
            Self::LookupFailed(detail) => write!(
                f,
                "the user's email address could not be looked up: {}",
                detail
            ),
        }
    }
}

/// Where a user's notifications go, or why they go nowhere.
#[derive(Debug)]
pub enum Recipient {
    Send(Mailbox),
    Skip(SkipReason),
}

/// What became of a notification.
#[derive(Debug)]
pub enum NotificationOutcome {
    /// The notification was sent
    Sent,
    /// Nobody could be notified, for this reason
    Skipped(SkipReason),
    /// Nothing had to be sent in the first place
    NotDue,
}

/// What a notification was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// A workspace is about to expire
    ExpiryReminder,
    /// A workspace has expired and is about to be deleted
    DeletionReminder,
    /// A workspace has been deleted
    Deleted,
}

impl NotificationKind {
    fn description(self) -> &'static str {
        match self {
            Self::ExpiryReminder => "expiry reminder",
            Self::DeletionReminder => "deletion reminder",
            Self::Deleted => "deletion notice",
        }
    }

    /// How to introduce the date the notification was about
    fn deadline_description(self) -> &'static str {
        match self {
            Self::ExpiryReminder => "expires",
            Self::DeletionReminder => "to be deleted",
            Self::Deleted => "deleted",
        }
    }
}

/// A notification a maintenance run could not deliver.
#[derive(Debug)]
struct SkippedNotification {
    user: String,
    reason: SkipReason,
    workspace: String,
    filesystem: String,
    kind: NotificationKind,
    /// When the workspace expires, is to be deleted, or was deleted
    deadline: DateTime<Utc>,
}

/// Find out where a user's notifications should be sent.
///
/// Depending on `recipient_source`, this is the address in the user's own
/// `~/.config/workspaces.toml`, the one the directory has for them, or the
/// former as a fallback for the latter.
pub fn resolve_recipient(username: &str, smtp_config: &config::SmtpConfig) -> Recipient {
    // Somebody who is not on this system any more cannot be notified -- and
    // has no home directory to read a configuration file from, either
    let Some(user) = get_user_by_name(username) else {
        return Recipient::Skip(SkipReason::UserNotFound);
    };

    recipient_for(&user, smtp_config)
}

/// [`resolve_recipient`] for a user we already looked up.
fn recipient_for(user: &users::User, smtp_config: &config::SmtpConfig) -> Recipient {
    let username = user.name().to_string_lossy();

    match smtp_config.recipient_source {
        config::RecipientSource::File => recipient_from_file(user, smtp_config),
        config::RecipientSource::Sssd => recipient_from_directory(&username, smtp_config),
        config::RecipientSource::SssdThenFile => {
            match recipient_from_directory(&username, smtp_config) {
                send @ Recipient::Send(..) => send,
                // A disabled account is not notified, whatever the user's own
                // configuration file may say
                locked @ Recipient::Skip(SkipReason::AccountLocked) => locked,
                Recipient::Skip(reason) => match recipient_from_file(user, smtp_config) {
                    send @ Recipient::Send(..) => send,
                    // A directory we could not even ask is more worth
                    // reporting than a file that holds nothing
                    from_file => match reason {
                        failed @ SkipReason::LookupFailed(..) => Recipient::Skip(failed),
                        _ => from_file,
                    },
                },
            }
        }
    }
}

/// The address from the user's own `~/.config/workspaces.toml`.
fn recipient_from_file(user: &users::User, smtp_config: &config::SmtpConfig) -> Recipient {
    let path = user.home_dir().join(".config/workspaces.toml");

    let toml_str = match fs::read_to_string(&path) {
        Ok(toml_str) => toml_str,
        Err(error) => {
            return Recipient::Skip(SkipReason::NoAddress(format!(
                "{}: {}",
                path.display(),
                error
            )));
        }
    };
    let user_config: config::UserConfig = match toml::from_str(&toml_str) {
        Ok(user_config) => user_config,
        Err(error) => {
            return Recipient::Skip(SkipReason::NoAddress(format!(
                "{}: {}",
                path.display(),
                error.message()
            )));
        }
    };

    let address = user_config.email.email.to_string();
    if smtp_config.is_placeholder_address(&address) {
        return Recipient::Skip(SkipReason::PlaceholderAddress(address));
    }

    Recipient::Send(user_config.email)
}

/// The address the directory has for the user, looked up through SSSD.
fn recipient_from_directory(username: &str, smtp_config: &config::SmtpConfig) -> Recipient {
    let attributes = match userinfo::get_user_attributes(username) {
        Ok(attributes) => attributes,
        Err(userinfo::Error::UserNotFound) => return Recipient::Skip(SkipReason::UserNotFound),
        // Without an answer we don't know where to send anything, so we don't
        Err(error) => return Recipient::Skip(SkipReason::LookupFailed(error.to_string())),
    };

    recipient_from_attributes(&attributes, smtp_config)
}

/// Decide where to send based on what the directory said about a user.
fn recipient_from_attributes(
    attributes: &userinfo::UserAttributes,
    smtp_config: &config::SmtpConfig,
) -> Recipient {
    if smtp_config.skip_inactive_users && attributes.account_locked == Some(true) {
        return Recipient::Skip(SkipReason::AccountLocked);
    }

    let mut placeholder = None;
    let mut unusable = None;
    for address in attributes
        .mails
        .iter()
        .map(|address| address.trim())
        .filter(|address| !address.is_empty())
    {
        if smtp_config.is_placeholder_address(address) {
            placeholder = placeholder.or_else(|| Some(address.to_string()));
            continue;
        }
        match address.parse::<Mailbox>() {
            Ok(mailbox) => return Recipient::Send(mailbox),
            Err(error) => {
                unusable = unusable
                    .or_else(|| Some(format!("`{}` is not a valid address: {}", address, error)));
            }
        }
    }

    // A placeholder is at least an address somebody chose, so it is the more
    // telling thing to report
    if let Some(address) = placeholder {
        return Recipient::Skip(SkipReason::PlaceholderAddress(address));
    }
    Recipient::Skip(SkipReason::NoAddress(unusable.unwrap_or_else(|| {
        "the directory has no `mail` attribute for them".to_string()
    })))
}

/// Build the SMTP transport the configuration describes.
fn build_mailer(smtp_config: &config::SmtpConfig) -> Result<SmtpTransport, NotificationError> {
    // Support relay as "host" or "host:port" (and "[IPv6]:port")
    let (relay_host, relay_port) = split_host_port(&smtp_config.relay);
    let mut builder = SmtpTransport::relay(relay_host).map_err(NotificationError::SmtpError)?;

    let tls_parameters = || {
        TlsParameters::new(relay_host.to_string())
            .map_err(|_| NotificationError::TlsParametersInvalid(relay_host.to_string()))
    };

    // TLS mode: default STARTTLS; if WRAPPER and no port given, default to 465
    match smtp_config.tls.unwrap_or(config::TlsMode::Starttls) {
        config::TlsMode::Wrapper => {
            builder = builder
                .port(relay_port.unwrap_or(465))
                .tls(Tls::Wrapper(tls_parameters()?));
        }
        config::TlsMode::Starttls => {
            if let Some(port) = relay_port {
                builder = builder.port(port);
            }
            builder = builder.tls(Tls::Required(tls_parameters()?));
        }
    }

    // Optional auth mechanism override
    if let Some(method) = smtp_config.auth {
        builder = builder.authentication(vec![match method {
            config::AuthMethod::Plain => Mechanism::Plain,
            config::AuthMethod::Login => Mechanism::Login,
        }]);
    }

    Ok(builder
        .credentials(Credentials::new(
            smtp_config.username.to_owned(),
            smtp_config.password.to_owned(),
        ))
        .build())
}

/// The address notifications are sent from: `[smtp] from` if it is set, else
/// the SMTP login name, which then has to be an email address.
fn from_mailbox(smtp_config: &config::SmtpConfig) -> Result<Mailbox, NotificationError> {
    match smtp_config.from.clone() {
        Some(mailbox) => Ok(mailbox),
        None => smtp_config
            .username
            .parse()
            .map_err(NotificationError::MailboxParseError),
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
) -> Result<NotificationOutcome, NotificationError> {
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
    let Some(duration_from_expiry_when_notification_should_have_been_issued) = filesystem
        .expiry_notifications_on_days
        .iter()
        .find(|d| d > &&duration_until_expiry)
    else {
        return Ok(NotificationOutcome::NotDue);
    };

    // ... and check if our last message is more recent ...
    if !duration_since_last_notification.map_or(true, |d| {
        (Utc::now() - d)
            < (expiration_time - *duration_from_expiry_when_notification_should_have_been_issued)
    }) {
        return Ok(NotificationOutcome::NotDue);
    }

    // if not, we have to notify the user -- which is when it is worth finding
    // out where they can be reached
    let recipient = match resolve_recipient(username, smtp_config) {
        Recipient::Send(mailbox) => mailbox,
        // Deliberately *not* recorded in `notifications`: a user whose
        // address starts working again should still get their reminder
        Recipient::Skip(reason) => return Ok(NotificationOutcome::Skipped(reason)),
    };

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

    let email = Message::builder()
        .from(from_mailbox(smtp_config)?)
        .to(recipient)
        .header(ContentType::TEXT_PLAIN)
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
        .map_err(NotificationError::MessageBuildError)?;

    build_mailer(smtp_config)?
        .send(&email)
        .map_err(NotificationError::SmtpError)?;
    connection
        .execute(
            "INSERT INTO notifications(workspace_id, timestamp) VALUES(?1, ?2)",
            (workspace_id, Utc::now()),
        )
        .map_err(NotificationError::DatabaseError)?;

    Ok(NotificationOutcome::Sent)
}

/// Admin-only: send a one-off test email using SMTP config.
/// If `to_override` is Some, send to that address; otherwise look up where the
/// target user's notifications would go.
pub fn notify_test(
    target_username: &str,
    to_override: Option<String>,
    smtp_config: &config::SmtpConfig,
) -> Result<(), Box<dyn Error>> {
    // Resolve recipient
    let to_mailbox: Mailbox = match to_override {
        Some(to) => to.parse().map_err(NotificationError::MailboxParseError)?,
        None => match resolve_recipient(target_username, smtp_config) {
            Recipient::Send(mailbox) => mailbox,
            Recipient::Skip(reason) => {
                return Err(format!(
                    "no test email could be sent to {}: {}",
                    target_username, reason
                )
                .into());
            }
        },
    };

    let host = hostname::get()?.to_string_lossy().to_string();
    let subject = format!("Workspaces test email from {}", host);
    let body = format!(
        "Hello,\n\nThis is a test email sent by Workspaces on {}.\n\
If you can read this, SMTP is configured correctly.\n",
        host
    );

    let msg = Message::builder()
        .from(from_mailbox(smtp_config)?)
        .to(to_mailbox.clone())
        .header(ContentType::TEXT_PLAIN)
        .subject(subject)
        .body(body)?;

    build_mailer(smtp_config)?.send(&msg)?;
    println!("Sent test email to {}", to_mailbox);
    Ok(())
}

/// Send a one-off event email with custom subject/body to a user.
fn notify_event(
    target_username: &str,
    smtp_config: &config::SmtpConfig,
    subject: String,
    body: String,
) -> Result<NotificationOutcome, NotificationError> {
    let recipient = match resolve_recipient(target_username, smtp_config) {
        Recipient::Send(mailbox) => mailbox,
        Recipient::Skip(reason) => return Ok(NotificationOutcome::Skipped(reason)),
    };

    let msg = Message::builder()
        .from(from_mailbox(smtp_config)?)
        .to(recipient)
        .header(ContentType::TEXT_PLAIN)
        .subject(subject)
        .body(body)
        .map_err(NotificationError::MessageBuildError)?;

    build_mailer(smtp_config)?
        .send(&msg)
        .map_err(NotificationError::SmtpError)?;

    Ok(NotificationOutcome::Sent)
}

/// Send an event email, saying on stderr if it could not be delivered.
///
/// For the mails the commands send themselves: an admin acting for somebody
/// else gets to see why nothing was sent, but it is not worth an admin report.
pub fn notify_event_or_warn(
    target_username: &str,
    smtp_config: &config::SmtpConfig,
    kind: &str,
    subject: String,
    body: String,
) {
    match notify_event(target_username, smtp_config, subject, body) {
        Ok(NotificationOutcome::Skipped(reason)) => eprintln!(
            "No '{}' email was sent to {}: {}",
            kind, target_username, reason
        ),
        Ok(_) => {}
        Err(error) => eprintln!(
            "Failed to send '{}' email to {}: {}",
            kind, target_username, error
        ),
    }
}

/// Mail the admin a summary of the notifications this run could not deliver,
/// grouped by user.
///
/// Does nothing if there is nothing to report, or if no `admin_report_to` is
/// configured.
fn send_admin_report(
    skipped: &mut [SkippedNotification],
    smtp_config: &config::SmtpConfig,
) -> Result<(), NotificationError> {
    let Some(to) = smtp_config.admin_report_to.clone() else {
        return Ok(());
    };
    if skipped.is_empty() {
        return Ok(());
    }

    let host = hostname::get()?.to_string_lossy().to_string();
    let msg = Message::builder()
        .from(from_mailbox(smtp_config)?)
        .to(to)
        .header(ContentType::TEXT_PLAIN)
        .subject(format!(
            "Workspaces on {}: {} notification(s) could not be delivered",
            host,
            skipped.len()
        ))
        .body(admin_report_body(skipped, &host))
        .map_err(NotificationError::MessageBuildError)?;

    build_mailer(smtp_config)?
        .send(&msg)
        .map_err(NotificationError::SmtpError)?;

    Ok(())
}

/// The admin report's text: every skipped notification, grouped by user and
/// sorted by the reason it was skipped.
fn admin_report_body(skipped: &mut [SkippedNotification], host: &str) -> String {
    skipped.sort_by(|a, b| {
        a.user
            .cmp(&b.user)
            .then(a.reason.cmp(&b.reason))
            .then(a.workspace.cmp(&b.workspace))
    });

    let mut body = format!(
        "Hello,\n\nthe workspaces maintenance run on {} could not deliver {} \
         notification(s):\n",
        host,
        skipped.len()
    );

    let mut last_user: Option<&str> = None;
    let mut last_reason: Option<&SkipReason> = None;
    for notification in skipped.iter() {
        if last_user != Some(&notification.user) {
            body.push_str(&format!("\n{}\n", notification.user));
            last_user = Some(&notification.user);
            last_reason = None;
        }
        if last_reason != Some(&notification.reason) {
            body.push_str(&format!("  {}\n", notification.reason));
            last_reason = Some(&notification.reason);
        }
        body.push_str(&format!(
            "    - {} for workspace \"{}\" on filesystem \"{}\" ({} {})\n",
            notification.kind.description(),
            notification.workspace,
            notification.filesystem,
            notification.kind.deadline_description(),
            notification.deadline.format("%Y-%m-%d"),
        ));
    }

    body.push_str(
        "\nReminders are not recorded as sent, so they will be retried on the next \
         maintenance run.\nNotices about workspaces that have already been deleted will not.\n",
    );

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use users::os::unix::UserExt;

    fn smtp_config(
        placeholder_mail_domains: &[&str],
        recipient_source: config::RecipientSource,
    ) -> config::SmtpConfig {
        config::SmtpConfig {
            relay: "smtp.example.org".to_string(),
            username: "workspaces@example.org".to_string(),
            password: "supersecret".to_string(),
            from: None,
            tls: None,
            auth: None,
            recipient_source,
            skip_inactive_users: false,
            placeholder_mail_domains: placeholder_mail_domains
                .iter()
                .map(|domain| domain.to_string())
                .collect(),
            admin_report_to: None,
        }
    }

    /// A user whose home directory is a fresh, empty directory
    fn user_with_home(name: &str) -> (users::User, PathBuf) {
        let home =
            std::env::temp_dir().join(format!("workspaces-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config")).unwrap();

        (
            users::User::new(4242, name, 4242).with_home_dir(&home),
            home,
        )
    }

    fn write_user_config(home: &PathBuf, contents: &str) {
        fs::write(home.join(".config/workspaces.toml"), contents).unwrap();
    }

    #[test]
    fn the_address_from_a_users_own_file_is_used() {
        let (user, home) = user_with_home("address");
        write_user_config(&home, "email = \"alice@tu-dresden.de\"\n");

        let recipient =
            recipient_from_file(&user, &smtp_config(&[], config::RecipientSource::File));

        assert!(
            matches!(&recipient, Recipient::Send(mailbox)
                if mailbox.email.to_string() == "alice@tu-dresden.de"),
            "{:?}",
            recipient
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_missing_or_broken_file_means_no_address() {
        let (user, home) = user_with_home("no-file");
        let config = smtp_config(&[], config::RecipientSource::File);

        assert!(
            matches!(
                recipient_from_file(&user, &config),
                Recipient::Skip(SkipReason::NoAddress(..))
            ),
            "a user without a configuration file has no address"
        );

        write_user_config(&home, "email = \"not an address\"\n");
        assert!(
            matches!(
                recipient_from_file(&user, &config),
                Recipient::Skip(SkipReason::NoAddress(..))
            ),
            "an invalid address is no address either"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_placeholder_address_in_a_users_file_is_not_used() {
        let (user, home) = user_with_home("placeholder");
        write_user_config(&home, "email = \"alice@katherlab.internal\"\n");

        let recipient = recipient_from_file(
            &user,
            &smtp_config(&["katherlab.internal"], config::RecipientSource::File),
        );

        assert_eq!(
            match recipient {
                Recipient::Skip(SkipReason::PlaceholderAddress(address)) => address,
                other => panic!("expected a placeholder skip, got {:?}", other),
            },
            "alice@katherlab.internal"
        );
        let _ = fs::remove_dir_all(&home);
    }

    fn attributes(mails: &[&str], account_locked: Option<bool>) -> userinfo::UserAttributes {
        userinfo::UserAttributes {
            mails: mails.iter().map(|mail| mail.to_string()).collect(),
            account_locked,
        }
    }

    #[test]
    fn the_first_usable_address_from_the_directory_is_used() {
        let config = smtp_config(&["katherlab.internal"], config::RecipientSource::Sssd);

        for mails in [
            // A single address
            &["alice@tu-dresden.de"][..],
            // The first of several
            &["alice@tu-dresden.de", "a.lice@tu-dresden.de"][..],
            // Skipping what cannot be used
            &["", "alice@katherlab.internal", "alice@tu-dresden.de"][..],
        ] {
            let recipient = recipient_from_attributes(&attributes(mails, Some(false)), &config);

            assert!(
                matches!(&recipient, Recipient::Send(mailbox)
                    if mailbox.email.to_string() == "alice@tu-dresden.de"),
                "{:?} for {:?}",
                recipient,
                mails
            );
        }
    }

    #[test]
    fn a_directory_without_an_address_is_skipped() {
        let config = smtp_config(&["katherlab.internal"], config::RecipientSource::Sssd);

        assert!(matches!(
            recipient_from_attributes(&attributes(&[], Some(false)), &config),
            Recipient::Skip(SkipReason::NoAddress(..))
        ));
        assert!(matches!(
            recipient_from_attributes(&attributes(&["not an address"], None), &config),
            Recipient::Skip(SkipReason::NoAddress(..))
        ));
        assert!(matches!(
            recipient_from_attributes(&attributes(&["alice@KATHERLAB.internal"], None), &config),
            Recipient::Skip(SkipReason::PlaceholderAddress(..))
        ));
    }

    #[test]
    fn a_disabled_account_is_only_skipped_if_configured() {
        let mails = ["bob@tu-dresden.de"];

        let mut config = smtp_config(&[], config::RecipientSource::Sssd);
        assert!(
            matches!(
                recipient_from_attributes(&attributes(&mails, Some(true)), &config),
                Recipient::Send(..)
            ),
            "`skip_inactive_users` is off, so a disabled account still gets mail"
        );

        config.skip_inactive_users = true;
        assert!(matches!(
            recipient_from_attributes(&attributes(&mails, Some(true)), &config),
            Recipient::Skip(SkipReason::AccountLocked)
        ));
        // A directory that doesn't say counts as enabled
        assert!(matches!(
            recipient_from_attributes(&attributes(&mails, None), &config),
            Recipient::Send(..)
        ));
        assert!(matches!(
            recipient_from_attributes(&attributes(&mails, Some(false)), &config),
            Recipient::Send(..)
        ));
    }

    #[test]
    fn sssd_then_file_falls_back_to_the_users_own_file() {
        // The directory has nothing to say about a made-up user -- whether
        // because it does not know them or because it cannot be asked at all
        let (user, home) = user_with_home("fallback");
        write_user_config(&home, "email = \"alice@tu-dresden.de\"\n");

        let recipient = recipient_for(
            &user,
            &smtp_config(&[], config::RecipientSource::SssdThenFile),
        );

        assert!(
            matches!(&recipient, Recipient::Send(mailbox)
                if mailbox.email.to_string() == "alice@tu-dresden.de"),
            "{:?}",
            recipient
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_user_the_system_does_not_know_is_skipped() {
        let recipient = resolve_recipient(
            "no-such-user-5ad9e1f0",
            &smtp_config(&[], config::RecipientSource::File),
        );

        assert!(
            matches!(recipient, Recipient::Skip(SkipReason::UserNotFound)),
            "{:?}",
            recipient
        );
    }

    fn skipped(
        user: &str,
        reason: SkipReason,
        workspace: &str,
        kind: NotificationKind,
    ) -> SkippedNotification {
        SkippedNotification {
            user: user.to_string(),
            reason,
            workspace: workspace.to_string(),
            filesystem: "bulk".to_string(),
            kind,
            deadline: DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
        }
    }

    #[test]
    fn the_admin_report_groups_by_user_and_reason() {
        let mut notifications = vec![
            skipped(
                "bob",
                SkipReason::AccountLocked,
                "scratch",
                NotificationKind::Deleted,
            ),
            skipped(
                "alice",
                SkipReason::NoAddress("no mail attribute".to_string()),
                "images",
                NotificationKind::ExpiryReminder,
            ),
            skipped(
                "alice",
                SkipReason::UserNotFound,
                "slides",
                NotificationKind::DeletionReminder,
            ),
            skipped(
                "alice",
                SkipReason::NoAddress("no mail attribute".to_string()),
                "data",
                NotificationKind::ExpiryReminder,
            ),
        ];

        let body = admin_report_body(&mut notifications, "ws.example.org");

        assert_eq!(
            body,
            "Hello,\n\
             \n\
             the workspaces maintenance run on ws.example.org could not deliver 4 \
             notification(s):\n\
             \n\
             alice\n\
             \x20 the user does not exist on this system (any more)\n\
             \x20   - deletion reminder for workspace \"slides\" on filesystem \"bulk\" \
             (to be deleted 2026-01-01)\n\
             \x20 no email address is known for the user (no mail attribute)\n\
             \x20   - expiry reminder for workspace \"data\" on filesystem \"bulk\" \
             (expires 2026-01-01)\n\
             \x20   - expiry reminder for workspace \"images\" on filesystem \"bulk\" \
             (expires 2026-01-01)\n\
             \n\
             bob\n\
             \x20 the user's account is disabled in the directory\n\
             \x20   - deletion notice for workspace \"scratch\" on filesystem \"bulk\" \
             (deleted 2026-01-01)\n\
             \n\
             Reminders are not recorded as sent, so they will be retried on the next \
             maintenance run.\n\
             Notices about workspaces that have already been deleted will not.\n"
        );
    }
}
