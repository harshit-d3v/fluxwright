use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Cdp(#[from] fluxwright_cdp::Error),
    #[error("admission queue is full")]
    QueueFull,
    #[error("timed out waiting to acquire a page lease")]
    AcquireTimeout,
    #[error("job timed out")]
    JobTimeout,
    #[error("engine is shut down")]
    ShuttingDown,
    #[error("no healthy browser available")]
    NoBrowser,
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Cdp(e) => e.is_retryable(),
            Error::NoBrowser => true,
            _ => false,
        }
    }
}
