//! Helpers over miniserde's `Value`. Config stays a `Value` because
//! merging and unknown-key checks need the raw tree; fixed-shape
//! messages use miniserde's derive instead.

use miniserde::json::{self, Object};

use crate::error::{Context, Result};

pub(crate) use miniserde::json::Value as Json;

pub(crate) fn parse(text: &str) -> Result<Json> {
    json::from_str(text).context("invalid JSON")
}

/// Reads a derived message; unknown keys are ignored, missing `Option`
/// fields become `None`.
pub(crate) fn from_str<T: miniserde::Deserialize>(text: &str) -> Result<T> {
    json::from_str(text).context("unexpected JSON")
}

pub(crate) fn stringify<T: miniserde::Serialize>(value: &T) -> String {
    json::to_string(value)
}

pub(crate) fn string(text: &str) -> Json {
    Json::String(text.to_string())
}

pub(crate) fn object<const N: usize>(pairs: [(&str, Json); N]) -> Json {
    Json::Object(
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<Object>(),
    )
}

#[cfg(test)]
mod tests {
    use super::from_str;

    #[derive(miniserde::Deserialize)]
    struct Status {
        status: String,
        missing: Option<String>,
    }

    #[test]
    fn derived_messages_ignore_unknown_keys() {
        let status: Status =
            from_str(r#"{"status": "Running", "pid": 7}"#).unwrap();
        assert_eq!(status.status, "Running");
        assert_eq!(status.missing, None);
    }
}
