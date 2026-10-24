//! Errors from the pi adapter.

use std::path::PathBuf;

/// An error from the pi adapter.
#[derive(Debug, thiserror::Error)]
pub enum PiError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {what}: {source}")]
    Json {
        what: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot run `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

impl PiError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn json(what: impl Into<String>, source: serde_json::Error) -> Self {
        Self::Json {
            what: what.into(),
            source,
        }
    }
}

impl From<PiError> for terrarium_core::Error {
    fn from(e: PiError) -> Self {
        match e {
            PiError::Io { path, source } => terrarium_core::Error::Io { path, source },
            other => terrarium_core::Error::Other(other.to_string()),
        }
    }
}

pub type Result<T, E = PiError> = std::result::Result<T, E>;
