use std::fmt;

/// A message plus, when there's an obvious next step, a hint. Humans see
/// `message: hint`; `--json` keeps them apart.
#[derive(Debug)]
pub(crate) struct Error {
    message: String,
    hint: Option<String>,
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn new(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            hint: None,
        }
    }

    pub(crate) fn with_hint(
        message: impl fmt::Display,
        hint: impl fmt::Display,
    ) -> Self {
        Self {
            message: message.to_string(),
            hint: Some(hint.to_string()),
        }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, ": {hint}")?;
        }
        Ok(())
    }
}

/// `.context("…")` for results and options, prefixing the cause.
pub(crate) trait Context<T> {
    fn context(self, message: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.map_err(|error| Error::new(format!("{message}: {error}")))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, message: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| Error::new(message))
    }
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::error::Error::new(format!($($arg)*)))
    };
}
pub(crate) use bail;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn a_hint_follows_the_message_for_humans() {
        let error = Error::with_hint("the vault isn't running", "run 'up'");
        assert_eq!(error.to_string(), "the vault isn't running: run 'up'");
        assert_eq!(error.message(), "the vault isn't running");
        assert_eq!(error.hint(), Some("run 'up'"));
        assert_eq!(Error::new("plain").hint(), None);
    }
}
