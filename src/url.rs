//! The little URL handling OAuth logins need: query parameters.

use percent_encoding::{
    AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode,
};

/// Everything but RFC 3986's unreserved characters.
const RESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `url` with `name=value` added to its query.
pub(crate) fn with_param(url: &str, name: &str, value: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!(
        "{url}{separator}{name}={}",
        utf8_percent_encode(value, RESERVED)
    )
}

/// The decoded value of query parameter `name`, the first if repeated.
pub(crate) fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split_once('#').map_or(query, |(query, _)| query);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| form_decode(key) == name)
        .map(|(_, value)| form_decode(value))
}

/// `application/x-www-form-urlencoded`: `+` is a space, `%XX` a byte.
fn form_decode(text: &str) -> String {
    percent_decode_str(&text.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
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
