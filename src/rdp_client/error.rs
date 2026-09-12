//! What a session failure is.

use std::fmt;

/// Why a session did not start, or did not continue.
///
/// A sentence and nothing else, because that is all a caller ever does with one:
/// show it. It is built from the whole cause chain — the step that failed, then what
/// it was doing, down to the field or the status the host actually objected to — so
/// the part a person can act on is not lost behind "RDP negotiation failed".
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    message: String,
}

impl Error {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

// Debug prints the same sentence rather than a struct dump: this ends up inside a
// `Result<(), Error>` in an `Event`, and `{:?}` on that is what most callers log.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Self {
        // `{:#}` is anyhow's own chain, `outer: inner: innermost`.
        Self { message: format!("{err:#}") }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context as _;

    use super::*;

    /// The reason is at the bottom of the chain, and it is the part worth reading.
    #[test]
    fn the_whole_chain_is_kept() {
        let inner: anyhow::Result<()> = Err(anyhow::anyhow!("the logon attempt failed"));
        let err = inner.context("CredSSP").context("RDP negotiation").unwrap_err();
        assert_eq!(
            Error::from(err).to_string(),
            "RDP negotiation: CredSSP: the logon attempt failed"
        );
    }
}
