//! What a session failure is.

use std::fmt;

/// Why a session did not start, or did not continue.
///
/// A sentence and nothing else, because that is all a caller ever does with one:
/// show it. It is built from the whole cause chain — IronRDP wraps the reason a
/// handshake failed (a refused credential, a CredSSP status, a TLS alert) inside
/// errors that only name the step — so the part a person can act on is not lost
/// behind "connect_finalize failed".
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    message: String,
}

impl Error {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// `doing`, then every error in `err`'s `source()` chain.
    pub(super) fn chain(doing: &str, err: &(dyn std::error::Error + 'static)) -> Self {
        let mut message = format!("{doing}: {err}");
        let mut source = err.source();
        while let Some(e) = source {
            let text = e.to_string();
            // A wrapper that already quotes its source would say it twice.
            if !message.contains(&text) {
                message.push_str(": ");
                message.push_str(&text);
            }
            source = e.source();
        }
        Self { message }
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
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Layer(&'static str, #[source] Option<Box<Layer>>);

    /// The reason is at the bottom of the chain, and it is the part worth reading.
    #[test]
    fn the_whole_chain_is_kept_and_repeats_are_not() {
        let inner = Layer("the logon attempt failed", None);
        let middle = Layer("CredSSP", Some(Box::new(inner)));
        let outer = Layer("connect_finalize", Some(Box::new(middle)));
        assert_eq!(
            Error::chain("RDP activation", &outer).to_string(),
            "RDP activation: connect_finalize: CredSSP: the logon attempt failed"
        );

        let quoting = Layer("TLS: bad certificate", Some(Box::new(Layer("bad certificate", None))));
        assert_eq!(Error::chain("upgrade", &quoting).to_string(), "upgrade: TLS: bad certificate");
    }
}
