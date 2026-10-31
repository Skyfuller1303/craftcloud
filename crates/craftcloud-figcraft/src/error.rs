use std::fmt;

/// Redacted adapter failure. Variants carry no server text, endpoint, key or link token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidEndpoint,
    InvalidKey,
    InvalidToken,
    InvalidIdentifier,
    InvalidDocument,
    UnsupportedArchive,
    RemoteUnavailable,
    Protocol,
    LimitExceeded,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEndpoint => "invalid FigCraft endpoint",
            Self::InvalidKey => "invalid FigCraft server key",
            Self::InvalidToken => "invalid FigCraft link token",
            Self::InvalidIdentifier => "invalid FigCraft identifier",
            Self::InvalidDocument => "invalid FigCraft document",
            Self::UnsupportedArchive => "unsupported FigCraft archive",
            Self::RemoteUnavailable => "FigCraft server unavailable",
            Self::Protocol => "FigCraft protocol error",
            Self::LimitExceeded => "FigCraft request exceeds adapter limits",
        })
    }
}

impl std::error::Error for Error {}
