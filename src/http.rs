//! Just enough HTTP/1.1 for a broker's admin API on 127.0.0.1.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::error::{Context, Error, Result, bail};
use crate::secret::Secret;

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) token: Option<&'a Secret>,
    pub(crate) body: Option<&'a [u8]>,
}

pub(crate) fn send(
    port: u16,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    // Never the headers or body: they carry tokens and credential values.
    log::trace!("vault {} {}", request.method, request.path);
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream =
        TcpStream::connect_timeout(&address, timeout).context(address)?;
    stream.set_read_timeout(Some(timeout)).context(address)?;
    stream.set_write_timeout(Some(timeout)).context(address)?;

    let body = request.body.unwrap_or_default();
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n",
        request.method,
        request.path,
        body.len()
    );
    if let Some(token) = request.token {
        head = head + "Authorization: Bearer " + token.expose() + "\r\n";
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).context(address)?;
    stream.write_all(body).context(address)?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).context(address)?;
    parse(&raw)
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
pub(crate) fn call(port: u16, request: &Request) -> Result<Vec<u8>> {
    let response = send(port, request, Duration::from_secs(30))?;
    if !(200..300).contains(&response.status) {
        let detail = String::from_utf8_lossy(&response.body);
        bail!(
            "{} {}: HTTP {}: {}",
            request.method,
            request.path,
            response.status,
            detail.trim().chars().take(200).collect::<String>()
        );
    }
    Ok(response.body)
}

fn parse(raw: &[u8]) -> Result<Response> {
    let split = find(raw, b"\r\n\r\n").context("malformed HTTP response")?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| Error::new("malformed HTTP response head"))?;
    let rest = &raw[split + 4..];

    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .context("malformed HTTP status line")?;

    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse::<usize>().ok();
        }
    }

    let body = if chunked {
        dechunk(rest)?
    } else {
        let end = length.map_or(rest.len(), |n| n.min(rest.len()));
        rest[..end].to_vec()
    };
    Ok(Response { status, body })
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n").context("malformed chunk")?;
        let size_text = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| Error::new("malformed chunk size"))?;
        let size_text = size_text.split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|_| Error::new("malformed chunk size"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(..size).context("truncated chunk")?;
        body.extend_from_slice(chunk);
        rest = rest.get(size + 2..).context("truncated chunk")?;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let response = parse(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{\"ok\":true}");
    }

    #[test]
    fn chunked_body() {
        let raw =
            b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let response = parse(raw).unwrap();
        assert_eq!(response.status, 403);
        assert_eq!(response.body, b"{\"a\":1}");
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse(b"not http").is_err());
        assert!(parse(b"HTTP/1.1 abc\r\n\r\n").is_err());
    }
}
