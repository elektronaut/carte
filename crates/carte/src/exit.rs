//! The exit codes every command ends with.

use std::process::ExitCode;

use carte_core::address::AddressError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    /// Anything without a code of its own.
    Error = 1,
    /// Invalid arguments.
    Usage = 2,
    /// The daemon isn't running, or is starting or stopping.
    DaemonDown = 3,
    /// The address names nothing, or more than one thing, or there is no app in the
    /// current directory.
    NotFound = 4,
    /// An app failed to start or didn't get ready in time.
    NotReady = 5,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// An error that ends the command with a specific exit code.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Failure {
            exit,
            message: message.into(),
        }
    }
}

impl From<AddressError> for Failure {
    fn from(err: AddressError) -> Self {
        Failure::new(Exit::NotFound, err.to_string())
    }
}

/// The exit code for an error: the first `Failure` in its chain, otherwise `Error`.
pub fn code(err: &anyhow::Error) -> Exit {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<Failure>())
        .map_or(Exit::Error, |failure| failure.exit)
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn finds_the_code_through_context() {
        let err = anyhow::Error::new(Failure::new(Exit::NotFound, "no app")).context("looking");
        assert_eq!(code(&err), Exit::NotFound);
        let plain: anyhow::Result<()> = Err(anyhow::anyhow!("boom")).context("doing");
        assert_eq!(code(&plain.unwrap_err()), Exit::Error);
    }

    #[test]
    fn address_errors_mean_not_found() {
        assert_eq!(Failure::from(AddressError::NoContext).exit, Exit::NotFound);
    }
}
