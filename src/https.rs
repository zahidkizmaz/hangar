//! HTTPS to OAuth providers through the host's curl: hangar has no TLS of
//! its own. Only public URLs go in argv; a request body goes on stdin.

use std::path::Path;

use crate::error::{Result, bail};
use crate::process;

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: String,
}

pub(crate) trait Https {
    fn get(&self, url: &str) -> Result<Response>;
    fn post_json(&self, url: &str, body: &str) -> Result<Response>;
}

/// Called by absolute path, like the keychain tool. The Nix package
/// bakes in its own curl.
const CURL: &str = match option_env!("HANGAR_CURL") {
    Some(path) => path,
    None => "/usr/bin/curl",
};

pub(crate) struct Curl {
    program: String,
}

impl Curl {
    pub(crate) fn new() -> Self {
        Self { program: program() }
    }
}

/// Only debug builds read `$HANGAR_TEST_CURL`, the CLI tests' fake;
/// release builds compile the lookup out (`nix/cli.nix` checks).
fn program() -> String {
    #[cfg(debug_assertions)]
    if let Some(fake) = crate::config::env_var("HANGAR_TEST_CURL") {
        return fake;
    }
    CURL.to_string()
}

/// `-q` must come first, or curl reads `~/.curlrc`. No `-L`: a redirect
/// is an answer, never followed.
fn args(url: &str, post: bool) -> Vec<&str> {
    let mut args = vec![
        "-q",
        "-sS",
        "--proto",
        "=https",
        "--noproxy",
        "*",
        "--max-time",
        "30",
        "--max-filesize",
        "1048576",
        "-H",
        "Accept: application/json",
        "-w",
        "\n%{http_code}",
    ];
    if post {
        args.extend([
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ]);
    }
    args.extend(["--url", url]);
    args
}

impl Curl {
    fn run(&self, url: &str, body: Option<&str>) -> Result<Response> {
        if !Path::new(&self.program).exists() {
            bail!(
                "{} not found: logging in with a URL needs curl",
                self.program
            );
        }
        let mut command = process::command(&self.program);
        command.args(args(url, body.is_some()));
        let output = process::output(&mut command, body.map(str::as_bytes))?;
        if !output.status.success() {
            bail!("{url}: {}", process::stderr(&output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some((body, status)) = stdout.rsplit_once('\n') else {
            bail!("{url}: no answer from curl");
        };
        let Ok(status) = status.trim().parse() else {
            bail!("{url}: no HTTP status from curl");
        };
        Ok(Response {
            status,
            body: body.to_string(),
        })
    }
}

impl Https for Curl {
    fn get(&self, url: &str) -> Result<Response> {
        self.run(url, None)
    }

    fn post_json(&self, url: &str, body: &str) -> Result<Response> {
        self.run(url, Some(body))
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
    use super::{CURL, Curl, Https, args, program};
    use crate::testing::scratch_dir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn fake_curl(name: &str, body: &str) -> (Curl, PathBuf) {
        let dir = scratch_dir(&format!("curl-{name}"));
        let tool = dir.join("curl");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"{0}/args\"\n\
             cat >\"{0}/stdin\"\n{body}\n",
            dir.display()
        );
        fs::write(&tool, script).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let curl = Curl {
            program: tool.display().to_string(),
        };
        (curl, dir)
    }

    #[test]
    fn curl_is_called_by_an_absolute_path() {
        assert!(CURL.starts_with('/'), "{CURL}");
        assert_eq!(program(), CURL);
        assert_eq!(Curl::new().program, CURL);
    }

    #[test]
    fn curl_reads_no_rc_file_follows_no_redirect_and_gets_no_body_in_argv() {
        let get = args("https://a.example/x", false);
        assert_eq!(get[0], "-q");
        assert!(!get.contains(&"-L") && !get.contains(&"--location"));
        assert!(get.windows(2).any(|pair| pair == ["--proto", "=https"]));
        assert_eq!(get[get.len() - 2..], ["--url", "https://a.example/x"]);
        assert!(!get.contains(&"--data-binary"));
        let post = args("https://a.example/x", true);
        assert!(post.windows(2).any(|pair| pair == ["--data-binary", "@-"]));
    }

    #[test]
    fn an_answer_is_split_into_status_and_body_and_a_post_body_goes_on_stdin() {
        let (curl, dir) = fake_curl("answer", r#"printf '{"a":1}\nline\n201'"#);
        let response = curl
            .post_json("https://a.example/r", r#"{"secret":1}"#)
            .unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, "{\"a\":1}\nline");
        let argv = fs::read_to_string(dir.join("args")).unwrap();
        assert!(!argv.contains("secret"), "{argv}");
        assert_eq!(
            fs::read_to_string(dir.join("stdin")).unwrap(),
            r#"{"secret":1}"#
        );
        let response = curl.get("https://a.example/g").unwrap();
        assert_eq!(fs::read_to_string(dir.join("stdin")).unwrap(), "");
        assert_eq!(response.status, 201);
    }

    #[test]
    fn curl_failures_name_the_url() {
        let (curl, _) =
            fake_curl("fails", "echo 'curl: (6) no such host' >&2; exit 6");
        let error = curl.get("https://a.example/").err().unwrap().to_string();
        assert_eq!(error, "https://a.example/: curl: (6) no such host");
        let (curl, _) = fake_curl("garbled", "printf 'no status'");
        let error = curl.get("https://a.example/").err().unwrap().to_string();
        assert_eq!(error, "https://a.example/: no answer from curl");
        let (curl, _) = fake_curl("not-a-status", "printf 'x\\nabc'");
        let error = curl.get("https://a.example/").err().unwrap().to_string();
        assert_eq!(error, "https://a.example/: no HTTP status from curl");
        let missing = Curl {
            program: "/nonexistent/curl".into(),
        };
        let error = missing.get("https://a.example/").err().unwrap();
        assert_eq!(
            error.to_string(),
            "/nonexistent/curl not found: logging in with a URL needs curl"
        );
    }
}
