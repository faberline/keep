//! The refusal error. Every refused check returns one of these.

use std::fmt;

/// A refused check. Its text is the line the receipt script printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GKE receipt refused: {}", self.0)
    }
}

impl std::error::Error for Refusal {}

pub type Result<T> = std::result::Result<T, Refusal>;

/// Refuse with `message`.
pub fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(Refusal(message.into()))
}
