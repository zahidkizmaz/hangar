//! HTTP through ureq: plain to the broker's admin API on 127.0.0.1, and
//! https only to OAuth providers.

use std::time::Duration;

use ureq::Agent;
use ureq::http::{self, header};

use crate::error::{Context, Result, bail};
use crate::secret::Secret;

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: String,
}

pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) token: Option<&'a Secret>,
    pub(crate) body: Option<&'a [u8]>,
}

const MAX_BODY: u64 = 1024 * 1024;

/// Trap: ureq's default config takes a proxy from the environment.
fn agent(timeout: Duration, https_only: bool) -> Agent {
    Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .https_only(https_only)
        .timeout_global(Some(timeout))
        .accept("application/json")
        .build()
        .into()
}

fn run(
    agent: &Agent,
    request: http::request::Builder,
    body: Option<&[u8]>,
) -> std::result::Result<Response, ureq::Error> {
    let mut response = match body {
        Some(body) => agent.run(request.body(body)?)?,
        None => agent.run(request.body(())?)?,
    };
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

pub(crate) fn send(
    port: u16,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    // Never the headers or body: they carry tokens and credential values.
    log::trace!("vault {} {}", request.method, request.path);
    let url = format!("http://127.0.0.1:{port}{}", request.path);
    let mut builder = http::Request::builder()
        .method(request.method)
        .uri(&url)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = request.token {
        builder = builder.header(
            header::AUTHORIZATION,
            format!("Bearer {}", token.expose()),
        );
    }
    let body = request.body.unwrap_or_default();
    run(&agent(timeout, false), builder, Some(body)).context(url)
}

/// A GET without a token, for probes like `/health`.
pub(crate) fn get(
    port: u16,
    path: &str,
    timeout: Duration,
) -> Result<Response> {
    let request = Request {
        method: "GET",
        path,
        token: None,
        body: None,
    };
    send(port, &request, timeout)
}

/// Like [`send`], but a non-2xx status is an error.
pub(crate) fn call(port: u16, request: &Request) -> Result<String> {
    let response = send(port, request, Duration::from_secs(30))?;
    if !(200..300).contains(&response.status) {
        bail!(
            "{} {}: HTTP {}: {}",
            request.method,
            request.path,
            response.status,
            response.body.trim().chars().take(200).collect::<String>()
        );
    }
    Ok(response.body)
}

pub(crate) trait Https {
    fn get(&self, url: &str) -> Result<Response>;
    fn post_json(&self, url: &str, body: &str) -> Result<Response>;
}

/// The OAuth providers' client.
pub(crate) fn https() -> Agent {
    agent(Duration::from_secs(30), true)
}

impl Https for Agent {
    fn get(&self, url: &str) -> Result<Response> {
        run(self, http::Request::get(url), None).context(url)
    }

    fn post_json(&self, url: &str, body: &str) -> Result<Response> {
        let request = http::Request::post(url)
            .header(header::CONTENT_TYPE, "application/json");
        run(self, request, Some(body.as_bytes())).context(url)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::{Https, Response};
    use crate::error::{Result, bail};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// Canned answers by URL; any other URL is a 404. Records each
    /// request, with a POST's body.
    #[derive(Default)]
    pub(crate) struct FakeHttps {
        pub(crate) answers: BTreeMap<String, (u16, String)>,
        pub(crate) requests: RefCell<Vec<String>>,
        pub(crate) fail: bool,
    }

    impl FakeHttps {
        pub(crate) fn answer(
            mut self,
            url: &str,
            status: u16,
            body: &str,
        ) -> Self {
            self.answers.insert(url.into(), (status, body.into()));
            self
        }

        fn respond(&self, request: String, url: &str) -> Result<Response> {
            self.requests.borrow_mut().push(request);
            if self.fail {
                bail!("{url}: could not resolve host");
            }
            let (status, body) = self
                .answers
                .get(url)
                .cloned()
                .unwrap_or((404, "Not Found".into()));
            Ok(Response { status, body })
        }
    }

    impl Https for FakeHttps {
        fn get(&self, url: &str) -> Result<Response> {
            self.respond(format!("GET {url}"), url)
        }

        fn post_json(&self, url: &str, body: &str) -> Result<Response> {
            self.respond(format!("POST {url} {body}"), url)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Https, MAX_BODY, agent, https};
    use crate::testing::{closed_port, serve_each};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn providers_get_https_only_without_a_proxy_redirect_or_long_wait() {
        let client = https();
        let config = client.config();
        assert!(config.https_only());
        assert!(config.proxy().is_none());
        assert_eq!(config.max_redirects(), 0);
        assert_eq!(config.timeouts().global, Some(Duration::from_secs(30)));
        let url = format!("http://127.0.0.1:{}/", closed_port());
        let error = Https::get(&client, &url).err().unwrap().to_string();
        assert!(error.contains("https only"), "{error}");
    }

    #[test]
    fn any_status_is_an_answer_and_a_redirect_is_never_followed() {
        let (port, server) = serve_each(vec![
            ("201 Created", r#"{"a":1}"#),
            ("302 Found\r\nLocation: /elsewhere", ""),
        ]);
        let client = agent(Duration::from_secs(5), false);
        let url = format!("http://127.0.0.1:{port}/register");
        let response = client.post_json(&url, r#"{"b":2}"#).unwrap();
        assert_eq!(
            (response.status, response.body.as_str()),
            (201, r#"{"a":1}"#)
        );
        assert_eq!(Https::get(&client, &url).unwrap().status, 302);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /register "));
        assert!(requests[0].contains("content-type: application/json\r\n"));
        assert!(requests[0].ends_with(r#"{"b":2}"#));
        assert!(requests[1].starts_with("GET /register "));
    }

    #[test]
    fn a_huge_or_slow_answer_fails_and_names_the_url() {
        let huge = "x".repeat(usize::try_from(MAX_BODY).unwrap() + 1);
        let (port, _) = serve_each(vec![("200 OK", huge.leak())]);
        let url = format!("http://127.0.0.1:{port}/huge");
        let error =
            Https::get(&agent(Duration::from_secs(5), false), &url).err();
        assert!(error.unwrap().to_string().starts_with(&url));

        let silent = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/slow");
        let error =
            Https::get(&agent(Duration::from_millis(200), false), &url).err();
        assert!(error.unwrap().to_string().starts_with(&url));
    }
}
