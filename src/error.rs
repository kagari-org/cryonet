use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CryonetError {
    #[error("connection error")]
    Connection,
    #[error("same id exists in network")]
    SameId,
    #[error("failed to verify token: {0}")]
    Token(String),
}
