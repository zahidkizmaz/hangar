//! Where a proxy's CA comes from, and the checks it passes before a bay
//! trusts it.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::digest::{SHA256, digest};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;

use crate::config::Fields;
use crate::error::{Context, Result, bail};
use crate::files;
use crate::http;
use crate::json;

const CA_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) enum CaSource {
    File(PathBuf),
    Url { url: String, field: Option<String> },
}

pub(crate) struct CaSpec {
    pub(crate) source: CaSource,
    pub(crate) sha256: Option<String>,
}

impl CaSpec {
    pub(crate) fn parse(fields: &Fields, home: &Path) -> Result<Self> {
        fields.only(&["file", "url", "field", "sha256"])?;
        let field = fields.optional_string("field")?;
        let source = match (
            fields.optional_string("file")?,
            fields.optional_string("url")?,
        ) {
            (Some(file), None) if field.is_none() => {
                CaSource::File(files::host_path(&file, home)?)
            }
            (Some(_), None) => {
                bail!("{}: only applies to \"url\"", fields.at("field"))
            }
            (None, Some(url)) => CaSource::Url { url, field },
            (None, None) => {
                bail!("{}: set \"file\" or \"url\"", fields.path())
            }
            (Some(_), Some(_)) => {
                bail!("{}: set \"file\" or \"url\", not both", fields.path())
            }
        };
        Ok(Self {
            source,
            sha256: fields.optional_string("sha256")?,
        })
    }

    pub(crate) fn load(&self) -> Result<Vec<u8>> {
        let pem = match &self.source {
            CaSource::File(path) => fs::read(path).context(path.display())?,
            CaSource::Url { url, field } => {
                download(url, field.as_deref())?.into_bytes()
            }
        };
        verify(&pem, self.sha256.as_deref())?;
        Ok(pem)
    }
}

fn download(url: &str, field: Option<&str>) -> Result<String> {
    let response = http::fetch(url, CA_TIMEOUT)?;
    if response.status != 200 {
        bail!("{url}: HTTP {}", response.status);
    }
    let Some(field) = field else {
        return Ok(response.body);
    };
    let body = json::parse(&response.body).context(url)?;
    Fields::new(&body, url)?
        .optional_string(field)?
        .context(format!("{url}: no \"{field}\" field"))
}

fn der(pem: &[u8]) -> Result<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem)
        .next()
        .and_then(std::result::Result::ok)
        .context("CA isn't a PEM certificate")
}

/// Lowercase hex of the SHA-256 of the certificate's DER.
pub(crate) fn fingerprint(pem: &[u8]) -> Result<String> {
    let hash = digest(&SHA256, der(pem)?.as_ref());
    Ok(hash.as_ref().iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}

fn normalized(pin: &str) -> String {
    let hex = pin.strip_prefix("sha256:").unwrap_or(pin);
    hex.replace(':', "").to_lowercase()
}

/// Everything in the PEM reaches a bay, so the pin must cover all of it.
fn only_one_certificate(pem: &[u8]) -> Result<()> {
    let blocks = String::from_utf8_lossy(pem).matches("-----BEGIN ").count();
    let certificates = CertificateDer::pem_slice_iter(pem).count();
    if blocks != 1 || certificates != 1 {
        bail!("CA must be exactly one certificate");
    }
    Ok(())
}

pub(crate) fn verify(pem: &[u8], sha256: Option<&str>) -> Result<()> {
    let actual = fingerprint(pem)?;
    only_one_certificate(pem)?;
    match sha256.map(normalized) {
        Some(pinned) if pinned != actual => {
            bail!("CA sha256 {actual} doesn't match pinned {pinned}")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{scratch_dir, serve_each};

    const PEM: &str = include_str!("testdata/ca.pem");
    const FINGERPRINT: &str =
        "ae1ec4d78197cbd62b970e2e0d7afc3d5d9a66dbd47f3f39cd56f6fe98387a5f";

    fn parsed(text: &str, home: &Path) -> Result<CaSpec> {
        let value = json::parse(text).unwrap();
        CaSpec::parse(&Fields::new(&value, "proxies.p.ca")?, home)
    }

    fn colons(hex: &str) -> String {
        let pairs: Vec<&str> = (0..hex.len())
            .step_by(2)
            .map(|at| &hex[at..at + 2])
            .collect();
        pairs.join(":")
    }

    #[test]
    fn pin_matches_and_mismatches() {
        assert_eq!(fingerprint(PEM.as_bytes()).unwrap(), FINGERPRINT);
        verify(PEM.as_bytes(), None).unwrap();
        verify(PEM.as_bytes(), Some(FINGERPRINT)).unwrap();
        let shouty = colons(&FINGERPRINT.to_uppercase());
        verify(PEM.as_bytes(), Some(&shouty)).unwrap();
        let prefixed = format!("sha256:{shouty}");
        verify(PEM.as_bytes(), Some(&prefixed)).unwrap();
        let error = verify(PEM.as_bytes(), Some("AB:CD"))
            .err()
            .unwrap()
            .to_string();
        assert_eq!(
            error,
            format!("CA sha256 {FINGERPRINT} doesn't match pinned abcd")
        );
    }

    #[test]
    fn a_bundle_or_a_key_is_not_a_ca() {
        let key =
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        for pem in [format!("{PEM}{PEM}"), format!("{PEM}{key}")] {
            for pin in [None, Some(FINGERPRINT)] {
                let error =
                    verify(pem.as_bytes(), pin).err().unwrap().to_string();
                assert_eq!(error, "CA must be exactly one certificate");
            }
        }
    }

    #[test]
    fn a_non_pem_is_not_a_certificate() {
        for text in [&b"nope"[..], b""] {
            let error = verify(text, None).err().unwrap().to_string();
            assert_eq!(error, "CA isn't a PEM certificate");
            assert!(fingerprint(text).is_err());
        }
    }

    #[test]
    fn url_source_reads_a_json_field() {
        let body = json::stringify(&json::object([(
            "certificate",
            json::string(PEM),
        )]));
        let (port, server) = serve_each(vec![
            ("200 OK", body.clone().leak()),
            ("200 OK", body.leak()),
        ]);
        let url = format!("http://127.0.0.1:{port}/ca");
        let spec = |field: &str| CaSpec {
            source: CaSource::Url {
                url: url.clone(),
                field: Some(field.into()),
            },
            sha256: Some(FINGERPRINT.into()),
        };
        assert_eq!(spec("certificate").load().unwrap(), PEM.as_bytes());
        let error = spec("cert").load().err().unwrap().to_string();
        assert!(error.contains(r#"no "cert" field"#), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn url_source_without_a_field_is_the_body() {
        let (port, _) = serve_each(vec![("200 OK", PEM)]);
        let spec = CaSpec {
            source: CaSource::Url {
                url: format!("http://127.0.0.1:{port}/ca"),
                field: None,
            },
            sha256: None,
        };
        assert_eq!(spec.load().unwrap(), PEM.as_bytes());
    }

    #[test]
    fn url_source_refuses_an_error_status() {
        let (port, _) = serve_each(vec![("500 Oops", "down")]);
        let spec = CaSpec {
            source: CaSource::Url {
                url: format!("http://127.0.0.1:{port}/ca"),
                field: None,
            },
            sha256: None,
        };
        assert!(spec.load().err().unwrap().to_string().contains("HTTP 500"));
    }

    #[test]
    fn load_checks_the_pin() {
        let home = scratch_dir("ca-pin");
        let file = home.join("ca.pem");
        fs::write(&file, PEM).unwrap();
        let spec = CaSpec {
            source: CaSource::File(file),
            sha256: Some("00".into()),
        };
        let error = spec.load().err().unwrap().to_string();
        assert!(error.contains("doesn't match pinned 00"), "{error}");
    }

    #[test]
    fn https_or_loopback_only() {
        let error =
            crate::http::fetch("http://example.com/ca", Duration::from_secs(1))
                .err()
                .unwrap();
        assert_eq!(
            error.to_string(),
            "CA URL must be https, or http on 127.0.0.1"
        );
    }

    #[test]
    fn parse_reads_a_file_or_a_url() {
        let home = Path::new("/home/u");
        let spec = parsed(r#"{"file": "~/ca.pem"}"#, home).unwrap();
        assert!(matches!(
            spec.source,
            CaSource::File(path) if path == Path::new("/home/u/ca.pem")
        ));
        let spec = parsed(
            r#"{"url": "https://x/ca", "field": "certificate",
                "sha256": "ab"}"#,
            home,
        )
        .unwrap();
        assert_eq!(spec.sha256.as_deref(), Some("ab"));
        assert!(matches!(
            spec.source,
            CaSource::Url { url, field }
                if url == "https://x/ca"
                    && field.as_deref() == Some("certificate")
        ));
    }

    #[test]
    fn parse_needs_exactly_one_source() {
        let home = Path::new("/home/u");
        assert_eq!(
            parsed("{}", home).err().unwrap().to_string(),
            r#"proxies.p.ca: set "file" or "url""#
        );
        assert_eq!(
            parsed(r#"{"file": "/a", "url": "https://x"}"#, home)
                .err()
                .unwrap()
                .to_string(),
            r#"proxies.p.ca: set "file" or "url", not both"#
        );
        assert_eq!(
            parsed(r#"{"file": "/a", "field": "f"}"#, home)
                .err()
                .unwrap()
                .to_string(),
            r#"proxies.p.ca.field: only applies to "url""#
        );
        assert_eq!(
            parsed(r#"{"file": "/a", "bogus": 1}"#, home)
                .err()
                .unwrap()
                .to_string(),
            "proxies.p.ca.bogus: unknown setting"
        );
    }
}
