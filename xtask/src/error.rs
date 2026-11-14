use std::fmt;
use std::path::PathBuf;

/// Everything that stops an xtask command from giving an answer.
///
/// A check that ran and said "no" is not an error: it is a result, and the
/// command reports it through its exit code.
#[derive(Debug)]
pub enum Error {
    Usage(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Spawn {
        command: String,
        source: std::io::Error,
    },
    Manifest {
        path: PathBuf,
        reason: String,
    },
    Schema {
        path: PathBuf,
        reason: String,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(message) => write!(f, "{message}"),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Spawn { command, source } => write!(f, "could not run `{command}`: {source}"),
            Error::Manifest { path, reason } => write!(f, "{}: {reason}", path.display()),
            Error::Schema { path, reason } => write!(f, "{}: {reason}", path.display()),
            Error::Json { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
