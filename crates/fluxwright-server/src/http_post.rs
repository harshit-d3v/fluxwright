//! Shared HTTP/1.1 + HTTPS (`tokio-rustls` + `webpki-roots` + `ring`) POST client helper.
//!
//! Used by cluster worker RPCs, signed webhook delivery, and OTLP trace export. When
//! `ssrf_check` is enabled, URLs are validated with [`fluxwright::netguard`] and DNS resolution
//! checks every returned IP address before connecting directly to a validated [`SocketAddr`],
//! eliminating DNS-rebinding TOCTOU races while preserving hostname SNI and `Host` headers.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fluxwright::netguard;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub(crate) const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024 + 65_536;
pub(crate) const SMALL_HTTP_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HttpEndpoint {
    pub(crate) tls: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) host_header: String,
    pub(crate) path_and_query: String,
}

impl HttpEndpoint {
    pub(crate) fn parse(url: &str, allow_query: bool) -> Result<Self, String> {
        let trimmed = url.trim();
        if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("URL must be non-empty without whitespace or control characters".into());
        }
        let (scheme, rest) = trimmed
            .split_once("://")
            .ok_or_else(|| "URL must start with http:// or https://".to_string())?;
        let tls = if scheme.eq_ignore_ascii_case("http") {
            false
        } else if scheme.eq_ignore_ascii_case("https") {
            true
        } else {
            return Err("URL must start with http:// or https://".into());
        };
        let default_port = if tls { 443 } else { 80 };

        if !allow_query && (rest.contains('?') || rest.contains('#')) {
            return Err("userinfo, query, and fragment are not allowed".into());
        }
        let rest_no_frag = rest.split('#').next().unwrap_or(rest);
        if rest_no_frag.contains('\\') {
            return Err("URL must not contain backslashes".into());
        }

        let auth_end = rest_no_frag.find(['/', '?']).unwrap_or(rest_no_frag.len());
        let authority = &rest_no_frag[..auth_end];
        let suffix = &rest_no_frag[auth_end..];

        if authority.contains('@') {
            return Err("userinfo is not allowed in URL".into());
        }

        let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
            let Some((ip_str, tail)) = after_bracket.split_once(']') else {
                return Err("unclosed IPv6 host".into());
            };
            if ip_str.parse::<Ipv6Addr>().is_err() {
                return Err("invalid IPv6 host".into());
            }
            let port = match tail {
                "" => default_port,
                _ => tail
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|&p| p != 0)
                    .ok_or_else(|| "invalid port".to_string())?,
            };
            (ip_str.to_ascii_lowercase(), port)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            if h.is_empty() || h.contains(':') {
                return Err("invalid host".into());
            }
            let port = p
                .parse::<u16>()
                .ok()
                .filter(|&n| n != 0)
                .ok_or_else(|| "invalid port".to_string())?;
            (h.to_ascii_lowercase(), port)
        } else {
            (authority.to_ascii_lowercase(), default_port)
        };

        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("empty or invalid host".into());
        }

        let path_and_query = if suffix.is_empty() {
            "/".to_string()
        } else if suffix.starts_with('?') {
            format!("/{suffix}")
        } else {
            suffix.to_string()
        };

        Ok(Self {
            tls,
            host,
            port,
            host_header: authority.to_string(),
            path_and_query,
        })
    }

    pub(crate) fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self.host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }

    pub(crate) fn base_path(&self) -> &str {
        let path_only = self
            .path_and_query
            .split('?')
            .next()
            .unwrap_or(&self.path_and_query);
        path_only.trim_end_matches('/')
    }
}

/// Shared rustls `ClientConfig` backed by `ring` and `webpki-roots`.
pub(crate) fn tls_client_config() -> &'static Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let cfg =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
        Arc::new(cfg)
    })
}

/// Returns `Ok(())` when `ip` is permitted by the SSRF policy (blocking link-local and cloud
/// metadata addresses, matching `fluxwright::netguard`).
pub(crate) fn ip_allowed(ip: IpAddr) -> Result<(), &'static str> {
    let blocked = match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0 == 0xfe80)
                || v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
    };
    if blocked {
        Err("link-local IP addresses (cloud metadata) are blocked")
    } else {
        Ok(())
    }
}

/// Filters resolved socket addresses against `ip_allowed` when `ssrf_check` is enabled.
/// Rejects the entire resolution set if any returned address is blocked, preventing mixed-record
/// DNS rebinding bypasses.
pub(crate) fn validate_resolved_addrs(
    addrs: Vec<SocketAddr>,
    ssrf_check: bool,
) -> Result<Vec<SocketAddr>, String> {
    if addrs.is_empty() {
        return Err("DNS resolution returned no addresses".into());
    }
    if ssrf_check {
        for addr in &addrs {
            ip_allowed(addr.ip()).map_err(str::to_string)?;
        }
    }
    Ok(addrs)
}

pub(crate) fn validate_header_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_HEADER_NAME_BYTES {
        return Err(format!(
            "header name must be 1-{MAX_HEADER_NAME_BYTES} bytes"
        ));
    }
    let valid = name.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    });
    if !valid {
        return Err(format!("invalid HTTP header name: {name:?}"));
    }
    Ok(())
}

pub(crate) fn validate_header_value(value: &str) -> Result<(), String> {
    if value.len() > MAX_HEADER_VALUE_BYTES {
        return Err(format!(
            "header value must be at most {MAX_HEADER_VALUE_BYTES} bytes"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("header value must not contain CR, LF, or control characters".into());
    }
    Ok(())
}

pub(crate) fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "host" | "content-length" | "connection" | "transfer-encoding"
    ) || lower.starts_with("x-fluxwright-")
}

fn is_framing_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "connection" | "transfer-encoding"
    )
}

fn validate_request_target(target: &str) -> Result<(), String> {
    if !target.starts_with('/') || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("invalid HTTP request target".into());
    }
    Ok(())
}

fn build_post_head(
    host_header: &str,
    target: &str,
    headers: &[(&str, &str)],
    body_len: usize,
) -> Result<Vec<u8>, String> {
    validate_request_target(target)?;
    validate_header_value(host_header)?;

    let mut head = format!(
        "POST {target} HTTP/1.1\r\nHost: {host_header}\r\nContent-Length: {body_len}\r\nConnection: close\r\n"
    );
    for &(name, value) in headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
        if is_framing_header(name) {
            return Err(format!("framing header '{name}' cannot be overridden"));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    Ok(head.into_bytes())
}

async fn resolve_and_connect(ep: &HttpEndpoint, ssrf_check: bool) -> Result<TcpStream, String> {
    if ssrf_check {
        let check_url = if ep.host.contains(':') {
            format!(
                "{}://[{}]:{}{}",
                if ep.tls { "https" } else { "http" },
                ep.host,
                ep.port,
                ep.path_and_query
            )
        } else {
            format!(
                "{}://{}:{}{}",
                if ep.tls { "https" } else { "http" },
                ep.host,
                ep.port,
                ep.path_and_query
            )
        };
        netguard::url_allowed(&check_url).map_err(str::to_string)?;
    }

    let addrs = if let Ok(ip) = ep.host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, ep.port)]
    } else {
        tokio::net::lookup_host((ep.host.as_str(), ep.port))
            .await
            .map_err(|e| format!("DNS lookup failed: {e}"))?
            .collect()
    };
    let validated = validate_resolved_addrs(addrs, ssrf_check)?;

    let mut last_err = None;
    for addr in validated {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "could not connect to target host".into()))
}

/// Sends an HTTP/1.1 or HTTPS POST request with strict header hygiene and optional DNS-rebinding
/// SSRF validation. Connects directly to the validated resolved `SocketAddr` while preserving the
/// hostname in TLS SNI and the `Host` header.
pub(crate) async fn post_bytes(
    ep: &HttpEndpoint,
    path_override: Option<&str>,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
    max_response_bytes: usize,
    ssrf_check: bool,
) -> Result<(u16, Vec<u8>), String> {
    let target = path_override.unwrap_or(ep.path_and_query.as_str());
    let head = build_post_head(&ep.host_header, target, headers, body.len())?;

    let io = async {
        let stream = resolve_and_connect(ep, ssrf_check).await?;
        if ep.tls {
            let server_name = ServerName::try_from(ep.host.clone())
                .map_err(|_| format!("invalid TLS server name: {}", ep.host))?;
            let connector = TlsConnector::from(tls_client_config().clone());
            let mut tls_stream = connector
                .connect(server_name, stream)
                .await
                .map_err(|e| e.to_string())?;
            exchange_http(&mut tls_stream, &head, body, max_response_bytes).await
        } else {
            let mut tcp = stream;
            exchange_http(&mut tcp, &head, body, max_response_bytes).await
        }
    };

    tokio::time::timeout(timeout, io)
        .await
        .map_err(|_| "HTTP request timed out".to_string())?
}

async fn exchange_http<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    head: &[u8],
    body: &[u8],
    max_response_bytes: usize,
) -> Result<(u16, Vec<u8>), String> {
    stream.write_all(head).await.map_err(|e| e.to_string())?;
    stream.write_all(body).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let mut parsed_meta: Option<(usize, u16, bool, Option<usize>)> = None;

    loop {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if raw.len().saturating_add(n) > max_response_bytes {
            return Err("HTTP response exceeded maximum size".into());
        }
        raw.extend_from_slice(&buf[..n]);

        if parsed_meta.is_none() {
            if let Some(sep) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let (status, is_chunked, content_length) = parse_header_meta(&raw[..sep])?;
                if !is_chunked && matches!(status, 204 | 304) {
                    return Ok((status, Vec::new()));
                }
                if let Some(cl) = content_length {
                    if sep.saturating_add(4).saturating_add(cl) > max_response_bytes {
                        return Err("HTTP response exceeded maximum size".into());
                    }
                }
                parsed_meta = Some((sep, status, is_chunked, content_length));
            }
        }

        if let Some((sep, status, is_chunked, Some(cl))) = parsed_meta {
            if !is_chunked && raw.len() >= sep + 4 + cl {
                return Ok((status, raw[sep + 4..sep + 4 + cl].to_vec()));
            }
        }
    }

    parse_http_response(&raw)
}

fn parse_header_meta(head_bytes: &[u8]) -> Result<(u16, bool, Option<usize>), String> {
    let header_text =
        std::str::from_utf8(head_bytes).map_err(|_| "invalid HTTP headers".to_string())?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status line: {status_line}"))?;

    let mut is_chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            } else if key.eq_ignore_ascii_case("content-length") {
                if let Ok(len) = v.trim().parse::<usize>() {
                    content_length = Some(len);
                }
            }
        }
    }
    Ok((status, is_chunked, content_length))
}

fn parse_http_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response".to_string())?;
    let (status, is_chunked, content_length) = parse_header_meta(&raw[..sep])?;
    let body = &raw[sep + 4..];
    let decoded = if is_chunked {
        decode_chunked(body)?
    } else if let Some(cl) = content_length {
        if body.len() < cl {
            return Err("truncated HTTP response body".into());
        }
        body[..cl].to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, decoded))
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            return Err("truncated chunked response".into());
        };
        let size_hex = std::str::from_utf8(&data[..line_end])
            .map_err(|_| "invalid chunk size")?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "invalid chunk hex")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err("truncated chunk payload".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}
