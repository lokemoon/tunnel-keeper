// One error type for the whole core. `kind` carries aws.js's classification
// ('expired', 'unreachable', ...) and `conflict` turns an error into the
// NeedsConfirmation question that ipc.rs reports as data rather than as a failure.

use crate::ports::Conflict;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub kind: Option<&'static str>,
    pub conflict: Option<Box<Conflict>>,
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error { message: message.into(), kind: None, conflict: None }
    }

    pub fn kind(message: impl Into<String>, kind: &'static str) -> Self {
        Error { message: message.into(), kind: Some(kind), conflict: None }
    }

    // Thrown when the port is held by a process this app did not start. Carries the
    // details the UI needs to ask the user before killing anything.
    pub fn needs_confirmation(conflict: Conflict) -> Self {
        Error { message: conflict.message.clone(), kind: None, conflict: Some(Box::new(conflict)) }
    }

    pub fn needs_confirmation_flag(&self) -> bool {
        self.conflict.is_some()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
