use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::Read;

use crate::error::{Context, Result};

/// A credential that redacts itself in `Debug`. There is deliberately no
/// `Display`: formatting a secret must go through `expose()`, so a mistake
/// fails to compile instead of sending "[redacted]".
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// A secret read from a file or tool: one trailing newline (LF or CRLF) is
/// formatting, not part of the value.
pub(crate) fn trim_line_end(value: &str) -> &str {
    let value = value.strip_suffix('\n').unwrap_or(value);
    value.strip_suffix('\r').unwrap_or(value)
}

/// How real credentials start. Shared by every check that keeps secrets out
/// of a bay (`env`, `files`).
pub(crate) const SECRET_PREFIXES: [&str; 14] = [
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-ant-",
    "sk-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "ASIA",
];

/// What makes `text` look like it holds a credential: a private key, or a
/// token that starts like one and is long enough to be real (so prose such
/// as "risk-free" doesn't count).
pub(crate) fn find_secret(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    let token = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.split(|c: char| !token(c)).find_map(|word| {
        SECRET_PREFIXES.iter().find_map(|prefix| {
            let rest = word.strip_prefix(prefix)?;
            let real = if prefix.starts_with('A') {
                rest.len() == 16
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            } else {
                rest.len() >= 16
            };
            real.then_some(*prefix)
        })
    })
}

pub(crate) fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0; bytes];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut buffer))
        .context("/dev/urandom")?;
    Ok(buffer.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use super::{Secret, find_secret, random_hex, trim_line_end};

    #[test]
    fn real_looking_credentials_are_found_but_prose_is_not() {
        let token = format!("ghp_{}", "a".repeat(36));
        assert_eq!(find_secret(&format!("token = \"{token}\"")), Some("ghp_"));
        assert_eq!(
            find_secret("key: sk-ant-api03-abcdefghijklmnopqrstuv"),
            Some("sk-ant-")
        );
        assert_eq!(find_secret("id AKIAIOSFODNN7EXAMPLE end"), Some("AKIA"));
        // Assembled, so the repo holds no key-shaped literal.
        let key = format!("-----BEGIN OPENSSH {} KEY-----\nabc\n", "PRIVATE");
        assert_eq!(find_secret(&key), Some("a private key"));
        for prose in [
            "a risk-free task-based plan",
            "use sk- prefixed keys from the vault",
            "AKIA is the AWS key prefix",
            "-----BEGIN CERTIFICATE-----",
            "ghp_short",
        ] {
            assert_eq!(find_secret(prose), None, "{prose}");
        }
    }

    #[test]
    fn random_hex_has_two_digits_per_byte() {
        let hex = random_hex(16).unwrap();
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(hex, random_hex(16).unwrap());
    }

    #[test]
    fn debug_never_prints_the_value() {
        let secret = Secret::new("hunter2".into());
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn trailing_lf_and_crlf_are_trimmed() {
        assert_eq!(trim_line_end("token\r\n"), "token");
        assert_eq!(trim_line_end("token\n"), "token");
        assert_eq!(trim_line_end("a\nb\n"), "a\nb");
        assert_eq!(trim_line_end("token\n\n"), "token\n");
        assert_eq!(trim_line_end("token\r\r\n"), "token\r");
    }
}
