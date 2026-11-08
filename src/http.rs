//! Plain GETs through ureq: a proxy's API on 127.0.0.1 and a proxy's CA.

use std::time::Duration;

use ureq::Agent;
use ureq::http;

use crate::error::{Context, Result, bail};

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: String,
}

const MAX_BODY: u64 = 1024 * 1024;

/// Trap: ureq's default config takes a proxy from the environment.
fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .accept("application/json")
        .build()
        .into()
}

fn run(agent: &Agent, url: &str) -> std::result::Result<Response, ureq::Error> {
    let mut response = agent.run(http::Request::get(url).body(())?)?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .lossy_utf8(true)
        .read_to_string()?;
    Ok(Response {
        status: response.status().as_u16(),
        body,
    })
}

pub(crate) fn get(port: u16, path: &str) -> Result<Response> {
    log::trace!("GET 127.0.0.1:{port}{path}");
    let url = format!("http://127.0.0.1:{port}{path}");
    run(&agent(Duration::from_secs(2)), &url).context(url)
}

pub(crate) fn http_authority(url: &str) -> Option<(String, Option<u16>)> {
    let uri: http::Uri = url.parse().ok()?;
    let authority = uri.authority()?;
    let plain = uri.scheme_str() == Some("http")
        && !authority.as_str().contains('@')
        && uri.query().is_none();
    plain.then(|| (authority.host().to_string(), authority.port_u16()))
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost")
}

pub(crate) fn fetch(url: &str, timeout: Duration) -> Result<Response> {
    let loopback =
        http_authority(url).is_some_and(|(host, _)| is_loopback_host(&host));
    if !loopback && !url.starts_with("https://") {
        bail!("CA URL must be https, or http on 127.0.0.1");
    }
    run(&agent(timeout), url).context(url)
}

#[cfg(test)]
mod tests {
    use super::{MAX_BODY, fetch, get, http_authority, is_loopback_host};
    use crate::testing::serve_each;
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn fetch_refuses_urls_that_only_look_like_loopback() {
        for url in [
            "http://127.0.0.1:80@evil.com/ca",
            "http://127.0.0.1.evil.com/ca",
            "http://user@127.0.0.1/ca",
            "http://example.com/ca",
        ] {
            let error = fetch(url, Duration::from_secs(1)).err().unwrap();
            assert!(error.to_string().starts_with("CA URL must be"), "{url}");
        }
        for url in ["http://127.0.0.1:8080/ca", "http://localhost:8080/ca"] {
            assert!(http_authority(url).is_some_and(|(host, port)| {
                is_loopback_host(&host) && port == Some(8080)
            }));
        }
    }

    #[test]
    fn any_status_is_an_answer_and_a_redirect_is_never_followed() {
        let (port, server) = serve_each(vec![
            ("201 Created", r#"{"a":1}"#),
            ("302 Found\r\nLocation: /elsewhere", ""),
        ]);
        let response = get(port, "/health").unwrap();
        assert_eq!(
            (response.status, response.body.as_str()),
            (201, r#"{"a":1}"#)
        );
        assert_eq!(get(port, "/health").unwrap().status, 302);
        let requests = server.join().unwrap();
        assert!(requests.iter().all(|r| r.starts_with("GET /health ")));
    }

    #[test]
    fn a_huge_or_slow_answer_fails_and_names_the_url() {
        let huge = "x".repeat(usize::try_from(MAX_BODY).unwrap() + 1);
        let (port, _) = serve_each(vec![("200 OK", huge.leak())]);
        let url = format!("http://127.0.0.1:{port}/huge");
        let error = fetch(&url, Duration::from_secs(5)).err();
        assert!(error.unwrap().to_string().starts_with(&url));

        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/slow");
        let error = fetch(&url, Duration::from_millis(200)).err();
        assert!(error.unwrap().to_string().starts_with(&url));
    }
}
