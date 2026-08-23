use dg_xch_core::errors::ChiaError;
use dg_xch_stores::StoreError;
use std::error::Error;
use std::fmt;

#[derive(Debug)]
pub enum NodeError {
    Consensus(ChiaError),
    Store(StoreError),
    Io(std::io::Error),
    Orphan(String),
    Invalid(String),
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Consensus(e) => write!(f, "consensus rejected block: {e:?}"),
            NodeError::Store(e) => write!(f, "store error: {e}"),
            NodeError::Io(e) => write!(f, "io error: {e}"),
            NodeError::Orphan(s) => write!(f, "orphan block: {s}"),
            NodeError::Invalid(s) => write!(f, "invalid block: {s}"),
        }
    }
}

impl Error for NodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            NodeError::Store(e) => Some(e),
            NodeError::Io(e) => Some(e),
            NodeError::Consensus(_) | NodeError::Orphan(_) | NodeError::Invalid(_) => None,
        }
    }
}

impl From<ChiaError> for NodeError {
    fn from(e: ChiaError) -> Self {
        NodeError::Consensus(e)
    }
}

impl From<StoreError> for NodeError {
    fn from(e: StoreError) -> Self {
        NodeError::Store(e)
    }
}

impl From<std::io::Error> for NodeError {
    fn from(e: std::io::Error) -> Self {
        NodeError::Io(e)
    }
}
