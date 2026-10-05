use std::fmt;

/// The result type returned by every sqlscope operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Classifies why an operation failed.
///
/// The kind is stable and is what language bindings map to their own error
/// types; the message is human-readable and may change between releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// An option or argument is invalid (unknown dialect, empty predicate, ...).
    InvalidArgument,
    /// The SQL text could not be parsed.
    Parse,
    /// The SQL parsed, but the statement shape is not supported by the
    /// operation, or the input exceeded a safety limit.
    Unsupported,
    /// sqlscope produced an invalid result. This indicates a bug.
    Internal,
}

impl ErrorKind {
    /// A stable, lower-case identifier for the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::InvalidArgument => "invalid_argument",
            ErrorKind::Parse => "parse",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ErrorKind::InvalidArgument => "invalid argument",
            ErrorKind::Parse => "parse error",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Internal => "internal error",
        })
    }
}

/// An error returned by a sqlscope operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Creates an error; useful for bindings that validate their own input.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, message)
    }

    pub(crate) fn parse(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Parse, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// Returns a copy of the error whose message is prefixed with `context`.
    pub(crate) fn context(self, context: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            message: format!("{context}: {}", self.message),
        }
    }

    /// The error classification.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The human-readable message, without the kind prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}
