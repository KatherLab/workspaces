//! User-facing error reporting.
//!
//! The errors we deal with internally (`rusqlite::Error`, `io::Error`, failed
//! `zfs` invocations, …) are not something an end user can act on: a full disk
//! surfaces as `SqliteFailure(Error { code: DiskFull, extended_code: 13 },
//! Some("database or disk is full"))`, and a panic additionally dumps a source
//! location and a backtrace hint. Neither is a helpful last thing to read
//! when your workspace just failed to be created.
//!
//! This module classifies an error chain into one of a few situations we can
//! describe in plain words, prints that description, and exits with a fitting
//! code.

use std::{error::Error, fmt, io, process};

use crate::ExitCodes;

/// Exit code for errors we have no special wording for.
///
/// This is what `fn main() -> Result<_, _>` used to return on any error, so
/// existing scripts keep seeing `1` for everything we cannot classify.
const EXIT_GENERAL_FAILURE: i32 = 1;

/// A failure situation we can explain to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Situation {
    /// The device holding the database (or a workspace) has no space left
    StorageFull,
    /// A disk quota is exhausted
    QuotaExceeded,
    /// The database cannot be written to
    ReadOnly,
    /// A file could not be accessed because of its permissions
    PermissionDenied,
    /// Another process is holding the database lock
    DatabaseBusy,
    /// The database file is damaged, or not a database at all
    DatabaseDamaged,
    /// Anything we have no special wording for
    Unknown,
}

impl Situation {
    /// The paragraph printed below the error message.
    fn advice(self) -> &'static str {
        match self {
            Situation::StorageFull => {
                "The disk is full, so the operation was aborted. Please ask your system\n\
                 administrator to free up space, then try again."
            }
            Situation::QuotaExceeded => {
                "A storage quota is exhausted, so the operation was aborted. Please ask your\n\
                 system administrator to raise the quota, then try again."
            }
            Situation::ReadOnly => {
                "The workspaces database cannot be written to. This usually means that the disk\n\
                 it lies on was mounted read-only, e.g. after a disk error. Please contact your\n\
                 system administrator."
            }
            Situation::PermissionDenied => {
                "You may not have the permissions needed for this operation. If you think you\n\
                 should, please contact your system administrator."
            }
            Situation::DatabaseBusy => {
                "Another `workspaces` process is currently using the database. Please try again\n\
                 in a moment."
            }
            Situation::DatabaseDamaged => {
                "The workspaces database could not be read. Please contact your system\n\
                 administrator."
            }
            Situation::Unknown => {
                "Workspaces could not complete this operation. If this keeps happening, please\n\
                 contact your system administrator."
            }
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Situation::StorageFull | Situation::QuotaExceeded => ExitCodes::StorageFull as i32,
            Situation::ReadOnly | Situation::DatabaseBusy | Situation::DatabaseDamaged => {
                ExitCodes::DatabaseError as i32
            }
            Situation::PermissionDenied => ExitCodes::InsufficientPrivileges as i32,
            Situation::Unknown => EXIT_GENERAL_FAILURE,
        }
    }
}

/// Print `error` in a form an end user can act on, then exit.
pub fn report(error: &(dyn Error + 'static)) -> ! {
    eprintln!("{}", message(error));
    process::exit(classify(error).exit_code())
}

/// What the user gets to read: what failed, and what they can do about it.
fn message(error: &(dyn Error + 'static)) -> String {
    format!("Error: {}\n\n{}", describe(error), classify(error).advice())
}

/// Replace the default panic handler, so that a bug doesn't greet the user
/// with a Rust panic message and a backtrace hint.
///
/// Set `RUST_BACKTRACE` to get the usual details for debugging.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown cause".to_string());
        let location = info
            .location()
            .map_or_else(|| "unknown location".to_string(), |l| l.to_string());

        eprintln!("Error: workspaces stopped because of an internal error.");
        eprintln!();
        eprintln!("{payload} (at {location})");
        eprintln!();
        eprintln!(
            "This is a bug in workspaces. Please report it to your system administrator,\n\
             together with the command you ran."
        );

        if std::env::var_os("RUST_BACKTRACE").is_some() {
            eprintln!("\n{}", std::backtrace::Backtrace::force_capture());
        }

        process::exit(ExitCodes::InternalError as i32)
    }));
}

/// The error and its causes, as a single `outer: inner: innermost` line.
///
/// Causes that only repeat what their parent already said are left out, so
/// that contexts added by [`Context::context`] read as one sentence.
fn describe(error: &(dyn Error + 'static)) -> String {
    let mut description = String::new();
    for cause in causes(error) {
        let message = cause.to_string();
        if !message.is_empty() && !description.contains(&message) {
            if !description.is_empty() {
                description.push_str(": ");
            }
            description.push_str(&message);
        }

        // `rusqlite` repeats itself: its message is followed by a cause from
        // the SQLite bindings ("Error code 13: database or disk is full"),
        // which only restates it in more technical terms
        if cause.is::<rusqlite::Error>() {
            break;
        }
    }
    description
}

/// Decide which situation an error chain describes.
fn classify(error: &(dyn Error + 'static)) -> Situation {
    for cause in causes(error) {
        if let Some(error) = cause.downcast_ref::<rusqlite::Error>()
            && let Some(situation) = classify_sqlite(error)
        {
            return situation;
        }
        if let Some(error) = cause.downcast_ref::<io::Error>()
            && let Some(situation) = classify_io(error)
        {
            return situation;
        }
    }
    Situation::Unknown
}

fn classify_sqlite(error: &rusqlite::Error) -> Option<Situation> {
    use libsqlite3_sys::ErrorCode;

    let rusqlite::Error::SqliteFailure(error, message) = error else {
        return None;
    };

    // Depending on where in the write path a full disk is noticed, SQLite
    // reports it as SQLITE_FULL or as an I/O error that says as much in its
    // message
    let message_mentions_full_disk = message
        .as_deref()
        .is_some_and(|message| message.contains("disk is full"));

    match error.code {
        ErrorCode::DiskFull => Some(Situation::StorageFull),
        ErrorCode::ReadOnly => Some(Situation::ReadOnly),
        ErrorCode::PermissionDenied | ErrorCode::AuthorizationForStatementDenied => {
            Some(Situation::PermissionDenied)
        }
        ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Some(Situation::DatabaseBusy),
        ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => Some(Situation::DatabaseDamaged),
        _ if message_mentions_full_disk => Some(Situation::StorageFull),
        _ => None,
    }
}

fn classify_io(error: &io::Error) -> Option<Situation> {
    /// `ENOSPC`, in case the platform doesn't map it to `StorageFull`
    const ENOSPC: i32 = 28;

    match error.kind() {
        io::ErrorKind::StorageFull => Some(Situation::StorageFull),
        io::ErrorKind::QuotaExceeded => Some(Situation::QuotaExceeded),
        io::ErrorKind::ReadOnlyFilesystem => Some(Situation::ReadOnly),
        io::ErrorKind::PermissionDenied => Some(Situation::PermissionDenied),
        _ if error.raw_os_error() == Some(ENOSPC) => Some(Situation::StorageFull),
        _ => None,
    }
}

/// An error together with everything that caused it, outermost first.
fn causes<'a>(error: &'a (dyn Error + 'static)) -> Vec<&'a (dyn Error + 'static)> {
    let mut causes = vec![error];
    let mut cause: Option<&'a (dyn Error + 'static)> = error.source();
    while let Some(error) = cause {
        causes.push(error);
        cause = error.source();
    }
    causes
}

/// An error with a sentence of user-facing context in front of it.
#[derive(Debug)]
pub struct Contextual {
    context: String,
    source: Box<dyn Error + 'static>,
}

impl fmt::Display for Contextual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.context)
    }
}

impl Error for Contextual {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Describe what we were trying to do when `error` happened, e.g.
/// `with_context("could not create the dataset", error)`.
///
/// The context is what the user gets to read first; the error itself is kept
/// as the cause, both for the details line and so that the likes of a full
/// disk can still be recognized.
pub fn with_context(
    context: impl Into<String>,
    error: impl Into<Box<dyn Error + 'static>>,
) -> Box<dyn Error> {
    Box::new(Contextual {
        context: context.into(),
        source: error.into(),
    })
}

/// Adds [`with_context`] as a method on `Result`.
pub trait Context<T> {
    /// Describe what we were trying to do, e.g.
    /// `.context("could not create the dataset")?`.
    fn context(self, context: impl Into<String>) -> Result<T, Box<dyn Error>>;
}

impl<T, E: Into<Box<dyn Error + 'static>>> Context<T> for Result<T, E> {
    fn context(self, context: impl Into<String>) -> Result<T, Box<dyn Error>> {
        self.map_err(|error| with_context(context, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_error(code: libsqlite3_sys::ErrorCode, message: &str) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            libsqlite3_sys::Error {
                code,
                extended_code: code as i32,
            },
            Some(message.to_string()),
        )
    }

    #[test]
    fn full_disk_is_recognized_through_context() {
        let error = with_context(
            "could not record the new workspace in the database",
            sqlite_error(
                libsqlite3_sys::ErrorCode::DiskFull,
                "database or disk is full",
            ),
        );

        assert_eq!(classify(error.as_ref()), Situation::StorageFull);
        assert_eq!(
            describe(error.as_ref()),
            "could not record the new workspace in the database: database or disk is full"
        );
    }

    #[test]
    fn a_full_disk_is_explained_to_the_user() {
        let error = with_context(
            "could not record the new workspace in the database",
            sqlite_error(
                libsqlite3_sys::ErrorCode::DiskFull,
                "database or disk is full",
            ),
        );

        assert_eq!(
            message(error.as_ref()),
            "Error: could not record the new workspace in the database: \
             database or disk is full\n\
             \n\
             The disk is full, so the operation was aborted. Please ask your system\n\
             administrator to free up space, then try again."
        );
    }

    #[test]
    fn full_disk_is_recognized_from_an_io_error() {
        let error = with_context(
            "could not write to the database",
            io::Error::from_raw_os_error(28),
        );

        assert_eq!(classify(error.as_ref()), Situation::StorageFull);
    }

    #[test]
    fn a_read_only_database_is_recognized() {
        let error: Box<dyn Error> = Box::new(sqlite_error(
            libsqlite3_sys::ErrorCode::ReadOnly,
            "attempt to write a readonly database",
        ));

        assert_eq!(classify(error.as_ref()), Situation::ReadOnly);
        assert_eq!(classify(error.as_ref()).exit_code(), 8);
    }

    #[test]
    fn unrelated_errors_stay_unclassified() {
        let error = with_context("could not rename the dataset", "zfs exited with status 1");

        assert_eq!(classify(error.as_ref()), Situation::Unknown);
        assert_eq!(classify(error.as_ref()).exit_code(), EXIT_GENERAL_FAILURE);
        assert_eq!(
            describe(error.as_ref()),
            "could not rename the dataset: zfs exited with status 1"
        );
    }
}
