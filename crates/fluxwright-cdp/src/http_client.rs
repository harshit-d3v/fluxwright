//! Async HTTP/1.1 + HTTPS client for `APIRequestContext` and `route.fetch()`, sharing cookies with the browser context.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::browser::Proxy;
use crate::connection::Connection;
use crate::context::{filter_cookies, Cookie};
use crate::error::{Error, Result};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_REDIRECTS: usize = 20;
const MAX_HEADER_BYTES: usize = 256 * 1024;
const MAX_BODY_BYTES: usize = 128 * 1024 * 1024;

/// Options for `APIRequestContext` requests (`context.request().fetch(...)` / `.get(...)` / ...).
#[derive(Debug, Clone, Default)]
pub struct ApiRequestOptions {
    pub method: Option<String>,
    pub headers: Vec<(String, String)>,
    pub params: Vec<(String, String)>,
    pub data: Option<Vec<u8>>,
    pub fail_on_status_code: bool,
    /// `None` defaults to 20; `Some(0)` returns 3xx responses without following redirects.
    pub max_redirects: Option<usize>,
    /// `None` uses the context default timeout (30 s); `Some(Duration::ZERO)` disables timeout.
    pub timeout: Option<Duration>,
}

/// Options for `route.fetch(...)`.
#[derive(Debug, Clone, Default)]
pub struct RouteFetchOptions {
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Option<Vec<(String, String)>>,
    pub post_data: Option<Vec<u8>>,
    /// `None` defaults to 20; `Some(0)` returns 3xx responses without following redirects.
    pub max_redirects: Option<usize>,
    pub timeout: Option<Duration>,
}

/// Response returned by `APIRequestContext` and `route.fetch()`.
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ApiResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Lower-cased headers map matching Playwright's `response.headers()`.
    pub fn headers_map(&self) -> HashMap<String, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        for (name, value) in &self.headers {
            let key = name.to_ascii_lowercase();
            match map.get_mut(&key) {
                Some(existing) => {
                    existing.push_str(if key == "set-cookie" { "\n" } else { ", " });
                    existing.push_str(value);
                }
                None => {
                    map.insert(key, value.clone());
                }
            }
        }
        map
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// Context-level HTTP defaults carried into `APIRequestContext` and `route.fetch()`.
#[derive(Debug, Clone, Default)]
pub struct HttpContextDefaults {
    pub base_url: Option<String>,
    pub extra_http_headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
    pub http_credentials: Option<(String, String)>,
    pub proxy: Option<Proxy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub query: Option<String>,
}

impl ParsedUrl {
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        if raw.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(Error::Other(format!("invalid URL `{raw}`")));
        }
        let trimmed = raw.trim();
        let (scheme_raw, rest) = trimmed
            .split_once("://")
            .ok_or_else(|| Error::Other(format!("invalid URL `{raw}`: missing scheme")))?;
        let scheme = scheme_raw.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return Err(Error::Other(format!("unsupported URL scheme `{scheme}` in `{raw}`"))),
        };
        let rest_no_frag = rest.split('#').next().unwrap_or(rest);
        let auth_end = rest_no_frag.find(['/', '?']).unwrap_or(rest_no_frag.len());
        let authority = &rest_no_frag[..auth_end];
        let suffix = &rest_no_frag[auth_end..];
        let (path_part, query) = match suffix.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (suffix, None),
        };
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        if host_port.is_empty() {
            return Err(Error::Other(format!("invalid URL `{raw}`: empty host")));
        }
        let (host, port) = if let Some(after_open) = host_port.strip_prefix('[') {
            let Some((v6, after_close)) = after_open.split_once(']') else {
                return Err(Error::Other(format!("invalid IPv6 host in URL `{raw}`")));
            };
            if v6.is_empty() {
                return Err(Error::Other(format!("invalid IPv6 host in URL `{raw}`")));
            }
            let port = if after_close.is_empty() {
                default_port
            } else if let Some(p) = after_close.strip_prefix(':') {
                p.parse::<u16>()
                    .map_err(|_| Error::Other(format!("invalid port in URL `{raw}`")))?
            } else {
                return Err(Error::Other(format!("invalid host in URL `{raw}`")));
            };
            (v6.to_ascii_lowercase(), port)
        } else if let Some((h, p)) = host_port.rsplit_once(':') {
            if h.is_empty() || h.contains(':') {
                return Err(Error::Other(format!("invalid host in URL `{raw}`")));
            }
            let port = p
                .parse::<u16>()
                .map_err(|_| Error::Other(format!("invalid port in URL `{raw}`")))?;
            (h.to_ascii_lowercase(), port)
        } else {
            (host_port.to_ascii_lowercase(), default_port)
        };
        if host.is_empty() {
            return Err(Error::Other(format!("invalid URL `{raw}`: empty host")));
        }
        let path = if path_part.is_empty() {
            "/".to_string()
        } else if path_part.starts_with('/') {
            path_part.to_string()
        } else {
            format!("/{path_part}")
        };
        Ok(Self {
            scheme,
            host,
            port,
            path,
            query,
        })
    }

    pub(crate) fn host_header(&self) -> String {
        let host_part = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let is_default = (self.scheme == "http" && self.port == 80)
            || (self.scheme == "https" && self.port == 443);
        if is_default {
            host_part
        } else {
            format!("{host_part}:{}", self.port)
        }
    }

    pub(crate) fn connect_host_port(&self) -> (String, u16) {
        (self.host.clone(), self.port)
    }

    pub(crate) fn path_and_query(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    pub(crate) fn canonical(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host_header(), self.path_and_query())
    }

    fn origin_key(&self) -> (&str, &str, u16) {
        (&self.scheme, &self.host, self.port)
    }
}

fn normalize_path(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    if stack.is_empty() {
        "/".to_string()
    } else {
        let mut out = format!("/{}", stack.join("/"));
        if trailing && !out.ends_with('/') {
            out.push('/');
        }
        out
    }
}

pub(crate) fn resolve_url(base_url: Option<&str>, target: &str) -> Result<String> {
    if target.contains("://") {
        return Ok(ParsedUrl::parse(target)?.canonical());
    }
    let base_raw = base_url.ok_or_else(|| {
        Error::Other(format!(
            "cannot resolve relative URL `{target}` without baseURL"
        ))
    })?;
    let base = ParsedUrl::parse(base_raw)?;
    let target_no_frag = target.split('#').next().unwrap_or(target);
    let candidate = if target_no_frag.starts_with("//") {
        format!("{}:{target_no_frag}", base.scheme)
    } else if target_no_frag.is_empty() {
        base.canonical()
    } else if target_no_frag.starts_with('?') {
        format!("{}://{}{}{}", base.scheme, base.host_header(), base.path, target_no_frag)
    } else if target_no_frag.starts_with('/') {
        format!("{}://{}{}", base.scheme, base.host_header(), target_no_frag)
    } else {
        let (rel_path, rel_query) = match target_no_frag.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (target_no_frag, None),
        };
        let dir = match base.path.rsplit_once('/') {
            Some((prefix, _)) => prefix,
            None => "",
        };
        let joined = normalize_path(&format!("{dir}/{rel_path}"));
        match rel_query {
            Some(q) => format!("{}://{}{}{}?{q}", base.scheme, base.host_header(), "", joined),
            None => format!("{}://{}{}", base.scheme, base.host_header(), joined),
        }
    };
    Ok(ParsedUrl::parse(&candidate)?.canonical())
}

fn percent_encode_component(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

pub(crate) fn append_query_params(url: &str, params: &[(String, String)]) -> String {
    if params.is_empty() {
        return url.to_string();
    }
    let mut out = url.to_string();
    let mut sep = if !out.contains('?') {
        '?'
    } else if out.ends_with('?') || out.ends_with('&') {
        '\0'
    } else {
        '&'
    };
    for (k, v) in params {
        if sep != '\0' {
            out.push(sep);
        }
        out.push_str(&percent_encode_component(k));
        out.push('=');
        out.push_str(&percent_encode_component(v));
        sep = '&';
    }
    out
}

fn tls_client_config() -> &'static Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let cfg = ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Arc::new(cfg)
    })
}

enum HttpStream {
    Plain(TcpStream),
    Tls(AsyncTlsStream),
}

struct AsyncTlsStream {
    stream: TcpStream,
    tls: ClientConnection,
}

impl AsyncTlsStream {
    async fn connect_on(stream: TcpStream, host: &str) -> Result<Self> {
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|_| Error::Other(format!("invalid TLS server name `{host}`")))?;
        let tls = ClientConnection::new(tls_client_config().clone(), server_name)
            .map_err(|e| Error::Other(format!("TLS init failed for `{host}`: {e}")))?;
        let mut conn = Self { stream, tls };
        conn.handshake().await?;
        Ok(conn)
    }

    async fn flush_tls(&mut self) -> Result<()> {
        while self.tls.wants_write() {
            let mut buf = Vec::new();
            let n = self.tls.write_tls(&mut buf)?;
            if n == 0 {
                break;
            }
            self.stream.write_all(&buf).await?;
        }
        Ok(())
    }

    async fn read_tls_step(&mut self) -> Result<usize> {
        let mut buf = [0u8; 16 * 1024];
        let n = self.stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(0);
        }
        let mut cursor = std::io::Cursor::new(&buf[..n]);
        while (cursor.position() as usize) < n {
            let m = self.tls.read_tls(&mut cursor)?;
            self.tls
                .process_new_packets()
                .map_err(|e| Error::Other(format!("TLS error: {e}")))?;
            if m == 0 {
                break;
            }
        }
        Ok(n)
    }

    async fn handshake(&mut self) -> Result<()> {
        while self.tls.is_handshaking() {
            self.flush_tls().await?;
            if self.tls.is_handshaking() {
                let n = self.read_tls_step().await?;
                self.flush_tls().await?;
                if n == 0 && self.tls.is_handshaking() {
                    return Err(Error::Other("TLS connection closed during handshake".into()));
                }
            }
        }
        self.flush_tls().await?;
        Ok(())
    }

    async fn write_all(&mut self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let n = self.tls.writer().write(data)?;
            if n == 0 {
                self.flush_tls().await?;
                continue;
            }
            data = &data[n..];
            self.flush_tls().await?;
        }
        Ok(())
    }

    async fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        loop {
            match self.tls.reader().read(out) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let n = self.read_tls_step().await?;
                    self.flush_tls().await?;
                    if n == 0 {
                        // Server closed TCP without close_notify; return 0 and let HTTP framing verify completeness.
                        return match self.tls.reader().read(out) {
                            Ok(m) => Ok(m),
                            Err(err)
                                if matches!(
                                    err.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::UnexpectedEof
                                ) =>
                            {
                                Ok(0)
                            }
                            Err(err) => Err(err.into()),
                        };
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl HttpStream {
    async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        match self {
            HttpStream::Plain(s) => Ok(s.write_all(data).await?),
            HttpStream::Tls(s) => s.write_all(data).await,
        }
    }

    async fn read(&mut self, out: &mut [u8]) -> Result<usize> {
        match self {
            HttpStream::Plain(s) => Ok(s.read(out).await?),
            HttpStream::Tls(s) => s.read(out).await,
        }
    }
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
            if !host_l.contains('.') || host_l == "localhost" || host_l == "127.0.0.1" || host_l == "::1" {
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
    use base64::Engine;
    let mut raw = String::with_capacity(username.len() + 1 + secret.map_or(0, str::len));
    raw.push_str(username);
    raw.push(':');
    if let Some(s) = secret {
        raw.push_str(s);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    format!("Basic {encoded}")
}

fn validate_token_clean(label: &str, value: &str) -> Result<()> {
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(Error::Other(format!("{label} contains invalid CR/LF/NUL characters")));
    }
    Ok(())
}

fn set_header_ci(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, existing)) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        *existing = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

fn has_header_ci(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

fn find_header_ci<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn read_connect_headers(stream: &mut TcpStream) -> Result<String> {
    let mut raw = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(Error::Other("proxy closed connection during CONNECT".into()));
        }
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&raw).into_owned());
        }
        if raw.len() > 16 * 1024 {
            return Err(Error::Other("proxy CONNECT response headers too large".into()));
        }
    }
}

fn parse_status_line(line: &str) -> Result<(u16, String)> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(Error::Other(format!("unexpected HTTP status line `{line}`")));
    }
    let code_str = parts
        .next()
        .ok_or_else(|| Error::Other(format!("missing HTTP status code in `{line}`")))?;
    let status = code_str
        .parse::<u16>()
        .map_err(|_| Error::Other(format!("invalid HTTP status code `{code_str}`")))?;
    let reason = parts.next().unwrap_or("").trim().to_string();
    Ok((status, reason))
}

fn parse_headers_block(block: &str) -> Result<(u16, String, Vec<(String, String)>)> {
    let mut lines = block.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| Error::Other("empty HTTP response".into()))?;
    let (status, status_text) = parse_status_line(first)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        headers.push((name.trim().to_string(), val.trim().to_string()));
    }
    Ok((status, status_text, headers))
}

pub(crate) fn decode_chunked_body(raw: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let Some(rel_crlf) = raw[pos..].windows(2).position(|w| w == b"\r\n") else {
            return Err(Error::Other("incomplete chunked HTTP response".into()));
        };
        let size_line = std::str::from_utf8(&raw[pos..pos + rel_crlf])
            .map_err(|_| Error::Other("invalid chunk size encoding".into()))?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| Error::Other(format!("invalid HTTP chunk size `{size_hex}`")))?;
        pos += rel_crlf + 2;
        if size == 0 {
            if raw[pos..].len() < 2 {
                return Err(Error::Other("incomplete chunked HTTP terminator".into()));
            }
            return Ok(out);
        }
        if out.len().saturating_add(size) > MAX_BODY_BYTES {
            return Err(Error::Other("HTTP response body exceeds 128 MiB limit".into()));
        }
        if raw.len() - pos < size + 2 {
            return Err(Error::Other("truncated chunked HTTP response body".into()));
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size;
        if &raw[pos..pos + 2] != b"\r\n" {
            return Err(Error::Other("missing CRLF after HTTP chunk data".into()));
        }
        pos += 2;
    }
}

fn try_decode_chunked_complete(raw: &[u8]) -> Option<Result<Vec<u8>>> {
    let mut pos = 0;
    let mut out = Vec::new();
    loop {
        let rel_crlf = raw[pos..].windows(2).position(|w| w == b"\r\n")?;
        let size_line = match std::str::from_utf8(&raw[pos..pos + rel_crlf]) {
            Ok(s) => s,
            Err(_) => return Some(Err(Error::Other("invalid chunk size encoding".into()))),
        };
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = match usize::from_str_radix(size_hex, 16) {
            Ok(s) => s,
            Err(_) => return Some(Err(Error::Other(format!("invalid HTTP chunk size `{size_hex}`")))),
        };
        pos += rel_crlf + 2;
        if size == 0 {
            if raw[pos..].windows(2).any(|w| w == b"\r\n") || raw[pos..].is_empty() {
                if raw[pos..].starts_with(b"\r\n") || raw[pos..].windows(4).any(|w| w == b"\r\n\r\n") {
                    return Some(Ok(out));
                }
                return None;
            }
            return None;
        }
        if out.len().saturating_add(size) > MAX_BODY_BYTES {
            return Some(Err(Error::Other("HTTP response body exceeds 128 MiB limit".into())));
        }
        if raw.len() - pos < size + 2 {
            return None;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size;
        if &raw[pos..pos + 2] != b"\r\n" {
            return Some(Err(Error::Other("missing CRLF after HTTP chunk data".into())));
        }
        pos += 2;
    }
}

async fn read_http_response(stream: &mut HttpStream, method: &str) -> Result<(u16, String, Vec<(String, String)>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(Error::Other("HTTP response headers too large".into()));
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(Error::Other("unexpected EOF before HTTP response headers".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| Error::Other("non-UTF-8 HTTP response headers".into()))?;
    let (mut status, mut status_text, mut headers) = parse_headers_block(header_str)?;
    let mut leftover = buf[header_end + 4..].to_vec();

    // Handle 1xx interim responses if a server sends 100-Continue or similar.
    while (100..200).contains(&status) && status != 101 {
        while !leftover.windows(4).any(|w| w == b"\r\n\r\n") {
            if leftover.len() > MAX_HEADER_BYTES {
                return Err(Error::Other("HTTP response headers too large".into()));
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(Error::Other("unexpected EOF after 1xx HTTP response".into()));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
        let pos = leftover.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let next_str = std::str::from_utf8(&leftover[..pos])
            .map_err(|_| Error::Other("non-UTF-8 HTTP response headers".into()))?;
        (status, status_text, headers) = parse_headers_block(next_str)?;
        leftover = leftover[pos + 4..].to_vec();
    }

    if method.eq_ignore_ascii_case("HEAD") || matches!(status, 204 | 304) || (100..200).contains(&status) {
        return Ok((status, status_text, headers, Vec::new()));
    }

    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    if is_chunked {
        loop {
            if let Some(decoded) = try_decode_chunked_complete(&leftover) {
                return Ok((status, status_text, headers, decoded?));
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                let decoded = decode_chunked_body(&leftover)?;
                return Ok((status, status_text, headers, decoded));
            }
            if leftover.len() + n > MAX_BODY_BYTES {
                return Err(Error::Other("HTTP response body exceeds 128 MiB limit".into()));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
    }

    if let Some(len_str) = find_header_ci(&headers, "content-length") {
        let expected = len_str
            .trim()
            .parse::<usize>()
            .map_err(|_| Error::Other(format!("invalid Content-Length `{len_str}`")))?;
        if expected > MAX_BODY_BYTES {
            return Err(Error::Other("HTTP response body exceeds 128 MiB limit".into()));
        }
        while leftover.len() < expected {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(Error::Other("unexpected EOF reading HTTP response body".into()));
            }
            leftover.extend_from_slice(&tmp[..n]);
        }
        leftover.truncate(expected);
        return Ok((status, status_text, headers, leftover));
    }

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok((status, status_text, headers, leftover));
        }
        if leftover.len() + n > MAX_BODY_BYTES {
            return Err(Error::Other("HTTP response body exceeds 128 MiB limit".into()));
        }
        leftover.extend_from_slice(&tmp[..n]);
    }
}

async fn send_single_hop(
    target: &ParsedUrl,
    method: &str,
    req_headers: Vec<(String, String)>,
    body: Option<&[u8]>,
    proxy: Option<&Proxy>,
) -> Result<(u16, String, Vec<(String, String)>, Vec<u8>)> {
    validate_token_clean("HTTP method", method)?;
    let (target_host, target_port) = target.connect_host_port();

    let use_proxy = proxy.filter(|p| !proxy_bypasses_host(p.bypass.as_deref(), &target.host));
    let mut proxy_auth_header: Option<String> = None;
    let mut absolute_form = false;

    let mut stream = if let Some(p) = use_proxy {
        let server = p.server.trim();
        let proxy_url_str = if server.contains("://") {
            server.to_string()
        } else {
            format!("http://{server}")
        };
        let proxy_scheme = proxy_url_str.split("://").next().unwrap_or("");
        if !proxy_scheme.eq_ignore_ascii_case("http") {
            // shortcut: SOCKS5 proxy not supported by APIRequestContext, upgrade when needed
            return Err(Error::Other(format!(
                "APIRequestContext only supports http:// proxies, not `{}`",
                p.server
            )));
        }
        let parsed_proxy = ParsedUrl::parse(&proxy_url_str)?;
        proxy_auth_header = p
            .username
            .as_deref()
            .map(|u| basic_auth_header(u, p.password.as_deref()));

        let mut tcp = TcpStream::connect(parsed_proxy.connect_host_port()).await?;
        if target.scheme == "https" {
            let connect_host = if target.host.contains(':') {
                format!("[{}]:{}", target.host, target.port)
            } else {
                format!("{}:{}", target.host, target.port)
            };
            validate_token_clean("CONNECT host", &connect_host)?;
            let mut connect_req = format!(
                "CONNECT {connect_host} HTTP/1.1\r\nHost: {connect_host}\r\n"
            );
            if let Some(auth) = &proxy_auth_header {
                validate_token_clean("Proxy-Authorization", auth)?;
                connect_req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
            }
            connect_req.push_str("\r\n");
            tcp.write_all(connect_req.as_bytes()).await?;
            let resp_block = read_connect_headers(&mut tcp).await?;
            let first_line = resp_block.split("\r\n").next().unwrap_or("");
            let (code, reason) = parse_status_line(first_line)?;
            if !(200..300).contains(&code) {
                return Err(Error::Other(format!(
                    "proxy CONNECT failed: {code} {reason}"
                )));
            }
            HttpStream::Tls(AsyncTlsStream::connect_on(tcp, &target_host).await?)
        } else {
            absolute_form = true;
            HttpStream::Plain(tcp)
        }
    } else {
        let tcp = TcpStream::connect((target_host.as_str(), target_port)).await?;
        if target.scheme == "https" {
            HttpStream::Tls(AsyncTlsStream::connect_on(tcp, &target_host).await?)
        } else {
            HttpStream::Plain(tcp)
        }
    };

    let request_target = if absolute_form {
        target.canonical()
    } else {
        target.path_and_query()
    };
    validate_token_clean("request target", &request_target)?;

    let mut headers = req_headers;
    headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case("accept-encoding")
            && !k.eq_ignore_ascii_case("connection")
            && !k.eq_ignore_ascii_case("host")
            && !k.eq_ignore_ascii_case("content-length")
    });
    set_header_ci(&mut headers, "Host", target.host_header());
    set_header_ci(&mut headers, "Connection", "close".to_string());
    set_header_ci(&mut headers, "Accept-Encoding", "identity".to_string());
    if let (true, Some(auth)) = (absolute_form, proxy_auth_header) {
        if !has_header_ci(&headers, "proxy-authorization") {
            set_header_ci(&mut headers, "Proxy-Authorization", auth);
        }
    }
    let body_bytes = body.unwrap_or(&[]);
    let send_content_length = body.is_some()
        || matches!(
            method.to_ascii_uppercase().as_str(),
            "POST" | "PUT" | "PATCH"
        );
    if send_content_length {
        set_header_ci(&mut headers, "Content-Length", body_bytes.len().to_string());
    }

    let mut wire = format!("{method} {request_target} HTTP/1.1\r\n");
    for (k, v) in &headers {
        validate_token_clean("HTTP header name", k)?;
        validate_token_clean("HTTP header value", v)?;
        wire.push_str(k);
        wire.push_str(": ");
        wire.push_str(v);
        wire.push_str("\r\n");
    }
    wire.push_str("\r\n");

    stream.write_all(wire.as_bytes()).await?;
    if !body_bytes.is_empty() {
        stream.write_all(body_bytes).await?;
    }

    read_http_response(&mut stream, method).await
}

fn now_unix_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn is_ip_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

fn default_cookie_path(req_path: &str) -> String {
    if !req_path.starts_with('/') {
        return "/".to_string();
    }
    match req_path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => req_path[..idx].to_string(),
    }
}

fn month_index(m: &str) -> Option<u32> {
    match m.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

/// Howard Hinnant's civil-to-days algorithm (returns Unix seconds in UTC).
fn utc_timestamp(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Option<f64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + (doe as i64) - 719468;
    let secs = days * 86400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);
    Some(secs as f64)
}

/// Parses HTTP-date formats (IMF-fixdate, RFC 850, asctime) into Unix seconds.
pub(crate) fn parse_http_date(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    let without_day_name = match trimmed.split_once(',') {
        Some((_, rest)) => rest.trim(),
        None => trimmed,
    };
    let tokens: Vec<&str> = without_day_name.split_whitespace().collect();
    // IMF-fixdate: "06 Nov 1994 08:49:37 GMT"
    if tokens.len() >= 5 && tokens[0].chars().all(|c| c.is_ascii_digit()) {
        let day = tokens[0].parse::<u32>().ok()?;
        let month = month_index(tokens[1])?;
        let mut year = tokens[2].parse::<i64>().ok()?;
        if tokens[2].len() == 2 {
            year += if year < 70 { 2000 } else { 1900 };
        }
        let mut hms = tokens[3].split(':');
        let h = hms.next()?.parse::<u32>().ok()?;
        let m = hms.next()?.parse::<u32>().ok()?;
        let s = hms.next()?.parse::<u32>().ok()?;
        return utc_timestamp(year, month, day, h, m, s);
    }
    // RFC 850: "06-Nov-94 08:49:37 GMT"
    if tokens.len() >= 2 && tokens[0].contains('-') {
        let mut dmy = tokens[0].split('-');
        let day = dmy.next()?.parse::<u32>().ok()?;
        let month = month_index(dmy.next()?)?;
        let y_str = dmy.next()?;
        let mut year = y_str.parse::<i64>().ok()?;
        if y_str.len() == 2 {
            year += if year < 70 { 2000 } else { 1900 };
        }
        let mut hms = tokens[1].split(':');
        let h = hms.next()?.parse::<u32>().ok()?;
        let m = hms.next()?.parse::<u32>().ok()?;
        let s = hms.next()?.parse::<u32>().ok()?;
        return utc_timestamp(year, month, day, h, m, s);
    }
    // asctime: "Sun Nov  6 08:49:37 1994"
    if tokens.len() >= 4 {
        let start = usize::from(month_index(tokens[0]).is_none());
        if tokens.len() >= start + 4 {
            let month = month_index(tokens[start])?;
            let day = tokens[start + 1].parse::<u32>().ok()?;
            let mut hms = tokens[start + 2].split(':');
            let h = hms.next()?.parse::<u32>().ok()?;
            let m = hms.next()?.parse::<u32>().ok()?;
            let s = hms.next()?.parse::<u32>().ok()?;
            let year = tokens[start + 3].parse::<i64>().ok()?;
            return utc_timestamp(year, month, day, h, m, s);
        }
    }
    None
}

pub(crate) fn parse_set_cookie(header_val: &str, req_url: &ParsedUrl) -> Option<Cookie> {
    let mut parts = header_val.split(';');
    let name_val = parts.next()?.trim();
    let (name_raw, value_raw) = name_val.split_once('=')?;
    let name = name_raw.trim();
    if name.is_empty() {
        return None;
    }
    let value = value_raw.trim();

    let mut domain_attr: Option<String> = None;
    let mut path_attr: Option<String> = None;
    let mut max_age: Option<i64> = None;
    let mut expires_attr: Option<f64> = None;
    let mut http_only = false;
    let mut secure = false;
    let mut same_site = "Lax".to_string();

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        if let Some((k, v)) = attr.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("domain") {
                if !v.is_empty() {
                    domain_attr = Some(v.to_string());
                }
            } else if k.eq_ignore_ascii_case("path") {
                if v.starts_with('/') {
                    path_attr = Some(v.to_string());
                }
            } else if k.eq_ignore_ascii_case("max-age") {
                if let Ok(secs) = v.parse::<i64>() {
                    max_age = Some(secs);
                }
            } else if k.eq_ignore_ascii_case("expires") {
                if let Some(ts) = parse_http_date(v) {
                    // Use 1.0 (not -1.0 which encodes session cookie) for past timestamps so Chrome deletes it.
                    expires_attr = Some(if ts <= 0.0 { 1.0 } else { ts });
                }
            } else if k.eq_ignore_ascii_case("samesite") {
                if v.eq_ignore_ascii_case("strict") {
                    same_site = "Strict".to_string();
                } else if v.eq_ignore_ascii_case("none") {
                    same_site = "None".to_string();
                } else if v.eq_ignore_ascii_case("lax") {
                    same_site = "Lax".to_string();
                }
            }
        } else if attr.eq_ignore_ascii_case("httponly") {
            http_only = true;
        } else if attr.eq_ignore_ascii_case("secure") {
            secure = true;
        }
    }

    let domain = if let Some(raw_dom) = domain_attr {
        let dom = raw_dom.trim_start_matches('.').to_ascii_lowercase();
        if dom.is_empty() {
            return None;
        }
        let req_host = req_url.host.as_str();
        if is_ip_host(req_host) {
            if req_host != dom {
                return None;
            }
            req_host.to_string()
        } else if req_host == dom || req_host.ends_with(&format!(".{dom}")) {
            format!(".{dom}")
        } else {
            return None;
        }
    } else {
        req_url.host.clone()
    };

    let expires = if let Some(secs) = max_age {
        if secs <= 0 {
            1.0
        } else {
            now_unix_secs() + (secs as f64)
        }
    } else {
        expires_attr.unwrap_or(-1.0)
    };

    Some(Cookie {
        name: name.to_string(),
        value: value.to_string(),
        domain,
        path: path_attr.unwrap_or_else(|| default_cookie_path(&req_url.path)),
        expires,
        http_only,
        secure,
        same_site,
        partition_key: None,
        cross_site_ancestor: None,
    })
}

/// Executes an HTTP/HTTPS request sharing cookies with `context_id` in `conn`.
pub(crate) async fn send_context_request(
    conn: &Connection,
    context_id: &str,
    defaults: &HttpContextDefaults,
    url: &str,
    opts: &ApiRequestOptions,
    inherit_headers: Option<&[(String, String)]>,
) -> Result<ApiResponse> {
    let resolved = resolve_url(defaults.base_url.as_deref(), url)?;
    let initial_url = append_query_params(&resolved, &opts.params);
    let timeout = match opts.timeout {
        Some(d) if d.is_zero() => None,
        Some(d) => Some(d),
        None => Some(DEFAULT_TIMEOUT),
    };
    let work = execute_with_redirects(conn, context_id, defaults, initial_url, opts, inherit_headers);
    let resp = match timeout {
        Some(limit) => tokio::time::timeout(limit, work)
            .await
            .map_err(|_| Error::timeout("APIRequestContext.fetch", limit.as_millis() as u64))??,
        None => work.await?,
    };
    if opts.fail_on_status_code && !(200..400).contains(&resp.status) {
        return Err(Error::Other(format!(
            "{} {}: {}",
            resp.status, resp.status_text, resp.url
        )));
    }
    Ok(resp)
}

async fn execute_with_redirects(
    conn: &Connection,
    context_id: &str,
    defaults: &HttpContextDefaults,
    initial_url: String,
    opts: &ApiRequestOptions,
    inherit_headers: Option<&[(String, String)]>,
) -> Result<ApiResponse> {
    let max_redirects = opts.max_redirects.unwrap_or(DEFAULT_MAX_REDIRECTS);
    let mut current_url = initial_url;
    let mut method = opts
        .method
        .as_deref()
        .unwrap_or("GET")
        .trim()
        .to_ascii_uppercase();
    let mut body = opts.data.clone();

    let mut base_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in &defaults.extra_http_headers {
        set_header_ci(&mut base_headers, k, v.clone());
    }
    if let Some(inherited) = inherit_headers {
        for (k, v) in inherited {
            set_header_ci(&mut base_headers, k, v.clone());
        }
    }
    for (k, v) in &opts.headers {
        set_header_ci(&mut base_headers, k, v.clone());
    }
    if !has_header_ci(&base_headers, "user-agent") {
        let ua = defaults
            .user_agent
            .clone()
            .unwrap_or_else(|| "Fluxwright".to_string());
        set_header_ci(&mut base_headers, "User-Agent", ua);
    }
    if !has_header_ci(&base_headers, "authorization") {
        if let Some((user, pass)) = &defaults.http_credentials {
            set_header_ci(
                &mut base_headers,
                "Authorization",
                basic_auth_header(user, Some(pass.as_str())),
            );
        }
    }

    let mut hops = 0usize;
    loop {
        let parsed = ParsedUrl::parse(&current_url)?;
        let mut hop_headers = base_headers.clone();

        if !has_header_ci(&hop_headers, "cookie") {
            let get_params = if context_id.is_empty() {
                json!({})
            } else {
                json!({ "browserContextId": context_id })
            };
            let res = conn
                .call(
                    "Storage.getCookies",
                    get_params,
                    None,
                    Duration::from_secs(10),
                )
                .await?;
            let all_cookies: Vec<Cookie> = res["cookies"]
                .as_array()
                .map(|a| a.iter().filter_map(Cookie::from_cdp).collect())
                .unwrap_or_default();
            let now = now_unix_secs();
            let matching = filter_cookies(&all_cookies, std::slice::from_ref(&current_url))?;
            let pairs: Vec<String> = matching
                .into_iter()
                .filter(|c| c.expires < 0.0 || c.expires > now)
                .map(|c| format!("{}={}", c.name, c.value))
                .collect();
            if !pairs.is_empty() {
                set_header_ci(&mut hop_headers, "Cookie", pairs.join("; "));
            }
        }

        let (status, status_text, resp_headers, resp_body) = send_single_hop(
            &parsed,
            &method,
            hop_headers,
            body.as_deref(),
            defaults.proxy.as_ref(),
        )
        .await?;

        let set_cookies: Vec<Value> = resp_headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .filter_map(|(_, v)| parse_set_cookie(v, &parsed))
            .map(|c| c.to_cdp())
            .collect();
        if !set_cookies.is_empty() {
            let mut set_params = json!({ "cookies": set_cookies });
            if !context_id.is_empty() {
                set_params["browserContextId"] = json!(context_id);
            }
            conn.call(
                "Storage.setCookies",
                set_params,
                None,
                Duration::from_secs(10),
            )
            .await?;
        }

        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if let Some(location) = find_header_ci(&resp_headers, "location") {
                if max_redirects == 0 {
                    return Ok(ApiResponse {
                        url: current_url,
                        status,
                        status_text,
                        headers: resp_headers,
                        body: resp_body,
                    });
                }
                if hops >= max_redirects {
                    return Err(Error::Other("Max redirect count exceeded".into()));
                }
                hops += 1;
                let next_url = resolve_url(Some(&current_url), location.trim())?;
                let next_parsed = ParsedUrl::parse(&next_url)?;
                if next_parsed.origin_key() != parsed.origin_key() {
                    base_headers.retain(|(k, _)| {
                        !k.eq_ignore_ascii_case("authorization")
                            && !k.eq_ignore_ascii_case("cookie")
                    });
                }
                if status == 303 || ((status == 301 || status == 302) && method == "POST") {
                    method = "GET".to_string();
                    body = None;
                    base_headers.retain(|(k, _)| {
                        !k.eq_ignore_ascii_case("content-type")
                            && !k.eq_ignore_ascii_case("content-length")
                    });
                }
                current_url = next_url;
                continue;
            }
        }

        return Ok(ApiResponse {
            url: current_url,
            status,
            status_text,
            headers: resp_headers,
            body: resp_body,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn resolves_and_appends_urls() {
        assert_eq!(
            resolve_url(Some("https://example.com:8443/app/page.html?a=1"), "../api/users").unwrap(),
            "https://example.com:8443/api/users"
        );
        assert_eq!(
            resolve_url(Some("http://127.0.0.1:8080/a/b"), "/root").unwrap(),
            "http://127.0.0.1:8080/root"
        );
        assert_eq!(
            resolve_url(Some("https://example.com/v1"), "https://other.test:443/x").unwrap(),
            "https://other.test/x"
        );
        assert_eq!(
            append_query_params(
                "http://example.com/search?q=1",
                &[("tag".into(), "a b&c".into())]
            ),
            "http://example.com/search?q=1&tag=a%20b%26c"
        );
    }

    #[test]
    fn parses_set_cookie_attributes_and_validates_domain() {
        let url = ParsedUrl::parse("https://app.example.com/users/profile").unwrap();
        let c = parse_set_cookie(
            "sid=abc=123; Domain=example.com; Path=/users; HttpOnly; Secure; SameSite=Strict",
            &url,
        )
        .unwrap();
        assert_eq!(c.name, "sid");
        assert_eq!(c.value, "abc=123");
        assert_eq!(c.domain, ".example.com");
        assert_eq!(c.path, "/users");
        assert!(c.http_only);
        assert!(c.secure);
        assert_eq!(c.same_site, "Strict");
        assert_eq!(c.expires, -1.0);

        let expired = parse_set_cookie("sid=gone; Max-Age=0", &url).unwrap();
        assert_eq!(expired.domain, "app.example.com");
        assert_eq!(expired.expires, 1.0);

        let past = parse_set_cookie("sid=old; Expires=Thu, 01 Jan 1970 00:00:00 GMT", &url).unwrap();
        assert_eq!(past.expires, 1.0);

        assert!(parse_set_cookie("bad=1; Domain=evil.com", &url).is_none());
    }

    #[test]
    fn decodes_chunked_encoding_and_rejects_crlf() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(decode_chunked_body(raw).unwrap(), b"hello world");
        assert!(decode_chunked_body(b"5\r\nhel").is_err());
        assert!(validate_token_clean("header", "ok\r\nInjected: 1").is_err());
    }

    #[tokio::test]
    async fn single_hop_handles_chunked_and_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            assert!(req.starts_with("POST /echo HTTP/1.1\r\n"));
            assert!(req.to_ascii_lowercase().contains("x-custom: 42\r\n"));
            let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-One: a\r\nX-One: b\r\n\r\n4\r\npong\r\n0\r\n\r\n";
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let target = ParsedUrl::parse(&format!("http://{addr}/echo")).unwrap();
        let (status, reason, headers, body) = send_single_hop(
            &target,
            "POST",
            vec![("X-Custom".into(), "42".into())],
            Some(b"ping"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(reason, "OK");
        assert_eq!(body, b"pong");
        let r = ApiResponse {
            url: target.canonical(),
            status,
            status_text: reason,
            headers,
            body,
        };
        assert!(r.ok());
        assert_eq!(r.headers_map().get("x-one").map(String::as_str), Some("a, b"));
        srv.await.unwrap();
    }
}
