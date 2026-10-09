//! The little URL handling OAuth logins need: query parameters.

use std::fmt::Write as _;

/// `url` with `name=value` added to its query.
pub(crate) fn with_param(url: &str, name: &str, value: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{name}={}", percent_encode(value))
}

/// Everything but RFC 3986's unreserved characters, as `%XX`.
fn percent_encode(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
        out
    })
}

/// The decoded value of query parameter `name`, the first if repeated.
pub(crate) fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| percent_decode(key) == name)
        .map(|(_, value)| percent_decode(value))
}

/// `application/x-www-form-urlencoded`: `+` is a space, `%XX` a byte.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (byte, hex) {
            (b'%', Some(value)) => {
                decoded.push(value);
                index += 3;
            }
            (b'+', _) => {
                decoded.push(b' ');
                index += 1;
            }
            _ => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{query_param, with_param};

    #[test]
    fn a_param_is_encoded_and_reads_back() {
        let url = with_param(
            "https://a.example/authorize",
            "resource",
            "https://r.example/v1 mcp~",
        );
        assert_eq!(
            url,
            "https://a.example/authorize?resource=https%3A%2F%2Fr.example%2Fv1%20mcp~"
        );
        assert_eq!(
            query_param(&url, "resource").as_deref(),
            Some("https://r.example/v1 mcp~")
        );
        assert_eq!(with_param("https://a?x=1", "y", "2"), "https://a?x=1&y=2");
    }

    #[test]
    fn query_parameters_are_found_and_decoded() {
        let url = "https://a.example/authorize?client_id=c+1&\
                   redirect_uri=http%3A%2F%2F127.0.0.1%3A14321%2Fcb&x&\
                   state=s&state=t#redirect_uri=wrong";
        assert_eq!(query_param(url, "client_id").as_deref(), Some("c 1"));
        assert_eq!(
            query_param(url, "redirect_uri").as_deref(),
            Some("http://127.0.0.1:14321/cb")
        );
        assert_eq!(query_param(url, "state").as_deref(), Some("s"));
        assert_eq!(query_param(url, "x"), None);
        assert_eq!(query_param("https://a.example/", "state"), None);
        assert_eq!(
            query_param("https://a?k=%zz%4", "k").as_deref(),
            Some("%zz%4")
        );
    }
}
