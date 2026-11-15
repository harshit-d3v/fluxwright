//! Blocks requests to hosts that must never be reached from a job: link-local addresses
//! (the cloud metadata service hands out instance credentials) and `file:` pages. Shared by
//! the MCP server and the HTTP server; each caller picks its own opt-out env var.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use fluxwright_cdp::InterceptedRequest;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::pool::PageLease;
use crate::Result;

/// Whether `url` may be navigated to or fetched at all: web pages only, and never a
/// link-local host. `about:blank` passes (it resolves to nothing).
pub fn url_allowed(url: &str) -> std::result::Result<(), &'static str> {
    // Browsers drop tabs and newlines anywhere in a URL.
    let url: String = url
        .trim()
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if url.eq_ignore_ascii_case("about:blank") {
        return Ok(());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("only http and https URLs are allowed")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err("only http and https URLs are allowed");
    }
    if host_blocked(rest) {
        return Err("link-local hosts (cloud metadata) are blocked");
    }
    Ok(())
}

/// For a request URL as Chrome wrote it: web requests to a blocked host, and `file:`.
pub fn request_blocked(url: &str) -> bool {
    match url.split_once("://") {
        Some(("http" | "https" | "ws" | "wss", rest)) => host_blocked(rest),
        _ => url.starts_with("file:"),
    }
}

/// Every request the page makes (subresources, redirects, navigations a script or click
/// starts) is checked against the host rules, on the URL as Chrome parsed it: nothing the
/// job's steps can slip past through a parser difference, a redirect or `evaluate`.
pub async fn guard_requests(page: &PageLease) -> Result<()> {
    let requests = page.intercept().await?;
    tokio::spawn(serve_guarded(requests));
    Ok(())
}

/// Per-host DNS verdicts, kept for the life of the page.
type Verdicts = Arc<Mutex<HashMap<String, bool>>>;

async fn serve_guarded(mut requests: UnboundedReceiver<InterceptedRequest>) {
    let verdicts = Verdicts::default();
    while let Some(req) = requests.recv().await {
        // One task per request: a slow DNS answer for one host holds up no other request.
        let verdicts = verdicts.clone();
        tokio::spawn(async move {
            if request_blocked(&req.url) || resolves_to_blocked(&req.url, &verdicts).await {
                tracing::warn!(url = %req.url, "blocked a request to a link-local host");
                let _ = req.abort("blockedbyclient").await;
            }
            // Any other request is dropped here, which lets it through.
        });
    }
}

/// The host of a web request URL, lower-cased, without port, userinfo, path or query.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => host.split(':').next()?,
    };
    Some(host.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether the URL's host resolves to a link-local address. Catches names that
/// `request_blocked`'s literal check misses; a DNS answer can still change between this check
/// and Chrome's own connect (rebinding), so deployments should also block these ranges at the
/// network level — see docs/SERVER.md.
async fn resolves_to_blocked(url: &str, verdicts: &Verdicts) -> bool {
    let Some(scheme) = url.split("://").next() else {
        return false;
    };
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return false;
    }
    let Some(host) = url_host(url).filter(|h| !h.is_empty()) else {
        return false;
    };
    if let Some(&blocked) = verdicts.lock().unwrap().get(&host) {
        return blocked;
    }
    // Unresolvable here does not mean unreachable for Chrome; only a proven bad answer blocks.
    // A failed lookup is not cached: the next request for the host asks again.
    let blocked = match tokio::net::lookup_host((host.as_str(), 80)).await {
        Ok(mut addrs) => addrs.any(|addr| addr_is_blocked(addr.ip())),
        Err(_) => return false,
    };
    verdicts.lock().unwrap().insert(host, blocked);
    blocked
}

/// Link-local (which holds the cloud metadata service) and AWS's IPv6 metadata address.
/// Loopback and private ranges stay reachable: local fixtures and intranet pages are real jobs.
fn addr_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.segments()[0] & 0xffc0 == 0xfe80 // fe80::/10
                || v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) // AWS IMDS over IPv6
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
    }
}

/// Whether the host of `after_scheme` (the URL after `://`) is link-local or a cloud metadata
/// name.
fn host_blocked(after_scheme: &str) -> bool {
    // Browsers read `\` as `/` in web URLs.
    let authority = after_scheme
        .split(['/', '?', '#', '\\'])
        .next()
        .unwrap_or("");
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
    .trim_end_matches('.');
    if host == "metadata.google.internal" || host == "metadata" {
        return true;
    }
    match host.parse::<Ipv6Addr>() {
        Ok(v6) => addr_is_blocked(v6.into()),
        Err(_) => ipv4_host(host).is_some_and(|v4| addr_is_blocked(v4.into())),
    }
}

/// A host read as IPv4 the way browsers do: 1 to 4 dot-separated numbers in decimal, hex (0x)
/// or octal (leading 0), the last one filling the remaining bytes. So `2852039166` and
/// `0xa9.0xfe.0xa9.0xfe` are both 169.254.169.254. None for a name.
pub fn ipv4_host(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in parts {
        let n = if let Some(hex) = p.strip_prefix("0x") {
            if hex.is_empty() {
                0
            } else {
                u64::from_str_radix(hex, 16).ok()?
            }
        } else if p.len() > 1 && p.starts_with('0') {
            u64::from_str_radix(&p[1..], 8).ok()?
        } else {
            p.parse::<u64>().ok()?
        };
        nums.push(n);
    }
    let (&last, head) = nums.split_last()?;
    if head.iter().any(|&n| n > 255) || last >= 256u64.pow(5 - nums.len() as u32) {
        return None;
    }
    let value = head
        .iter()
        .enumerate()
        .fold(last, |v, (i, &n)| v + (n << (8 * (3 - i))));
    Some(Ipv4Addr::from(value as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_link_local_and_metadata() {
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "http://2852039166/",
            "http://0xa9.0xfe.0xa9.0xfe/",
            "http://[::ffff:169.254.169.254]/",
            "http://[fe80::1]/",
            "http://metadata.google.internal/",
            "https://example.com@169.254.169.254/",
            "file:///etc/passwd",
        ] {
            assert!(request_blocked(url), "{url}");
            assert!(url_allowed(url).is_err(), "{url}");
        }
        for url in [
            "https://example.com/",
            "http://localhost:8080/",
            "http://192.168.1.1/",
            "about:blank",
        ] {
            assert!(!request_blocked(url), "{url}");
            assert!(url_allowed(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn resolved_addresses_block_link_local_only() {
        for ip in [
            "169.254.169.254",
            "fe80::1",
            "fd00:ec2::254",
            "::ffff:169.254.1.1",
        ] {
            assert!(addr_is_blocked(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "::1",
            "10.0.0.1",
            "192.168.1.1",
            "fc00::1",
            "fd00::1",
            "8.8.8.8",
        ] {
            assert!(!addr_is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }
}
