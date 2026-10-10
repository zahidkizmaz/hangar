//! Finding a provider's OAuth endpoints (RFC 9728, RFC 8414, `OpenID`
//! Connect) and registering hangar there as a public client (RFC 7591).
//! Every URL is checked before it's fetched or handed to the broker.

use crate::error::{Context, Error, Result, bail};
use crate::http::Https;
use crate::json::{self, Json};
use crate::url::with_param;

/// An `https://` URL whose host is a public name.
struct HttpsUrl {
    /// `https://host[:port]`.
    origin: String,
    /// Without query or fragment; empty for `/`.
    path: String,
}

impl HttpsUrl {
    fn whole(&self) -> String {
        format!("{}{}", self.origin, self.path)
    }
}

/// `https`, and a host name that can't be an address: no IP literal, no
/// `localhost`, and a last label that starts with a letter, so `127.1`
/// and `0x7f000001` are refused too: for what hangar fetches, opens and
/// hands to the broker.
pub(crate) fn check_url(url: &str) -> Result<()> {
    parse(url).map(drop)
}

fn parse(url: &str) -> Result<HttpsUrl> {
    let Some(rest) = url.strip_prefix("https://") else {
        bail!("{url}: expected an https:// URL");
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(host, _)| host)
        .to_ascii_lowercase();
    let last = host.rsplit('.').next().unwrap_or_default();
    let public = host.contains('.')
        && last.starts_with(|c: char| c.is_ascii_alphabetic())
        && !host.ends_with(".localhost")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c));
    if !public {
        bail!("{url}: expected a public host name");
    }
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    Ok(HttpsUrl {
        origin: format!("https://{authority}"),
        path: path.trim_end_matches('/').to_string(),
    })
}

/// What a login needs from the provider.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Provider {
    /// With `resource=` when the resource named itself (RFC 8707).
    pub(crate) authorization_url: String,
    pub(crate) token_url: String,
    registration_url: Option<String>,
    auth_methods: Option<Vec<String>>,
}

#[derive(miniserde::Deserialize)]
struct ResourceMetadata {
    resource: Option<String>,
    authorization_servers: Option<Vec<String>>,
}

#[derive(miniserde::Deserialize)]
struct ServerMetadata {
    issuer: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    code_challenge_methods_supported: Option<Vec<String>>,
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

const ENDPOINT_FLAGS: &str =
    "pass --authorization-url, --token-url and --client-id instead";

/// The provider behind `url`: the resource's own metadata names its
/// issuer, else `url`'s origin is the issuer.
pub(crate) fn discover(https: &dyn Https, url: &str) -> Result<Provider> {
    let target = parse(url)?;
    let resource = protected_resource(https, &target)?;
    let issuer = resource
        .as_ref()
        .map_or_else(|| target.origin.clone(), |(issuer, _)| issuer.clone());
    let mut provider = authorization_server(https, &issuer)?;
    if let Some((_, resource)) = resource {
        provider.authorization_url =
            with_param(&provider.authorization_url, "resource", &resource);
    }
    Ok(provider)
}

/// RFC 9728 §3: the path form names `url` itself, the root form the
/// origin. Anything but a 200 means there is none.
fn protected_resource(
    https: &dyn Https,
    target: &HttpsUrl,
) -> Result<Option<(String, String)>> {
    let well_known =
        format!("{}/.well-known/oauth-protected-resource", target.origin);
    let mut candidates =
        vec![(format!("{well_known}{}", target.path), target.whole())];
    if !target.path.is_empty() {
        candidates.push((well_known, target.origin.clone()));
    }
    for (url, expected) in candidates {
        let response = https.get(&url)?;
        if response.status != 200 {
            continue;
        }
        let metadata: ResourceMetadata =
            json::from_str(&response.body).context(&url)?;
        let resource = metadata.resource.unwrap_or_default();
        if resource.trim_end_matches('/') != expected {
            bail!("{url}: names resource {resource}, not {expected}");
        }
        let Some(issuer) = metadata
            .authorization_servers
            .and_then(|servers| servers.into_iter().next())
        else {
            bail!("{url}: names no authorization server");
        };
        check_url(&issuer)?;
        return Ok(Some((issuer, resource)));
    }
    Ok(None)
}

/// RFC 8414 §3, then `OpenID` Connect discovery.
fn authorization_server(https: &dyn Https, issuer: &str) -> Result<Provider> {
    let parsed = parse(issuer)?;
    let expected = parsed.whole();
    let candidates = [
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            parsed.origin, parsed.path
        ),
        format!("{expected}/.well-known/openid-configuration"),
    ];
    for url in candidates {
        let response = https.get(&url)?;
        if response.status == 200 {
            let metadata: ServerMetadata =
                json::from_str(&response.body).context(&url)?;
            return provider(&url, &expected, metadata);
        }
    }
    Err(Error::with_hint(
        format!("no OAuth metadata for {expected}"),
        ENDPOINT_FLAGS,
    ))
}

fn provider(
    url: &str,
    issuer: &str,
    metadata: ServerMetadata,
) -> Result<Provider> {
    let named = metadata.issuer.unwrap_or_default();
    if named.trim_end_matches('/') != issuer {
        bail!("{url}: names issuer {named}, not {issuer}");
    }
    if metadata
        .code_challenge_methods_supported
        .is_some_and(|methods| !methods.iter().any(|method| method == "S256"))
    {
        bail!("{url}: no PKCE with S256");
    }
    let endpoint = |value: Option<String>, name: &str| {
        let Some(value) = value else {
            bail!("{url}: no {name}");
        };
        check_url(&value)?;
        Ok(value)
    };
    let registration_url = metadata
        .registration_endpoint
        .map(|value| endpoint(Some(value), "registration_endpoint"))
        .transpose()?;
    Ok(Provider {
        authorization_url: endpoint(
            metadata.authorization_endpoint,
            "authorization_endpoint",
        )?,
        token_url: endpoint(metadata.token_endpoint, "token_endpoint")?,
        registration_url,
        auth_methods: metadata.token_endpoint_auth_methods_supported,
    })
}

/// Only these fields are read; a `registration_access_token` never is.
#[derive(miniserde::Deserialize)]
struct Registration {
    client_id: Option<String>,
    token_endpoint_auth_method: Option<String>,
    client_secret: Option<String>,
}

#[derive(miniserde::Deserialize)]
struct Refusal {
    error: Option<String>,
    error_description: Option<String>,
}

/// Registers hangar as a public client, which has no secret to keep:
/// anything else is refused with how to bring a client of one's own.
pub(crate) fn register(
    https: &dyn Https,
    provider: &Provider,
    redirect_uri: &str,
) -> Result<String> {
    let own_client = |why: String| {
        Error::with_hint(
            why,
            format!(
                "register a client with redirect URL {redirect_uri} and \
                 pass --client-id"
            ),
        )
    };
    let Some(endpoint) = &provider.registration_url else {
        return Err(own_client(
            "the provider has no client registration".into(),
        ));
    };
    if provider
        .auth_methods
        .as_ref()
        .is_some_and(|methods| !methods.iter().any(|method| method == "none"))
    {
        return Err(own_client(
            "the provider registers no public clients".into(),
        ));
    }
    let response =
        https.post_json(endpoint, &registration_body(redirect_uri))?;
    if !(200..300).contains(&response.status) {
        let refusal: Option<Refusal> = json::from_str(&response.body).ok();
        let reason = refusal
            .map(|refusal| {
                [refusal.error, refusal.error_description]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ")
            })
            .filter(|reason| !reason.is_empty())
            .map_or_else(String::new, |reason| format!(" ({reason})"));
        return Err(own_client(format!(
            "client registration at {endpoint} failed: HTTP {}{reason}",
            response.status
        )));
    }
    let registration: Registration =
        json::from_str(&response.body).context(endpoint)?;
    let public = registration
        .token_endpoint_auth_method
        .as_deref()
        .is_none_or(|method| method == "none")
        && registration.client_secret.is_none();
    if !public {
        return Err(own_client(
            "the provider registered a client with a secret".into(),
        ));
    }
    match registration.client_id {
        Some(id) if !id.is_empty() => Ok(id),
        _ => bail!("{endpoint}: registration returned no client_id"),
    }
}

fn registration_body(redirect_uri: &str) -> String {
    let strings = |values: &[&str]| {
        Json::Array(values.iter().map(|value| json::string(value)).collect())
    };
    json::stringify(&json::object([
        ("client_name", json::string("hangar")),
        ("redirect_uris", strings(&[redirect_uri])),
        (
            "grant_types",
            strings(&["authorization_code", "refresh_token"]),
        ),
        ("response_types", strings(&["code"])),
        ("token_endpoint_auth_method", json::string("none")),
    ]))
}

#[cfg(test)]
mod tests {
    use super::{Provider, check_url, discover, register};
    use crate::http::fake::FakeHttps;

    /// mcp.atlassian.com's answer, as fetched on 2026-10-10.
    const ATLASSIAN: &str = r#"{"issuer":"https://mcp.atlassian.com","authorization_endpoint":"https://mcp.atlassian.com/v1/authorize","token_endpoint":"https://mcp.atlassian.com/v1/token","registration_endpoint":"https://mcp.atlassian.com/v1/register","response_types_supported":["code"],"response_modes_supported":["query"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["client_secret_basic","client_secret_post","none"],"revocation_endpoint":"https://mcp.atlassian.com/v1/token","code_challenge_methods_supported":["plain","S256"]}"#;
    const ATLASSIAN_AS: &str =
        "https://mcp.atlassian.com/.well-known/oauth-authorization-server";
    const CALLBACK: &str = "http://127.0.0.1:14321/v1/oauth/callback";

    fn requests(https: &FakeHttps) -> Vec<String> {
        https.requests.borrow().clone()
    }

    #[test]
    fn atlassian_has_no_resource_metadata_so_its_origin_is_the_issuer() {
        let https = FakeHttps::default().answer(ATLASSIAN_AS, 200, ATLASSIAN);
        let provider =
            discover(&https, "https://mcp.atlassian.com/v1/mcp").unwrap();
        assert_eq!(
            provider,
            Provider {
                authorization_url: "https://mcp.atlassian.com/v1/authorize"
                    .into(),
                token_url: "https://mcp.atlassian.com/v1/token".into(),
                registration_url: Some(
                    "https://mcp.atlassian.com/v1/register".into()
                ),
                auth_methods: Some(vec![
                    "client_secret_basic".into(),
                    "client_secret_post".into(),
                    "none".into(),
                ]),
            }
        );
        assert_eq!(
            requests(&https),
            [
                "GET https://mcp.atlassian.com/.well-known/oauth-protected-resource/v1/mcp",
                "GET https://mcp.atlassian.com/.well-known/oauth-protected-resource",
                format!("GET {ATLASSIAN_AS}").as_str(),
            ]
        );
    }

    fn server(issuer: &str, extra: &str) -> String {
        format!(
            r#"{{"issuer":"{issuer}","authorization_endpoint":"https://id.example.com/auth","token_endpoint":"https://id.example.com/token"{extra}}}"#
        )
    }

    #[test]
    fn resource_metadata_names_the_issuer_and_the_resource() {
        let path_form =
            "https://api.example.com/.well-known/oauth-protected-resource/mcp";
        let resource = r#"{"resource":"https://api.example.com/mcp","authorization_servers":["https://id.example.com/tenant"]}"#;
        let issuer_meta = "https://id.example.com/.well-known/oauth-authorization-server/tenant";
        let https = FakeHttps::default()
            .answer(path_form, 200, resource)
            .answer(
                issuer_meta,
                200,
                &server("https://id.example.com/tenant/", ""),
            );
        let provider =
            discover(&https, "https://api.example.com/mcp/?x=1").unwrap();
        assert_eq!(
            provider.authorization_url,
            "https://id.example.com/auth?resource=https%3A%2F%2Fapi.example.com%2Fmcp"
        );
        assert_eq!(provider.registration_url, None);

        // The root form describes the origin; OpenID is the last resort.
        let root_form =
            "https://api.example.com/.well-known/oauth-protected-resource";
        let root = r#"{"resource":"https://api.example.com/","authorization_servers":["https://id.example.com"]}"#;
        let openid = "https://id.example.com/.well-known/openid-configuration";
        let https = FakeHttps::default().answer(root_form, 200, root).answer(
            openid,
            200,
            &server("https://id.example.com", ""),
        );
        let provider = discover(&https, "https://api.example.com/mcp").unwrap();
        assert!(
            provider
                .authorization_url
                .ends_with("?resource=https%3A%2F%2Fapi.example.com%2F")
        );
    }

    #[test]
    fn metadata_that_names_something_else_is_refused() {
        let error = |https: FakeHttps| {
            discover(&https, "https://api.example.com/mcp")
                .unwrap_err()
                .to_string()
        };
        let root_form =
            "https://api.example.com/.well-known/oauth-protected-resource";
        let wrong = r#"{"resource":"https://api.example.com/mcp","authorization_servers":["https://id.example.com"]}"#;
        assert_eq!(
            error(FakeHttps::default().answer(root_form, 200, wrong)),
            format!(
                "{root_form}: names resource https://api.example.com/mcp, not https://api.example.com"
            )
        );
        let none = r#"{"resource":"https://api.example.com"}"#;
        assert_eq!(
            error(FakeHttps::default().answer(root_form, 200, none)),
            format!("{root_form}: names no authorization server")
        );
        let private = r#"{"resource":"https://api.example.com","authorization_servers":["https://10.0.0.1"]}"#;
        assert_eq!(
            error(FakeHttps::default().answer(root_form, 200, private)),
            "https://10.0.0.1: expected a public host name"
        );

        let as_url =
            "https://api.example.com/.well-known/oauth-authorization-server";
        let answer =
            |body: &str| FakeHttps::default().answer(as_url, 200, body);
        assert_eq!(
            error(answer(&server("https://other.example.com", ""))),
            format!(
                "{as_url}: names issuer https://other.example.com, not https://api.example.com"
            )
        );
        let plain = server("https://api.example.com", "").replace(
            "https://id.example.com/token",
            "http://id.example.com/token",
        );
        assert_eq!(
            error(answer(&plain)),
            "http://id.example.com/token: expected an https:// URL"
        );
        let no_s256 = server(
            "https://api.example.com",
            r#","code_challenge_methods_supported":["plain"]"#,
        );
        assert_eq!(
            error(answer(&no_s256)),
            format!("{as_url}: no PKCE with S256")
        );
        let no_token = r#"{"issuer":"https://api.example.com","authorization_endpoint":"https://id.example.com/auth"}"#;
        assert_eq!(
            error(answer(no_token)),
            format!("{as_url}: no token_endpoint")
        );
        assert_eq!(
            error(FakeHttps::default()),
            "no OAuth metadata for https://api.example.com: pass \
             --authorization-url, --token-url and --client-id instead"
        );
        let down = FakeHttps {
            fail: true,
            ..FakeHttps::default()
        };
        assert!(error(down).contains("could not resolve host"));
    }

    #[test]
    fn only_public_https_host_names_pass() {
        for good in [
            "https://mcp.atlassian.com/v1/mcp",
            "https://Example.COM:8443/x?y#z",
            "https://xn--bcher-kva.example",
        ] {
            assert!(check_url(good).is_ok(), "{good}");
        }
        for bad in [
            "http://mcp.example.com",
            "https://localhost/x",
            "https://api.localhost",
            "https://127.0.0.1/",
            "https://127.1/",
            "https://0x7f000001/",
            "https://[::1]/",
            "https://intranet/",
            "https://user@example.com/",
            "https://example.com:x/",
            "https://",
        ] {
            assert!(check_url(bad).is_err(), "{bad}");
        }
    }

    fn atlassian() -> Provider {
        let https = FakeHttps::default().answer(ATLASSIAN_AS, 200, ATLASSIAN);
        discover(&https, "https://mcp.atlassian.com/v1/mcp").unwrap()
    }

    const REGISTER: &str = "https://mcp.atlassian.com/v1/register";

    #[test]
    fn hangar_registers_as_a_public_client_with_the_broker_callback() {
        let answer = r#"{"client_id":"dyn-1","token_endpoint_auth_method":"none","registration_access_token":"rat-secret","client_id_issued_at":1}"#;
        let https = FakeHttps::default().answer(REGISTER, 201, answer);
        assert_eq!(register(&https, &atlassian(), CALLBACK).unwrap(), "dyn-1");
        assert_eq!(
            requests(&https),
            [format!(
                r#"POST {REGISTER} {{"client_name":"hangar","grant_types":["authorization_code","refresh_token"],"redirect_uris":["{CALLBACK}"],"response_types":["code"],"token_endpoint_auth_method":"none"}}"#
            )]
        );
    }

    #[test]
    fn a_provider_without_public_registration_asks_for_a_client_id() {
        let hint = format!(
            ": register a client with redirect URL {CALLBACK} and pass --client-id"
        );
        let error = |provider: &Provider, https: &FakeHttps| {
            register(https, provider, CALLBACK).unwrap_err().to_string()
        };
        let none = FakeHttps::default();
        let mut provider = atlassian();
        provider.auth_methods = Some(vec!["client_secret_basic".into()]);
        assert_eq!(
            error(&provider, &none),
            format!("the provider registers no public clients{hint}")
        );
        provider.registration_url = None;
        assert_eq!(
            error(&provider, &none),
            format!("the provider has no client registration{hint}")
        );

        let answered = |status, body: &str| {
            let https = FakeHttps::default().answer(REGISTER, status, body);
            error(&atlassian(), &https)
        };
        assert_eq!(
            answered(
                400,
                r#"{"error":"invalid_redirect_uri","error_description":"http not allowed"}"#
            ),
            format!(
                "client registration at {REGISTER} failed: HTTP 400 \
                 (invalid_redirect_uri: http not allowed){hint}"
            )
        );
        assert_eq!(
            answered(500, "oops"),
            format!("client registration at {REGISTER} failed: HTTP 500{hint}")
        );
        let secret = r#"{"client_id":"c","client_secret":"s"}"#;
        let basic = r#"{"client_id":"c","token_endpoint_auth_method":"client_secret_basic"}"#;
        for body in [secret, basic] {
            assert_eq!(
                answered(201, body),
                format!("the provider registered a client with a secret{hint}")
            );
        }
        assert_eq!(
            answered(201, r#"{"client_id":""}"#),
            format!("{REGISTER}: registration returned no client_id")
        );
    }
}
