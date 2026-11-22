//! Proxy rotation pool with sticky session affinity and exponential failure cooldown.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fluxwright_cdp::Proxy;
use serde::Serialize;

use crate::error::{Error, Result};

const DEFAULT_FAILURE_THRESHOLD: u32 = 2;
const DEFAULT_BASE_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_COOLDOWN: Duration = Duration::from_secs(600);
const MAX_STICKY_SESSIONS: usize = 4096;

/// How [`ProxyPool`] selects a proxy when no sticky session binding applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProxyRotation {
    #[default]
    RoundRobin,
    LeastUsed,
}

/// Sanitized health telemetry for a configured proxy (never includes credentials).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyStatus {
    pub server: String,
    pub active: usize,
    pub total_uses: u64,
    pub failures: u64,
    pub consecutive_failures: u32,
    pub healthy: bool,
    pub cooldown_remaining_ms: u64,
}

/// Parses `scheme://[user[:pass]@]host[:port]` (`http`, `https`, `socks5`, `socks5h`) into [`Proxy`].
pub fn parse_proxy_url(raw: &str) -> Result<Proxy> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Other("proxy URL must not be empty or contain whitespace".into()));
    }
    let Some((raw_scheme, rest)) = trimmed.split_once("://") else {
        return Err(Error::Other("proxy URL missing scheme".into()));
    };
    let scheme = raw_scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks5h") {
        return Err(Error::Other(format!(
            "unsupported proxy scheme {raw_scheme:?} (expected http, https, socks5, or socks5h)"
        )));
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(Error::Other(
            "invalid proxy authority: path, query, and fragment are not supported".into(),
        ));
    }
    let (auth, host_port) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    if host_port.is_empty() || host_port.starts_with(':') {
        return Err(Error::Other("missing proxy host".into()));
    }
    let (username, password) = match auth {
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(a.to_string()), None),
        },
        None => (None, None),
    };
    Ok(Proxy {
        server: format!("{scheme}://{host_port}"),
        bypass: None,
        username,
        password,
    })
}

struct SlotState {
    proxy: Proxy,
    active: usize,
    total_uses: u64,
    failures: u64,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

struct PoolState {
    slots: Vec<SlotState>,
    rotation: ProxyRotation,
    failure_threshold: u32,
    base_cooldown: Duration,
    cursor: usize,
    sticky: HashMap<String, usize>,
    sticky_order: VecDeque<String>,
}

/// Shared pool of outbound proxies with sticky session keys and exponential failure cooldown.
#[derive(Clone)]
pub struct ProxyPool {
    inner: Arc<Mutex<PoolState>>,
}

impl fmt::Debug for ProxyPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.inner.lock().unwrap();
        let servers: Vec<&str> = guard.slots.iter().map(|s| s.proxy.server.as_str()).collect();
        f.debug_struct("ProxyPool")
            .field("servers", &servers)
            .field("rotation", &guard.rotation)
            .finish()
    }
}

/// Active proxy assignment tied to one browser context; reports failures or releases active count on drop.
pub struct ProxyLease {
    pool: ProxyPool,
    index: usize,
    proxy: Proxy,
}

impl ProxyLease {
    pub fn proxy(&self) -> &Proxy {
        &self.proxy
    }

    pub fn report_outcome(&self, ok: bool) {
        if ok {
            self.pool.report_success(self.index);
        } else {
            self.pool.report_failure(self.index);
        }
    }
}

impl Drop for ProxyLease {
    fn drop(&mut self) {
        let mut st = self.pool.inner.lock().unwrap();
        if let Some(slot) = st.slots.get_mut(self.index) {
            slot.active = slot.active.saturating_sub(1);
        }
    }
}

impl ProxyPool {
    pub fn new(proxies: Vec<Proxy>) -> Result<Self> {
        if proxies.is_empty() {
            return Err(Error::Other("ProxyPool requires at least one proxy".into()));
        }
        let mut slots = Vec::with_capacity(proxies.len());
        for proxy in proxies {
            if proxy.server.trim().is_empty() {
                return Err(Error::Other("proxy server URL must not be empty".into()));
            }
            slots.push(SlotState {
                proxy,
                active: 0,
                total_uses: 0,
                failures: 0,
                consecutive_failures: 0,
                cooldown_until: None,
            });
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(PoolState {
                slots,
                rotation: ProxyRotation::default(),
                failure_threshold: DEFAULT_FAILURE_THRESHOLD,
                base_cooldown: DEFAULT_BASE_COOLDOWN,
                cursor: 0,
                sticky: HashMap::new(),
                sticky_order: VecDeque::new(),
            })),
        })
    }

    pub fn from_urls<I, S>(urls: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        for url in urls {
            parsed.push(parse_proxy_url(url.as_ref())?);
        }
        Self::new(parsed)
    }

    pub fn with_rotation(self, rotation: ProxyRotation) -> Self {
        self.inner.lock().unwrap().rotation = rotation;
        self
    }

    pub fn with_failure_policy(self, failure_threshold: u32, base_cooldown: Duration) -> Self {
        let mut st = self.inner.lock().unwrap();
        st.failure_threshold = failure_threshold.max(1);
        st.base_cooldown = base_cooldown.max(Duration::from_millis(1));
        self
    }

    pub fn acquire(&self, session_key: Option<&str>) -> ProxyLease {
        self.acquire_at(session_key, Instant::now())
    }

    fn acquire_at(&self, session_key: Option<&str>, now: Instant) -> ProxyLease {
        let mut st = self.inner.lock().unwrap();
        let len = st.slots.len();

        let sticky_pick = session_key
            .and_then(|key| st.sticky.get(key).copied())
            .filter(|&idx| idx < len && is_healthy(&st.slots[idx], now));

        let idx = sticky_pick.unwrap_or_else(|| {
            let healthy: Vec<usize> = (0..len)
                .filter(|&i| is_healthy(&st.slots[i], now))
                .collect();
            if healthy.is_empty() {
                // Graceful fallback: choose the proxy closest to exiting cooldown.
                (0..len)
                    .min_by_key(|&i| (st.slots[i].cooldown_until, st.slots[i].active, i))
                    .unwrap_or(0)
            } else {
                match st.rotation {
                    ProxyRotation::RoundRobin => {
                        let start = st.cursor % len;
                        let mut picked = healthy[0];
                        for offset in 0..len {
                            let cand = (start + offset) % len;
                            if is_healthy(&st.slots[cand], now) {
                                picked = cand;
                                st.cursor = (cand + 1) % len;
                                break;
                            }
                        }
                        picked
                    }
                    ProxyRotation::LeastUsed => *healthy
                        .iter()
                        .min_by_key(|&&i| (st.slots[i].active, st.slots[i].total_uses, i))
                        .unwrap(),
                }
            }
        });

        if let Some(key) = session_key.filter(|k| !k.is_empty()) {
            if !st.sticky.contains_key(key) {
                while st.sticky.len() >= MAX_STICKY_SESSIONS {
                    if let Some(old) = st.sticky_order.pop_front() {
                        st.sticky.remove(&old);
                    } else {
                        break;
                    }
                }
                st.sticky_order.push_back(key.to_string());
            }
            st.sticky.insert(key.to_string(), idx);
        }

        let slot = &mut st.slots[idx];
        slot.active += 1;
        slot.total_uses += 1;
        let proxy = slot.proxy.clone();

        ProxyLease {
            pool: self.clone(),
            index: idx,
            proxy,
        }
    }

    pub fn report_success(&self, index: usize) {
        let mut st = self.inner.lock().unwrap();
        if let Some(slot) = st.slots.get_mut(index) {
            slot.consecutive_failures = 0;
            slot.cooldown_until = None;
        }
    }

    pub fn report_failure(&self, index: usize) {
        self.report_failure_at(index, Instant::now());
    }

    fn report_failure_at(&self, index: usize, now: Instant) {
        let mut st = self.inner.lock().unwrap();
        let threshold = st.failure_threshold;
        let base = st.base_cooldown;
        if let Some(slot) = st.slots.get_mut(index) {
            slot.failures += 1;
            slot.consecutive_failures += 1;
            if slot.consecutive_failures >= threshold {
                let shift = (slot.consecutive_failures - threshold).min(10);
                let factor = 1u32 << shift;
                let backoff = base.saturating_mul(factor).min(MAX_COOLDOWN);
                slot.cooldown_until = Some(now + backoff);
            }
        }
    }

    pub fn stats(&self) -> Vec<ProxyStatus> {
        self.stats_at(Instant::now())
    }

    fn stats_at(&self, now: Instant) -> Vec<ProxyStatus> {
        let st = self.inner.lock().unwrap();
        st.slots
            .iter()
            .map(|slot| {
                let remaining = slot
                    .cooldown_until
                    .and_then(|until| until.checked_duration_since(now))
                    .map_or(0, |d| d.as_millis() as u64);
                ProxyStatus {
                    server: slot.proxy.server.clone(),
                    active: slot.active,
                    total_uses: slot.total_uses,
                    failures: slot.failures,
                    consecutive_failures: slot.consecutive_failures,
                    healthy: remaining == 0,
                    cooldown_remaining_ms: remaining,
                }
            })
            .collect()
    }
}

fn is_healthy(slot: &SlotState, now: Instant) -> bool {
    slot.cooldown_until.is_none_or(|until| now >= until)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_urls_and_rejects_invalid() {
        let p1 = parse_proxy_url("http://user:pass@proxy.internal:8080").unwrap();
        assert_eq!(p1.server, "http://proxy.internal:8080");
        assert_eq!(p1.username.as_deref(), Some("user"));
        assert_eq!(p1.password.as_deref(), Some("pass"));

        let p2 = parse_proxy_url("socks5h://127.0.0.1:1080/").unwrap();
        assert_eq!(p2.server, "socks5h://127.0.0.1:1080");
        assert!(p2.username.is_none());

        for bad in [
            "",
            "proxy.internal:8080",
            "ftp://proxy.internal:21",
            "http://:8080",
            "http://proxy.internal:8080/path",
            "http://bad host:8080",
        ] {
            assert!(parse_proxy_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn round_robin_least_used_sticky_and_cooldown_work() {
        let pool = ProxyPool::from_urls([
            "http://u:secret@p1.example:8000",
            "http://p2.example:8000",
        ])
        .unwrap()
        .with_failure_policy(2, Duration::from_secs(30));

        let t0 = Instant::now();
        let l1 = pool.acquire_at(None, t0);
        let l2 = pool.acquire_at(None, t0);
        assert_ne!(l1.proxy().server, l2.proxy().server);
        drop(l1);
        drop(l2);

        // Sticky session pins to the same healthy proxy across repeated acquires.
        let s1 = pool.acquire_at(Some("acct-a"), t0);
        let pinned_server = s1.proxy().server.clone();
        let pinned_idx = s1.index;
        drop(s1);
        let s2 = pool.acquire_at(Some("acct-a"), t0);
        assert_eq!(s2.proxy().server, pinned_server);
        drop(s2);

        // Failing past threshold puts that proxy in cooldown and fails over.
        pool.report_failure_at(pinned_idx, t0);
        assert!(pool.stats_at(t0)[pinned_idx].healthy);
        pool.report_failure_at(pinned_idx, t0);
        let stats = pool.stats_at(t0);
        assert!(!stats[pinned_idx].healthy);
        assert!(stats[pinned_idx].cooldown_remaining_ms > 0);
        let failover = pool.acquire_at(Some("acct-a"), t0);
        assert_ne!(failover.proxy().server, pinned_server);
        drop(failover);

        // If all proxies cool down, acquire gracefully falls back instead of failing.
        let other_idx = 1 - pinned_idx;
        pool.report_failure_at(other_idx, t0 + Duration::from_secs(5));
        pool.report_failure_at(other_idx, t0 + Duration::from_secs(5));
        let fallback = pool.acquire_at(None, t0 + Duration::from_secs(6));
        assert_eq!(fallback.index, pinned_idx);
        drop(fallback);

        // Success clears cooldown and stats/Debug never expose passwords.
        pool.report_success(pinned_idx);
        assert!(pool.stats_at(t0)[pinned_idx].healthy);
        let serialized = serde_json::to_string(&pool.stats_at(t0)).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!format!("{pool:?}").contains("secret"));

        // LeastUsed mode favors fewer active leases.
        let lu = ProxyPool::from_urls(["http://a:1", "http://b:2"])
            .unwrap()
            .with_rotation(ProxyRotation::LeastUsed);
        let hold = lu.acquire_at(None, t0);
        let next = lu.acquire_at(None, t0);
        assert_ne!(hold.proxy().server, next.proxy().server);
        drop((hold, next));
    }
}
