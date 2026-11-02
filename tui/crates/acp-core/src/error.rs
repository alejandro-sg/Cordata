//! The one error type `acp-core` returns.

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("session is closed")]
    SessionClosed,

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
