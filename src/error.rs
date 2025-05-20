use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CryonetError {
    #[error("connection error")]
    Connection,
}
