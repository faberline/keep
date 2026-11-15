//! The refusal error: every rule that fails reports one of these.

/// A release rule refused the input. The display text is `GKE receipt refused: <message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GKE receipt refused: {}", self.0)
    }
}

impl std::error::Error for Refusal {}

pub type Result<T> = std::result::Result<T, Refusal>;

/// Return a refusal with `message`.
pub fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(Refusal(message.into()))
}
