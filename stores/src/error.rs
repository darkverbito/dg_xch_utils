use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum StoreError {
    Backend(sqlx::Error),
    Io(std::io::Error),
    Corrupt(String),
    Batch(String),
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Backend(e) => write!(f, "backend error: {e}"),
            StoreError::Io(e) => write!(f, "io error: {e}"),
            StoreError::Corrupt(s) => write!(f, "corrupt store data: {s}"),
            StoreError::Batch(s) => write!(f, "batch error: {s}"),
        }
    }
}
impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StoreError::Backend(e) => Some(e),
            StoreError::Io(e) => Some(e),
            StoreError::Corrupt(_) | StoreError::Batch(_) => None,
        }
    }
}
impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        StoreError::Backend(e)
    }
}
impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}
