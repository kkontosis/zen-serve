//! Errors: server answers (spec/api.md §1), transport failures, client-side
//! checks, and integrity alarms (spec/formats.md §14.4).

use std::fmt;
use std::time::Duration;

/// What an alarm is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmObject {
    /// The ACL chain.
    Acl,
    /// The signed fs header.
    Header,
    /// A device's checkpoints.
    Checkpoint,
    /// A topic.
    Topic,
    /// A filesystem tree.
    Tree,
    /// A zen-db namespace.
    Db,
    /// A block volume.
    Vol,
    /// A relayed connection.
    Svc,
}

/// The kind of a detected attack, as in the TS client:
/// * `Rollback`: an answer older than a pin (a withheld tail, an older value)
/// * `Fork`: two histories (two bodies of one device at one seq, a `seen`
///   link the server can't back, an entry lower than its writer's previous one)
/// * `Gap`, `Rewrite`: a skipped or replaced event of a chain (formats.md §5.1)
/// * `Invalid`: bytes no device sealed or signed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmKind {
    /// An answer older than a pin.
    Rollback,
    /// Two histories.
    Fork,
    /// A skipped chain position.
    Gap,
    /// Another body at a chain position.
    Rewrite,
    /// Bytes no device sealed or signed.
    Invalid,
}

impl AlarmObject {
    /// The name the TS client uses.
    pub fn name(self) -> &'static str {
        match self {
            AlarmObject::Acl => "acl",
            AlarmObject::Header => "header",
            AlarmObject::Checkpoint => "checkpoint",
            AlarmObject::Topic => "topic",
            AlarmObject::Tree => "tree",
            AlarmObject::Db => "db",
            AlarmObject::Vol => "vol",
            AlarmObject::Svc => "svc",
        }
    }
}

impl AlarmKind {
    /// The name the TS client uses.
    pub fn name(self) -> &'static str {
        match self {
            AlarmKind::Rollback => "rollback",
            AlarmKind::Fork => "fork",
            AlarmKind::Gap => "gap",
            AlarmKind::Rewrite => "rewrite",
            AlarmKind::Invalid => "invalid",
        }
    }
}

/// A detected attack (spec/formats.md §14.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    /// What happened.
    pub kind: AlarmKind,
    /// What it is about.
    pub object: AlarmObject,
    /// The topic id, device fingerprint, volume, …
    pub id: Option<Vec<u8>>,
    /// A human-readable account.
    pub detail: String,
}

impl Alarm {
    /// The alarm as JSON, field names as in the TS client (`id` in hex).
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "kind": self.kind.name(),
            "object": self.object.name(),
            "detail": self.detail,
        });
        if let Some(id) = &self.id {
            v["id"] = crate::hex(id).into();
        }
        v
    }
}

impl fmt::Display for Alarm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind.name(),
            self.object.name(),
            self.detail
        )
    }
}

/// An error of this client.
#[derive(Debug, Clone)]
pub enum Error {
    /// An error answer of the server.
    Api {
        /// HTTP status.
        status: u16,
        /// Machine-readable code (`conflict`, `forbidden`, …).
        code: String,
        /// Human-readable message.
        message: String,
        /// `Retry-After`, when the server sent one.
        retry_after: Option<Duration>,
    },
    /// The request got no answer.
    Network(String),
    /// A detected attack: the fs is untrusted from now on.
    Integrity(Alarm),
    /// A check of this client's own (`acl_chain`, `signer_needed`, `no_slot`,
    /// `epoch_ahead`, `format`, `locked`, …).
    Local {
        /// The code, as the TS client names it.
        code: &'static str,
        /// What failed.
        message: String,
    },
    /// A local file.
    Io(String),
}

impl Error {
    /// A client-side error.
    pub fn local(code: &'static str, message: impl Into<String>) -> Self {
        Error::Local {
            code,
            message: message.into(),
        }
    }

    /// The error code: the server's, `network`, `integrity`, `io`, or a local one.
    pub fn code(&self) -> &str {
        match self {
            Error::Api { code, .. } => code,
            Error::Network(_) => "network",
            Error::Integrity(_) => "integrity",
            Error::Local { code, .. } => code,
            Error::Io(_) => "io",
        }
    }

    /// The HTTP status; 0 for errors without a server answer.
    pub fn status(&self) -> u16 {
        match self {
            Error::Api { status, .. } => *status,
            _ => 0,
        }
    }

    /// Whether this error has `code`.
    pub fn is(&self, code: &str) -> bool {
        self.code() == code
    }

    /// Whether the whole transaction may be retried (409 `conflict`, `too_old`).
    pub fn retryable(&self) -> bool {
        self.status() == 409 && (self.is("conflict") || self.is("too_old"))
    }

    /// The alarm, for an integrity error.
    pub fn alarm(&self) -> Option<&Alarm> {
        match self {
            Error::Integrity(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn format(message: impl Into<String>) -> Self {
        Error::local("format", message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Api {
                status,
                code,
                message,
                ..
            } => write!(f, "{code} ({status}): {message}"),
            Error::Network(m) => write!(f, "network: {m}"),
            Error::Integrity(a) => write!(f, "integrity: {a}"),
            Error::Local { code, message } => write!(f, "{code}: {message}"),
            Error::Io(m) => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<zen_core::Error> for Error {
    fn from(e: zen_core::Error) -> Self {
        let code = match e {
            zen_core::Error::Format => "format",
            zen_core::Error::Decrypt => "decrypt",
            zen_core::Error::Signature => "signature",
            _ => "crypto",
        };
        Error::local(code, e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

/// A result of this client.
pub type Result<T> = std::result::Result<T, Error>;
