//! Multi-tenant bearer token authentication, per-tenant sliding-window rate limiting,
//! concurrency quota lookup, and Prometheus metric snapshots.

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::future::{ready, Future};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::Extensions;
use serde::Deserialize;

use crate::{same_token, validate_token, ConfigError};

pub(crate) const DEFAULT_TENANT: &str = "default";
const MAX_TENANTS: usize = 256;
const MAX_TENANTS_FILE_BYTES: u64 = 1024 * 1024;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Configuration for one non-admin tenant authenticated by its own bearer token.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub id: String,
    pub token: String,
    #[serde(default)]
    pub max_concurrent_jobs: Option<usize>,
    #[serde(default)]
    pub rate_limit_per_min: Option<u32>,
}

impl fmt::Debug for TenantConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantConfig")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .field("max_concurrent_jobs", &self.max_concurrent_jobs)
            .field("rate_limit_per_min", &self.rate_limit_per_min)
            .finish()
    }
}

/// Authenticated caller identity attached to request extensions by the `authorize` middleware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TenantIdentity {
    pub(crate) id: String,
    pub(crate) is_admin: bool,
}

impl TenantIdentity {
    pub(crate) fn admin_default() -> Self {
        Self {
            id: DEFAULT_TENANT.to_string(),
            is_admin: true,
        }
    }

    pub(crate) fn can_access(&self, owner: &str) -> bool {
        self.is_admin || self.id == owner
    }
}

impl<S: Send + Sync> FromRequestParts<S> for TenantIdentity {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        ready(Ok(tenant_from_ext(&parts.extensions)))
    }
}

pub(crate) fn tenant_from_ext(ext: &Extensions) -> TenantIdentity {
    ext.get::<TenantIdentity>()
        .cloned()
        .unwrap_or_else(TenantIdentity::admin_default)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TenantMetricRow {
    pub(crate) tenant: String,
    pub(crate) requests_total: u64,
    pub(crate) rate_limited_total: u64,
    pub(crate) active_jobs: usize,
}

struct TenantEntry {
    id: String,
    token: Vec<u8>,
    max_concurrent_jobs: Option<usize>,
    rate_limit_per_min: Option<u32>,
}

#[derive(Default)]
struct TenantRuntime {
    window: VecDeque<Instant>,
    requests_total: u64,
    rate_limited_total: u64,
}

pub(crate) struct TenantRegistry {
    entries: Vec<TenantEntry>,
    state: Mutex<HashMap<String, TenantRuntime>>,
}

fn is_valid_tenant_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id != DEFAULT_TENANT
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn validate_tenants(
    tenants: &[TenantConfig],
    admin_token: &[u8],
) -> Result<(), ConfigError> {
    if tenants.len() > MAX_TENANTS {
        return Err(ConfigError::InvalidTenants(format!(
            "at most {MAX_TENANTS} tenants are supported"
        )));
    }
    let mut seen_ids: HashSet<&str> = HashSet::with_capacity(tenants.len());
    let mut seen_tokens: Vec<&[u8]> = Vec::with_capacity(tenants.len());
    for cfg in tenants {
        if !is_valid_tenant_id(&cfg.id) {
            return Err(ConfigError::InvalidTenants(format!(
                "invalid tenant id '{}': must be 1-64 ASCII [A-Za-z0-9._-] and not '{DEFAULT_TENANT}'",
                cfg.id
            )));
        }
        if !seen_ids.insert(cfg.id.as_str()) {
            return Err(ConfigError::InvalidTenants(format!(
                "duplicate tenant id '{}'",
                cfg.id
            )));
        }
        if validate_token(&cfg.token).is_err() {
            return Err(ConfigError::InvalidTenants(format!(
                "tenant '{}' token must be 32-256 printable ASCII characters without whitespace",
                cfg.id
            )));
        }
        if cfg.max_concurrent_jobs == Some(0) {
            return Err(ConfigError::InvalidTenants(format!(
                "tenant '{}' max_concurrent_jobs must be at least 1",
                cfg.id
            )));
        }
        if cfg.rate_limit_per_min == Some(0) {
            return Err(ConfigError::InvalidTenants(format!(
                "tenant '{}' rate_limit_per_min must be at least 1",
                cfg.id
            )));
        }
        let tok_bytes = cfg.token.as_bytes();
        if same_token(tok_bytes, admin_token) {
            return Err(ConfigError::InvalidTenants(format!(
                "tenant '{}' token must not duplicate the primary admin token",
                cfg.id
            )));
        }
        if seen_tokens.iter().any(|prev| same_token(tok_bytes, prev)) {
            return Err(ConfigError::InvalidTenants(format!(
                "tenant '{}' token duplicates another tenant token",
                cfg.id
            )));
        }
        seen_tokens.push(tok_bytes);
    }
    Ok(())
}

/// Loads and parses a JSON array of [`TenantConfig`] entries (capped at 1 MiB, max 256 tenants).
pub fn load_tenants_file(path: &Path) -> Result<Vec<TenantConfig>, ConfigError> {
    let file = std::fs::File::open(path).map_err(|e| {
        ConfigError::InvalidTenants(format!(
            "failed to open tenants file '{}': {e}",
            path.display()
        ))
    })?;
    let mut buf = Vec::new();
    file.take(MAX_TENANTS_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| {
            ConfigError::InvalidTenants(format!(
                "failed to read tenants file '{}': {e}",
                path.display()
            ))
        })?;
    if buf.len() as u64 > MAX_TENANTS_FILE_BYTES {
        return Err(ConfigError::InvalidTenants(format!(
            "tenants file '{}' exceeds 1 MiB limit",
            path.display()
        )));
    }
    let configs: Vec<TenantConfig> = serde_json::from_slice(&buf).map_err(|e| {
        ConfigError::InvalidTenants(format!(
            "invalid JSON in tenants file '{}': {e}",
            path.display()
        ))
    })?;
    if configs.len() > MAX_TENANTS {
        return Err(ConfigError::InvalidTenants(format!(
            "tenants file '{}' exceeds maximum of {MAX_TENANTS} tenants",
            path.display()
        )));
    }
    Ok(configs)
}

impl TenantRegistry {
    pub(crate) fn new(tenants: Vec<TenantConfig>, admin_token: &[u8]) -> Result<Self, ConfigError> {
        validate_tenants(&tenants, admin_token)?;
        let mut state = HashMap::with_capacity(tenants.len());
        let mut entries = Vec::with_capacity(tenants.len());
        for cfg in tenants {
            state.insert(cfg.id.clone(), TenantRuntime::default());
            entries.push(TenantEntry {
                id: cfg.id,
                token: cfg.token.into_bytes(),
                max_concurrent_jobs: cfg.max_concurrent_jobs,
                rate_limit_per_min: cfg.rate_limit_per_min,
            });
        }
        Ok(Self {
            entries,
            state: Mutex::new(state),
        })
    }

    /// Constant-time token lookup against the primary admin token and all configured tenants.
    pub(crate) fn authenticate(&self, supplied: &[u8], admin_token: &[u8]) -> Option<TenantIdentity> {
        let is_admin = same_token(supplied, admin_token);
        let mut matched_tenant: Option<&str> = None;
        for entry in &self.entries {
            if same_token(supplied, &entry.token) {
                matched_tenant = Some(entry.id.as_str());
            }
        }
        if is_admin {
            Some(TenantIdentity::admin_default())
        } else {
            matched_tenant.map(|id| TenantIdentity {
                id: id.to_string(),
                is_admin: false,
            })
        }
    }

    /// Enforces the 60-second sliding-window rate limit for `tenant_id`.
    /// Returns `Err(retry_after_secs)` when the configured `rate_limit_per_min` is exceeded.
    pub(crate) fn check_rate_limit(&self, tenant_id: &str) -> Result<(), u64> {
        if self.entries.is_empty() {
            return Ok(());
        }
        let limit = self
            .entries
            .iter()
            .find(|e| e.id == tenant_id)
            .and_then(|e| e.rate_limit_per_min);

        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let rt = state.entry(tenant_id.to_string()).or_default();
        while rt
            .window
            .front()
            .is_some_and(|ts| now.saturating_duration_since(*ts) >= RATE_LIMIT_WINDOW)
        {
            rt.window.pop_front();
        }
        if let Some(max_per_min) = limit {
            if rt.window.len() >= max_per_min as usize {
                rt.rate_limited_total += 1;
                let retry_after = rt
                    .window
                    .front()
                    .map(|oldest| {
                        let rem = RATE_LIMIT_WINDOW
                            .saturating_sub(now.saturating_duration_since(*oldest));
                        let secs = rem.as_secs() + u64::from(rem.subsec_nanos() > 0);
                        secs.max(1)
                    })
                    .unwrap_or(1);
                return Err(retry_after);
            }
            rt.window.push_back(now);
        }
        rt.requests_total += 1;
        Ok(())
    }

    pub(crate) fn max_concurrent_jobs(&self, tenant_id: &str) -> Option<usize> {
        self.entries
            .iter()
            .find(|e| e.id == tenant_id)
            .and_then(|e| e.max_concurrent_jobs)
    }

    pub(crate) fn prometheus_snapshot(
        &self,
        active_jobs_by_tenant: &HashMap<String, usize>,
    ) -> Vec<TenantMetricRow> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<TenantMetricRow> = Vec::with_capacity(state.len().max(self.entries.len()));
        let mut seen: HashSet<&str> = HashSet::new();
        for entry in &self.entries {
            seen.insert(entry.id.as_str());
            let rt = state.get(&entry.id);
            rows.push(TenantMetricRow {
                tenant: entry.id.clone(),
                requests_total: rt.map_or(0, |r| r.requests_total),
                rate_limited_total: rt.map_or(0, |r| r.rate_limited_total),
                active_jobs: active_jobs_by_tenant.get(&entry.id).copied().unwrap_or(0),
            });
        }
        // Include admin "default" tenant when it has recorded activity or active jobs.
        if !seen.contains(DEFAULT_TENANT) {
            let rt = state.get(DEFAULT_TENANT);
            let active = active_jobs_by_tenant
                .get(DEFAULT_TENANT)
                .copied()
                .unwrap_or(0);
            let reqs = rt.map_or(0, |r| r.requests_total);
            let limited = rt.map_or(0, |r| r.rate_limited_total);
            if reqs > 0 || limited > 0 || active > 0 {
                rows.push(TenantMetricRow {
                    tenant: DEFAULT_TENANT.to_string(),
                    requests_total: reqs,
                    rate_limited_total: limited,
                    active_jobs: active,
                });
            }
        }
        rows.sort_by(|a, b| a.tenant.cmp(&b.tenant));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADMIN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TENANT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TENANT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn tenant_config_debug_redacts_token() {
        let cfg = TenantConfig {
            id: "acme".into(),
            token: TENANT_A.into(),
            max_concurrent_jobs: Some(4),
            rate_limit_per_min: Some(60),
        };
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("acme"));
        assert!(dbg.contains("[REDACTED]"));
        assert!(!dbg.contains(TENANT_A));
    }

    #[test]
    fn validates_ids_tokens_and_uniqueness() {
        let good = vec![
            TenantConfig {
                id: "acme-1.prod_a".into(),
                token: TENANT_A.into(),
                max_concurrent_jobs: Some(2),
                rate_limit_per_min: Some(10),
            },
            TenantConfig {
                id: "beta".into(),
                token: TENANT_B.into(),
                max_concurrent_jobs: None,
                rate_limit_per_min: None,
            },
        ];
        assert!(validate_tenants(&good, ADMIN.as_bytes()).is_ok());

        for bad_id in ["", "default", "bad id", "bad/id", &"a".repeat(65)] {
            let cfg = vec![TenantConfig {
                id: bad_id.into(),
                token: TENANT_A.into(),
                max_concurrent_jobs: None,
                rate_limit_per_min: None,
            }];
            assert!(validate_tenants(&cfg, ADMIN.as_bytes()).is_err(), "{bad_id}");
        }

        // Duplicate admin token rejected
        assert!(validate_tenants(
            &[TenantConfig {
                id: "acme".into(),
                token: ADMIN.into(),
                max_concurrent_jobs: None,
                rate_limit_per_min: None,
            }],
            ADMIN.as_bytes()
        )
        .is_err());

        // Duplicate tenant token rejected
        assert!(validate_tenants(
            &[
                TenantConfig {
                    id: "a".into(),
                    token: TENANT_A.into(),
                    max_concurrent_jobs: None,
                    rate_limit_per_min: None,
                },
                TenantConfig {
                    id: "b".into(),
                    token: TENANT_A.into(),
                    max_concurrent_jobs: None,
                    rate_limit_per_min: None,
                },
            ],
            ADMIN.as_bytes()
        )
        .is_err());
    }
}
