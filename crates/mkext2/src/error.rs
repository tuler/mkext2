use std::fmt;
use std::io;

/// Errors produced while building or reading an image.
#[derive(Debug)]
pub enum Error {
    /// An I/O error, with a description of what was being done.
    Io { context: String, source: io::Error },
    /// A fatal condition that stops the build.
    Fatal(String),
    /// Invalid options or arguments.
    Usage(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn fatal(msg: impl Into<String>) -> Self {
        Error::Fatal(msg.into())
    }

    pub(crate) fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } if context.is_empty() => write!(f, "{source}"),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::Fatal(msg) | Error::Usage(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io {
            context: String::new(),
            source,
        }
    }
}

/// Attach context to I/O results.
pub(crate) trait IoContext<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, context: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|e| Error::io(context(), e))
    }
}
