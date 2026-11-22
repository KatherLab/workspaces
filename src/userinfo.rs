//! Looking up user information in the directory (FreeIPA, LDAP, AD) through
//! SSSD.
//!
//! SSSD publishes the attributes of the users it knows on the system bus, so
//! we can ask it for a user's email address and account status with `busctl`
//! instead of linking a D-Bus client:
//!
//! ```text
//! $ busctl call --system org.freedesktop.sssd.infopipe \
//!     /org/freedesktop/sssd/infopipe org.freedesktop.sssd.infopipe \
//!     GetUserAttr sas alice 2 mail nsAccountLock
//! a{sv} 2 "mail" as 1 "alice@tu-dresden.de" "nsAccountLock" as 1 "FALSE"
//! ```
//!
//! This requires the attributes to be allow-listed in `sssd.conf`:
//!
//! ```ini
//! [ifp]
//! user_attributes = +mail, +nsAccountLock
//! ```

use std::{
    io,
    process::{Command, ExitStatus},
};

/// `busctl`, by absolute path: `workspaces` is installed setuid root, so it
/// must not look up executables in a `PATH` the caller controls
const BUSCTL: &str = "/usr/bin/busctl";

const SSSD_SERVICE: &str = "org.freedesktop.sssd.infopipe";
const SSSD_OBJECT: &str = "/org/freedesktop/sssd/infopipe";
const SSSD_INTERFACE: &str = "org.freedesktop.sssd.infopipe";

/// The attributes we ask SSSD about
const ATTRIBUTES: [&str; 2] = ["mail", "nsAccountLock"];

/// What the directory knows about a user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UserAttributes {
    /// The user's `mail` values, in the order the directory lists them
    pub mails: Vec<String>,
    /// The user's `nsAccountLock`: `Some(true)` for a disabled account,
    /// `None` if the directory doesn't say
    pub account_locked: Option<bool>,
}

#[derive(Debug)]
pub enum Error {
    /// `busctl` could not be run at all
    Command(io::Error),
    /// `busctl` ran, but the call failed, e.g. because SSSD is not running
    Busctl { status: ExitStatus, stderr: String },
    /// The directory does not know this user
    UserNotFound,
    /// `busctl`'s output was not what we expected
    Parse(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Command(err) => write!(f, "could not run `{}`: {}", BUSCTL, err),
            Error::Busctl { status, stderr } if stderr.is_empty() => {
                write!(f, "`busctl` failed with {}", status)
            }
            Error::Busctl { status, stderr } => {
                write!(f, "`busctl` failed with {}: {}", status, stderr)
            }
            Error::UserNotFound => write!(f, "the directory does not know this user"),
            Error::Parse(detail) => {
                write!(f, "could not make sense of `busctl`'s output: {}", detail)
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Command(err) => Some(err),
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// Ask SSSD what the directory knows about `user`.
pub fn get_user_attributes(user: &str) -> Result<UserAttributes> {
    let output = Command::new(BUSCTL)
        // The environment of a setuid binary is the caller's; `busctl` has no
        // business reading it
        .env_clear()
        .args([
            "call",
            "--system",
            SSSD_SERVICE,
            SSSD_OBJECT,
            SSSD_INTERFACE,
            "GetUserAttr",
            "sas",
            user,
            &ATTRIBUTES.len().to_string(),
        ])
        .args(ATTRIBUTES)
        .output()
        .map_err(Error::Command)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // SSSD answers with a `NotFound` error for users it has never heard of
        if stderr.contains("NotFound") || stderr.contains("No such user") {
            return Err(Error::UserNotFound);
        }
        return Err(Error::Busctl {
            status: output.status,
            stderr,
        });
    }

    parse_user_attributes(&String::from_utf8_lossy(&output.stdout))
}

/// One piece of `busctl`'s output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A bare word: a type signature, a count, …
    Word(String),
    /// A quoted string value
    Text(String),
}

/// Parse the `a{sv}` dictionary `busctl call` prints for `GetUserAttr`, e.g.
/// `a{sv} 2 "mail" as 1 "alice@tu-dresden.de" "nsAccountLock" as 1 "FALSE"`.
fn parse_user_attributes(output: &str) -> Result<UserAttributes> {
    let tokens = tokenize(output);
    let mut tokens = tokens.into_iter();

    match tokens.next() {
        // Older `busctl` versions wrap the reply in its full signature
        Some(Token::Word(signature)) if signature.contains("a{sv}") => {}
        token => {
            return Err(Error::Parse(format!(
                "expected an `a{{sv}}` dictionary, found {}",
                describe(token.as_ref())
            )));
        }
    }

    let attribute_count = next_count(&mut tokens, "the number of attributes")?;
    let mut attributes = UserAttributes::default();

    for _ in 0..attribute_count {
        let name = match tokens.next() {
            Some(Token::Text(name)) => name,
            token => {
                return Err(Error::Parse(format!(
                    "expected an attribute name, found {}",
                    describe(token.as_ref())
                )));
            }
        };
        let values = next_values(&mut tokens, &name)?;

        match name.as_str() {
            "mail" => attributes.mails = values,
            // Anything but a plain `TRUE` leaves the account enabled: we would
            // rather send a notification too many than silently skip a user
            // because the directory said something we don't understand
            "nsAccountLock" => {
                attributes.account_locked = values
                    .first()
                    .map(|value| value.eq_ignore_ascii_case("true"));
            }
            // We only ask for the two attributes above, but SSSD is free to
            // send more
            _ => {}
        }
    }

    Ok(attributes)
}

/// The values of one attribute, i.e. a variant holding either a string or an
/// array of strings.
fn next_values(tokens: &mut impl Iterator<Item = Token>, name: &str) -> Result<Vec<String>> {
    match tokens.next() {
        // `as 2 "alice@example.org" "a.lice@example.org"`
        Some(Token::Word(signature)) if signature == "as" => {
            let value_count = next_count(tokens, "the number of values")?;
            let mut values = Vec::with_capacity(value_count);
            for _ in 0..value_count {
                match tokens.next() {
                    Some(Token::Text(value)) => values.push(value),
                    token => {
                        return Err(Error::Parse(format!(
                            "expected a value of `{}`, found {}",
                            name,
                            describe(token.as_ref())
                        )));
                    }
                }
            }
            Ok(values)
        }
        // `s "alice@example.org"`
        Some(Token::Word(signature)) if signature == "s" => match tokens.next() {
            Some(Token::Text(value)) => Ok(vec![value]),
            token => Err(Error::Parse(format!(
                "expected a value of `{}`, found {}",
                name,
                describe(token.as_ref())
            ))),
        },
        token => Err(Error::Parse(format!(
            "expected the type of `{}`, found {}",
            name,
            describe(token.as_ref())
        ))),
    }
}

fn next_count(tokens: &mut impl Iterator<Item = Token>, what: &str) -> Result<usize> {
    match tokens.next() {
        Some(Token::Word(word)) => word
            .parse()
            .map_err(|_| Error::Parse(format!("expected {}, found `{}`", what, word))),
        token => Err(Error::Parse(format!(
            "expected {}, found {}",
            what,
            describe(token.as_ref())
        ))),
    }
}

/// How a token is referred to in parsing errors.
fn describe(token: Option<&Token>) -> String {
    match token {
        Some(Token::Word(word)) => format!("`{}`", word),
        Some(Token::Text(text)) => format!("\"{}\"", text),
        None => "the end of the output".to_string(),
    }
}

/// Split `busctl`'s output into quoted strings and bare words.
fn tokenize(output: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut characters = output.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            _ if character.is_whitespace() => {}
            '"' => {
                let mut text = String::new();
                loop {
                    match characters.next() {
                        Some('\\') => {
                            if let Some(escaped) = characters.next() {
                                text.push(escaped);
                            }
                        }
                        Some('"') | None => break,
                        Some(character) => text.push(character),
                    }
                }
                tokens.push(Token::Text(text));
            }
            _ => {
                let mut word = String::from(character);
                while let Some(&character) = characters.peek() {
                    if character.is_whitespace() || character == '"' {
                        break;
                    }
                    word.push(character);
                    characters.next();
                }
                tokens.push(Token::Word(word));
            }
        }
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_active_user_with_an_address() {
        let attributes = parse_user_attributes(
            "a{sv} 2 \"mail\" as 1 \"alice@tu-dresden.de\" \"nsAccountLock\" as 1 \"FALSE\"\n",
        )
        .unwrap();

        assert_eq!(attributes.mails, ["alice@tu-dresden.de"]);
        assert_eq!(attributes.account_locked, Some(false));
    }

    #[test]
    fn a_disabled_user() {
        let attributes = parse_user_attributes(
            "a{sv} 2 \"mail\" as 1 \"bob@tu-dresden.de\" \"nsAccountLock\" as 1 \"TRUE\"\n",
        )
        .unwrap();

        assert_eq!(attributes.mails, ["bob@tu-dresden.de"]);
        assert_eq!(attributes.account_locked, Some(true));
    }

    #[test]
    fn a_user_without_an_address() {
        // SSSD leaves out the attributes it has no value for
        let attributes =
            parse_user_attributes("a{sv} 1 \"nsAccountLock\" as 1 \"FALSE\"\n").unwrap();

        assert!(attributes.mails.is_empty());
        assert_eq!(attributes.account_locked, Some(false));

        // … up to and including all of them
        let attributes = parse_user_attributes("a{sv} 0\n").unwrap();

        assert!(attributes.mails.is_empty());
        assert_eq!(attributes.account_locked, None);
    }

    #[test]
    fn a_user_with_several_addresses() {
        let attributes = parse_user_attributes(
            "a{sv} 2 \"mail\" as 2 \"carol@tu-dresden.de\" \"c.arol@tu-dresden.de\" \
             \"nsAccountLock\" as 1 \"FALSE\"\n",
        )
        .unwrap();

        assert_eq!(
            attributes.mails,
            ["carol@tu-dresden.de", "c.arol@tu-dresden.de"]
        );
    }

    #[test]
    fn an_address_given_as_a_plain_string() {
        let attributes =
            parse_user_attributes("a{sv} 1 \"mail\" s \"dave@tu-dresden.de\"\n").unwrap();

        assert_eq!(attributes.mails, ["dave@tu-dresden.de"]);
    }

    #[test]
    fn mangled_output_is_an_error() {
        for output in [
            "",
            "Call failed: Unexpected error",
            // Truncated half-way through
            "a{sv} 2 \"mail\" as",
            "a{sv} 2 \"mail\" as 2 \"eve@tu-dresden.de\"",
            // A count that isn't one
            "a{sv} lots \"mail\" as 1 \"eve@tu-dresden.de\"",
            // A type we don't know what to do with
            "a{sv} 1 \"mail\" ay 1 42",
        ] {
            assert!(
                matches!(parse_user_attributes(output), Err(Error::Parse(..))),
                "expected a parsing error for {:?}, got {:?}",
                output,
                parse_user_attributes(output)
            );
        }
    }
}
