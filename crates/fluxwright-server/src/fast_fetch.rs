//! DNS-pinned, SSRF-safe async HTTP/1.1 + HTTPS fetcher for the Tier 1 scraping fast path.
//!
//! Validates every URL with [`fluxwright::netguard`] and pins DNS resolution before connecting
//! directly to a validated [`SocketAddr`], repeating all checks on every 3xx redirect hop (up to
//! 10 hops) while preserving TLS SNI and `Host` headers.

use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use base64::Engine as _;
use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};
use fluxwright::{netguard, Proxy, ProxyPool};
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use url::{Host, Url};

use crate::http_post::{ip_allowed, tls_client_config};

pub(crate) const MAX_FAST_FETCH_BODY_BYTES: usize = 10 * 1024 * 1024;
pub(crate) const MAX_REDIRECT_HOPS: usize = 10;
const MAX_HEADER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FetchMode {
    #[default]
    Auto,
    Http,
    Browser,
}

#[derive(Debug, Clone)]
pub(crate) struct FastFetchResponse {
    pub(crate) final_url: String,
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) enum FastFetchError {
    InvalidOrBlocked(String),
    Transport(String),
}

pub(crate) async fn fetch_url(
    raw_url: &str,
    timeout: Duration,
    allow_any_url: bool,
    proxy_pool: Option<&ProxyPool>,
    proxy_session: Option<&str>,
) -> Result<FastFetchResponse, FastFetchError> {
    let initial = parse_http_url(raw_url, allow_any_url)?;
    let proxy_lease = proxy_pool.map(|pool| pool.acquire(proxy_session));

    let work = async {
        let mut current = initial;
        let mut hops = 0usize;
        loop {
            let proxy = proxy_lease.as_ref().and_then(|lease| {
                let p = lease.proxy();
                let host = url_host_no_brackets(&current);
                if proxy_bypasses_host(p.bypass.as_deref(), &host) {
                    None
                } else {
                    Some(p)
                }
            });

            let resp = fetch_single_hop(&current, allow_any_url, proxy)
                .await
                .map_err(|e| {
                    if let (Some(lease), true) = (&proxy_lease, proxy.is_some()) {
                        if matches!(e, FastFetchError::Transport(_)) {
                            lease.report_outcome(false);
                        }
                    }
                    e
                })?;

            if let (Some(lease), true) = (&proxy_lease, proxy.is_some()) {
                lease.report_outcome(true);
            }

            if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
                if let Some(location) = find_header_ci(&resp.headers, "location") {
                    if hops >= MAX_REDIRECT_HOPS {
                        return Err(FastFetchError::Transport(format!(
                            "exceeded maximum of {MAX_REDIRECT_HOPS} redirect hops"
                        )));
                    }
                    hops += 1;
                    let mut next = current.join(location.trim()).map_err(|e| {
                        FastFetchError::InvalidOrBlocked(format!("invalid redirect location: {e}"))
                    })?;
                    next.set_fragment(None);
                    validate_parsed_url(&next, allow_any_url)?;
                    current = next;
                    continue;
                }
            }

            let content_type = find_header_ci(&resp.headers, "content-type").map(str::to_string);
            let content_encoding =
                find_header_ci(&resp.headers, "content-encoding").map(str::to_string);
            let decoded_body = decode_content_encoding(resp.raw_body, content_encoding.as_deref())?;

            return Ok(FastFetchResponse {
                final_url: current.to_string(),
                status: resp.status,
                headers: resp.headers,
                content_type,
                body: decoded_body,
            });
        }
    };

    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| FastFetchError::Transport("HTTP fast-path request timed out".into()))?
}

struct HopResponse {
    status: u16,
    headers: Vec<(String, String)>,
    raw_body: Vec<u8>,
}

fn parse_http_url(raw: &str, allow_any_url: bool) -> Result<Url, FastFetchError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(FastFetchError::InvalidOrBlocked(
            "URL must be non-empty without whitespace or control characters".into(),
        ));
    }
    if !allow_any_url {
        netguard::url_allowed(trimmed)
            .map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
    }
    let mut parsed = Url::parse(trimmed)
        .map_err(|e| FastFetchError::InvalidOrBlocked(format!("invalid URL: {e}")))?;
    parsed.set_fragment(None);
    validate_parsed_url(&parsed, allow_any_url)?;
    Ok(parsed)
}

fn validate_parsed_url(url: &Url, allow_any_url: bool) -> Result<(), FastFetchError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(FastFetchError::InvalidOrBlocked(
            "only http and https URLs are allowed".into(),
        ));
    }
    if url.host().is_none() {
        return Err(FastFetchError::InvalidOrBlocked(
            "URL is missing host".into(),
        ));
    }
    if !allow_any_url {
        netguard::url_allowed(url.as_str())
            .map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
        if let Some(Host::Domain(domain)) = url.host() {
            if let Some(v4) = netguard::ipv4_host(domain) {
                ip_allowed(IpAddr::V4(v4))
                    .map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
            }
        } else if let Some(Host::Ipv4(v4)) = url.host() {
            ip_allowed(IpAddr::V4(v4))
                .map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
        } else if let Some(Host::Ipv6(v6)) = url.host() {
            ip_allowed(IpAddr::V6(v6))
                .map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
        }
    }
    Ok(())
}

fn url_host_no_brackets(url: &Url) -> String {
    match url.host() {
        Some(Host::Domain(d)) => d.to_string(),
        Some(Host::Ipv4(v4)) => v4.to_string(),
        Some(Host::Ipv6(v6)) => v6.to_string(),
        None => String::new(),
    }
}

fn format_host_header(url: &Url) -> String {
    let host_part = match url.host() {
        Some(Host::Domain(d)) => d.to_string(),
        Some(Host::Ipv4(v4)) => v4.to_string(),
        Some(Host::Ipv6(v6)) => format!("[{v6}]"),
        None => String::new(),
    };
    match url.port() {
        Some(p) => format!("{host_part}:{p}"),
        None => host_part,
    }
}

fn format_connect_authority(url: &Url) -> String {
    let port = url.port_or_known_default().unwrap_or(80);
    let host_part = match url.host() {
        Some(Host::Domain(d)) => d.to_string(),
        Some(Host::Ipv4(v4)) => v4.to_string(),
        Some(Host::Ipv6(v6)) => format!("[{v6}]"),
        None => String::new(),
    };
    format!("{host_part}:{port}")
}

fn path_and_query(url: &Url) -> String {
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    match url.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    }
}

fn sanitize_absolute_target(url: &Url) -> String {
    let mut clean = url.clone();
    clean.set_fragment(None);
    let _ = clean.set_username("");
    let _ = clean.set_password(None);
    clean.to_string()
}

async fn resolve_pinned_addrs(
    url: &Url,
    allow_any_url: bool,
) -> Result<Vec<SocketAddr>, FastFetchError> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| FastFetchError::InvalidOrBlocked("missing target port".into()))?;
    let addrs = match url.host() {
        Some(Host::Ipv4(v4)) => vec![SocketAddr::new(IpAddr::V4(v4), port)],
        Some(Host::Ipv6(v6)) => vec![SocketAddr::new(IpAddr::V6(v6), port)],
        Some(Host::Domain(domain)) => tokio::net::lookup_host((domain, port))
            .await
            .map_err(|e| FastFetchError::Transport(format!("DNS lookup failed: {e}")))?
            .collect(),
        None => {
            return Err(FastFetchError::InvalidOrBlocked(
                "URL is missing host".into(),
            ))
        }
    };
    if addrs.is_empty() {
        return Err(FastFetchError::Transport(
            "DNS resolution returned no addresses".into(),
        ));
    }
    if !allow_any_url {
        for addr in &addrs {
            ip_allowed(addr.ip()).map_err(|e| FastFetchError::InvalidOrBlocked(e.to_string()))?;
        }
    }
    Ok(addrs)
}

async fn connect_pinned(addrs: &[SocketAddr]) -> Result<TcpStream, FastFetchError> {
    let mut last_err = None;
    for &addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(FastFetchError::Transport(
        last_err
            .map(|e| format!("TCP connect failed: {e}"))
            .unwrap_or_else(|| "could not connect to target host".into()),
    ))
}

async fn connect_tls(
    stream: TcpStream,
    url: &Url,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, FastFetchError> {
    let host_str = url_host_no_brackets(url);
    let server_name = ServerName::try_from(host_str.clone()).map_err(|_| {
        FastFetchError::InvalidOrBlocked(format!("invalid TLS server name: {host_str}"))
    })?;
    let connector = TlsConnector::from(tls_client_config().clone());
    connector
        .connect(server_name, stream)
        .await
        .map_err(|e| FastFetchError::Transport(format!("TLS handshake failed: {e}")))
}

async fn fetch_single_hop(
    url: &Url,
    allow_any_url: bool,
    proxy: Option<&Proxy>,
) -> Result<HopResponse, FastFetchError> {
    let is_tls = url.scheme() == "https";
    let host_header = format_host_header(url);

    if let Some(p) = proxy {
        let proxy_raw = p.server.trim();
        let proxy_url_str = if proxy_raw.contains("://") {
            proxy_raw.to_string()
        } else {
            format!("http://{proxy_raw}")
        };
        let proxy_url = Url::parse(&proxy_url_str).map_err(|e| {
            FastFetchError::Transport(format!("invalid proxy URL `{proxy_raw}`: {e}"))
        })?;
        if !proxy_url.scheme().eq_ignore_ascii_case("http") {
            return Err(FastFetchError::Transport(format!(
                "fast path only supports http:// proxies, got `{}`",
                p.server
            )));
        }
        let proxy_host = url_host_no_brackets(&proxy_url);
        let proxy_port = proxy_url.port_or_known_default().unwrap_or(80);
        if proxy_host.is_empty() {
            return Err(FastFetchError::Transport("proxy URL missing host".into()));
        }

        let proxy_auth_header = p
            .username
            .as_deref()
            .map(|u| basic_auth_header(u, p.password.as_deref()));

        let mut tcp = TcpStream::connect((proxy_host.as_str(), proxy_port))
            .await
            .map_err(|e| FastFetchError::Transport(format!("proxy connect failed: {e}")))?;

        if is_tls {
            let connect_auth = format_connect_authority(url);
            let mut connect_req =
                format!("CONNECT {connect_auth} HTTP/1.1\r\nHost: {connect_auth}\r\n");
            if let Some(auth) = &proxy_auth_header {
                if auth.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                    return Err(FastFetchError::Transport(
                        "invalid characters in Proxy-Authorization".into(),
                    ));
                }
                connect_req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
            }
            connect_req.push_str("\r\n");
            tcp.write_all(connect_req.as_bytes()).await.map_err(|e| {
                FastFetchError::Transport(format!("proxy CONNECT write failed: {e}"))
            })?;
            tcp.flush().await.map_err(|e| {
                FastFetchError::Transport(format!("proxy CONNECT flush failed: {e}"))
            })?;

            let (code, reason) = read_connect_status(&mut tcp).await?;
            if !(200..300).contains(&code) {
                return Err(FastFetchError::Transport(format!(
                    "proxy CONNECT rejected target: {code} {reason}"
                )));
            }
            let mut tls = connect_tls(tcp, url).await?;
            let target = path_and_query(url);
            let req = build_get_request(&target, &host_header, None)?;
            send_and_read(&mut tls, req.as_bytes()).await
        } else {
            let target = sanitize_absolute_target(url);
            let req = build_get_request(&target, &host_header, proxy_auth_header.as_deref())?;
            send_and_read(&mut tcp, req.as_bytes()).await
        }
    } else {
        let addrs = resolve_pinned_addrs(url, allow_any_url).await?;
        let mut tcp = connect_pinned(&addrs).await?;
        let target = path_and_query(url);
        let req = build_get_request(&target, &host_header, None)?;
        if is_tls {
            let mut tls = connect_tls(tcp, url).await?;
            send_and_read(&mut tls, req.as_bytes()).await
        } else {
            send_and_read(&mut tcp, req.as_bytes()).await
        }
    }
}

fn build_get_request(
    target: &str,
    host_header: &str,
    proxy_auth: Option<&str>,
) -> Result<String, FastFetchError> {
    if target.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
        || host_header.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
    {
        return Err(FastFetchError::InvalidOrBlocked(
            "invalid characters in request target or Host header".into(),
        ));
    }
    let mut head = format!(
        "GET {target} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: Fluxwright/0.13\r\n\
         Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\n\
         Accept-Encoding: gzip, deflate\r\n\
         Accept-Language: en-US,en;q=0.9\r\n\
         Connection: close\r\n"
    );
    if let Some(auth) = proxy_auth {
        if auth.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(FastFetchError::Transport(
                "invalid characters in Proxy-Authorization".into(),
            ));
        }
        head.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    head.push_str("\r\n");
    Ok(head)
}

async fn read_connect_status(stream: &mut TcpStream) -> Result<(u16, String), FastFetchError> {
    let mut raw = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| FastFetchError::Transport(format!("proxy CONNECT read failed: {e}")))?;
        if n == 0 {
            return Err(FastFetchError::Transport(
                "proxy closed connection during CONNECT".into(),
            ));
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            let block = std::str::from_utf8(&raw).map_err(|_| {
                FastFetchError::Transport("non-UTF-8 proxy CONNECT response".into())
            })?;
            let first = block.split("\r\n").next().unwrap_or("");
            return parse_status_line(first);
        }
        if raw.len() > 16 * 1024 {
            return Err(FastFetchError::Transport(
                "proxy CONNECT response headers too large".into(),
            ));
        }
    }
}

async fn send_and_read<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    head: &[u8],
) -> Result<HopResponse, FastFetchError> {
    stream
        .write_all(head)
        .await
        .map_err(|e| FastFetchError::Transport(format!("HTTP write failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| FastFetchError::Transport(format!("HTTP flush failed: {e}")))?;

    let (status, headers, mut leftover) = read_response_headers(stream).await?;

    // Redirect responses do not need their body read when Location is present.
    if matches!(status, 301 | 302 | 303 | 307 | 308)
        && find_header_ci(&headers, "location").is_some()
    {
        return Ok(HopResponse {
            status,
            headers,
            raw_body: Vec::new(),
        });
    }

    if matches!(status, 204 | 304) || (100..200).contains(&status) {
        return Ok(HopResponse {
            status,
            headers,
            raw_body: Vec::new(),
        });
    }

    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });

    let mut tmp = [0u8; 16 * 1024];

    if is_chunked {
        loop {
            if let Some(decoded) = try_decode_chunked_complete(&leftover) {
                return Ok(HopResponse {
                    status,
                    headers,
                    raw_body: decoded?,
                });
            }
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| FastFetchError::Transport(format!("HTTP body read failed: {e}")))?;
            if n == 0 {
                let decoded = decode_chunked_body(&leftover)?;
                return Ok(HopResponse {
                    status,
                    headers,
                    raw_body: decoded,
                });
            }
            // Allow modest framing overhead alongside MAX_FAST_FETCH_BODY_BYTES before incremental decode.
            if leftover.len().saturating_add(n) > MAX_FAST_FETCH_BODY_BYTES + 1024 * 1024 {
                return Err(FastFetchError::Transport(
                    "HTTP chunked response exceeds 10 MiB limit".into(),
                ));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
    }

    if let Some(len_str) = find_header_ci(&headers, "content-length") {
        let expected = len_str.trim().parse::<usize>().map_err(|_| {
            FastFetchError::Transport(format!("invalid Content-Length `{len_str}`"))
        })?;
        if expected > MAX_FAST_FETCH_BODY_BYTES {
            return Err(FastFetchError::Transport(
                "HTTP response body exceeds 10 MiB limit".into(),
            ));
        }
        while leftover.len() < expected {
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| FastFetchError::Transport(format!("HTTP body read failed: {e}")))?;
            if n == 0 {
                return Err(FastFetchError::Transport(
                    "unexpected EOF reading HTTP response body".into(),
                ));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
        leftover.truncate(expected);
        return Ok(HopResponse {
            status,
            headers,
            raw_body: leftover,
        });
    }

    if leftover.len() > MAX_FAST_FETCH_BODY_BYTES {
        return Err(FastFetchError::Transport(
            "HTTP response body exceeds 10 MiB limit".into(),
        ));
    }
    loop {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| FastFetchError::Transport(format!("HTTP body read failed: {e}")))?;
        if n == 0 {
            return Ok(HopResponse {
                status,
                headers,
                raw_body: leftover,
            });
        }
        if leftover.len().saturating_add(n) > MAX_FAST_FETCH_BODY_BYTES {
            return Err(FastFetchError::Transport(
                "HTTP response body exceeds 10 MiB limit".into(),
            ));
        }
        leftover.extend_from_slice(&tmp[..n]);
    }
}

async fn read_response_headers<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), FastFetchError> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(FastFetchError::Transport(
                "HTTP response headers too large".into(),
            ));
        }
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| FastFetchError::Transport(format!("HTTP header read failed: {e}")))?;
        if n == 0 {
            return Err(FastFetchError::Transport(
                "unexpected EOF before HTTP response headers".into(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| FastFetchError::Transport("non-UTF-8 HTTP response headers".into()))?;
    let (mut status, mut headers) = parse_headers_block(header_str)?;
    let mut leftover = buf[header_end + 4..].to_vec();

    while (100..200).contains(&status) && status != 101 {
        while !leftover.windows(4).any(|w| w == b"\r\n\r\n") {
            if leftover.len() > MAX_HEADER_BYTES {
                return Err(FastFetchError::Transport(
                    "HTTP response headers too large".into(),
                ));
            }
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| FastFetchError::Transport(format!("HTTP header read failed: {e}")))?;
            if n == 0 {
                return Err(FastFetchError::Transport(
                    "unexpected EOF after 1xx HTTP response".into(),
                ));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
        let pos = leftover.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let next_str = std::str::from_utf8(&leftover[..pos])
            .map_err(|_| FastFetchError::Transport("non-UTF-8 HTTP response headers".into()))?;
        (status, headers) = parse_headers_block(next_str)?;
        leftover = leftover[pos + 4..].to_vec();
    }

    Ok((status, headers, leftover))
}

fn parse_status_line(line: &str) -> Result<(u16, String), FastFetchError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(FastFetchError::Transport(format!(
            "unexpected HTTP status line `{line}`"
        )));
    }
    let code_str = parts.next().ok_or_else(|| {
        FastFetchError::Transport(format!("missing HTTP status code in `{line}`"))
    })?;
    let status = code_str
        .parse::<u16>()
        .map_err(|_| FastFetchError::Transport(format!("invalid HTTP status code `{code_str}`")))?;
    let reason = parts.next().unwrap_or("").trim().to_string();
    Ok((status, reason))
}

fn parse_headers_block(block: &str) -> Result<(u16, Vec<(String, String)>), FastFetchError> {
    let mut lines = block.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| FastFetchError::Transport("empty HTTP response".into()))?;
    let (status, _) = parse_status_line(first)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn try_decode_chunked_complete(raw: &[u8]) -> Option<Result<Vec<u8>, FastFetchError>> {
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let rel_crlf = raw[pos..].windows(2).position(|w| w == b"\r\n")?;
        let size_line = match std::str::from_utf8(&raw[pos..pos + rel_crlf]) {
            Ok(s) => s,
            Err(_) => {
                return Some(Err(FastFetchError::Transport(
                    "invalid HTTP chunk size encoding".into(),
                )))
            }
        };
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_hex, 16) {
            Ok(s) => s,
            Err(_) => {
                return Some(Err(FastFetchError::Transport(format!(
                    "invalid HTTP chunk size `{size_hex}`"
                ))))
            }
        };
        pos += rel_crlf + 2;
        if size == 0 {
            if raw[pos..].starts_with(b"\r\n") || raw[pos..].windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(Ok(out));
            }
            return None;
        }
        if out.len().saturating_add(size) > MAX_FAST_FETCH_BODY_BYTES {
            return Some(Err(FastFetchError::Transport(
                "HTTP response body exceeds 10 MiB limit".into(),
            )));
        }
        if raw.len() - pos < size + 2 {
            return None;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size;
        if &raw[pos..pos + 2] != b"\r\n" {
            return Some(Err(FastFetchError::Transport(
                "missing CRLF after HTTP chunk data".into(),
            )));
        }
        pos += 2;
    }
}

fn decode_chunked_body(raw: &[u8]) -> Result<Vec<u8>, FastFetchError> {
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let Some(rel_crlf) = raw[pos..].windows(2).position(|w| w == b"\r\n") else {
            return Err(FastFetchError::Transport(
                "incomplete chunked HTTP response".into(),
            ));
        };
        let size_line = std::str::from_utf8(&raw[pos..pos + rel_crlf])
            .map_err(|_| FastFetchError::Transport("invalid HTTP chunk size encoding".into()))?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| {
            FastFetchError::Transport(format!("invalid HTTP chunk size `{size_hex}`"))
        })?;
        pos += rel_crlf + 2;
        if size == 0 {
            if raw[pos..].len() < 2 {
                return Err(FastFetchError::Transport(
                    "incomplete chunked HTTP terminator".into(),
                ));
            }
            return Ok(out);
        }
        if out.len().saturating_add(size) > MAX_FAST_FETCH_BODY_BYTES {
            return Err(FastFetchError::Transport(
                "HTTP response body exceeds 10 MiB limit".into(),
            ));
        }
        if raw.len() - pos < size + 2 {
            return Err(FastFetchError::Transport(
                "truncated chunked HTTP response body".into(),
            ));
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size;
        if &raw[pos..pos + 2] != b"\r\n" {
            return Err(FastFetchError::Transport(
                "missing CRLF after HTTP chunk data".into(),
            ));
        }
        pos += 2;
    }
}

pub(crate) fn decode_content_encoding(
    raw: Vec<u8>,
    content_encoding: Option<&str>,
) -> Result<Vec<u8>, FastFetchError> {
    let Some(enc_raw) = content_encoding else {
        return Ok(raw);
    };
    let enc = enc_raw.trim().to_ascii_lowercase();
    if enc.is_empty() || enc == "identity" {
        return Ok(raw);
    }
    if enc == "gzip" || enc == "x-gzip" {
        let decoder = MultiGzDecoder::new(raw.as_slice());
        return read_limited(decoder, "gzip");
    }
    if enc == "deflate" {
        // Servers send either RFC 1950 zlib-wrapped deflate or raw deflate streams.
        let zlib = ZlibDecoder::new(raw.as_slice());
        if let Ok(out) = read_limited(zlib, "deflate") {
            return Ok(out);
        }
        let raw_deflate = DeflateDecoder::new(raw.as_slice());
        return read_limited(raw_deflate, "deflate");
    }
    Err(FastFetchError::Transport(format!(
        "unsupported Content-Encoding `{enc_raw}`"
    )))
}

fn read_limited<R: std::io::Read>(reader: R, kind: &str) -> Result<Vec<u8>, FastFetchError> {
    let mut limited = reader.take((MAX_FAST_FETCH_BODY_BYTES + 1) as u64);
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|e| FastFetchError::Transport(format!("{kind} decompression failed: {e}")))?;
    if out.len() > MAX_FAST_FETCH_BODY_BYTES {
        return Err(FastFetchError::Transport(format!(
            "decompressed {kind} response exceeds 10 MiB limit"
        )));
    }
    Ok(out)
}

pub(crate) fn find_header_ci<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn proxy_bypasses_host(bypass: Option<&str>, host: &str) -> bool {
    let Some(list) = bypass else {
        return false;
    };
    let host_l = host.to_ascii_lowercase();
    for raw in list.split(',') {
        let pat = raw.trim().to_ascii_lowercase();
        if pat.is_empty() {
            continue;
        }
        if pat == "<local>" {
            if !host_l.contains('.')
                || host_l == "localhost"
                || host_l == "127.0.0.1"
                || host_l == "::1"
            {
                return true;
            }
            continue;
        }
        let pat_host = pat.split(':').next().unwrap_or(&pat);
        if let Some(suffix) = pat_host.strip_prefix("*.") {
            if host_l == suffix || host_l.ends_with(&format!(".{suffix}")) {
                return true;
            }
        } else if let Some(suffix) = pat_host.strip_prefix('*') {
            if host_l.ends_with(suffix) {
                return true;
            }
        } else if let Some(bare) = pat_host.strip_prefix('.') {
            if host_l == bare || host_l.ends_with(pat_host) {
                return true;
            }
        } else if host_l == pat_host {
            return true;
        }
    }
    false
}

fn basic_auth_header(username: &str, secret: Option<&str>) -> String {
    let mut raw = String::with_capacity(username.len() + 1 + secret.map_or(0, str::len));
    raw.push_str(username);
    raw.push(':');
    if let Some(s) = secret {
        raw.push_str(s);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    format!("Basic {encoded}")
}
