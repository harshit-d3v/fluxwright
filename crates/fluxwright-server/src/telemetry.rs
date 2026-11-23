//! W3C TraceContext extraction and OpenTelemetry trace instrumentation.
//!
//! Follows the W3C TraceContext specification (the `traceparent` header):
//! `traceparent = version "-" trace_id "-" parent_id "-" trace_flags`
//! Example: `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`

use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

/// Parsed or generated W3C TraceContext for a job execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTraceContext {
    /// 32-character lowercase hex string (16 bytes).
    pub trace_id: String,
    /// 16-character lowercase hex string of the incoming parent span ID (if provided).
    pub parent_span_id: Option<String>,
    /// 16-character lowercase hex string of this job's root span ID.
    pub root_span_id: String,
    /// 2-character hex flags (e.g. "01" for sampled).
    pub trace_flags: String,
}

impl JobTraceContext {
    /// Extracts a W3C TraceContext from the `traceparent` header, or generates a fresh trace ID
    /// and root span ID if it is missing or malformed.
    pub fn from_headers(traceparent: Option<&str>) -> Self {
        if let Some(tp) = traceparent {
            if let Some(ctx) = parse_traceparent(tp.trim()) {
                let root_span_id = generate_span_id();
                return Self {
                    trace_id: ctx.trace_id,
                    parent_span_id: Some(ctx.parent_span_id),
                    root_span_id,
                    trace_flags: ctx.trace_flags,
                };
            }
        }

        Self::new()
    }

    /// Generates a fresh W3C TraceContext with high entropy.
    pub fn new() -> Self {
        Self {
            trace_id: generate_trace_id(),
            parent_span_id: None,
            root_span_id: generate_span_id(),
            trace_flags: "01".to_string(),
        }
    }

    /// Formats the context into a standard W3C `traceparent` header string for downstream requests.
    pub fn to_traceparent(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id, self.root_span_id, self.trace_flags)
    }
}

impl Default for JobTraceContext {
    fn default() -> Self {
        Self::new()
    }
}

struct ParsedTraceparent {
    trace_id: String,
    parent_span_id: String,
    trace_flags: String,
}

fn parse_traceparent(s: &str) -> Option<ParsedTraceparent> {
    // Exact format: 00-32hex-16hex-2hex (length 55 chars)
    if s.len() != 55 {
        return None;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 4 {
        return None;
    }

    let version = parts[0];
    let trace_id = parts[1];
    let parent_id = parts[2];
    let flags = parts[3];

    // Version must be valid hex, currently "00" is supported (or forward compatible non-FF)
    if version == "ff" || !is_hex(version, 2) {
        return None;
    }

    // Trace ID must be 32 hex chars and not all zeroes
    if !is_hex(trace_id, 32) || trace_id.chars().all(|c| c == '0') {
        return None;
    }

    // Parent span ID must be 16 hex chars and not all zeroes
    if !is_hex(parent_id, 16) || parent_id.chars().all(|c| c == '0') {
        return None;
    }

    // Flags must be 2 hex chars
    if !is_hex(flags, 2) {
        return None;
    }

    Some(ParsedTraceparent {
        trace_id: trace_id.to_ascii_lowercase(),
        parent_span_id: parent_id.to_ascii_lowercase(),
        trace_flags: flags.to_ascii_lowercase(),
    })
}

fn is_hex(s: &str, expected_len: usize) -> bool {
    s.len() == expected_len && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Generates a random 128-bit trace ID formatted as a 32-character lowercase hex string.
pub fn generate_trace_id() -> String {
    let mut id = Uuid::new_v4().as_u128();
    if id == 0 {
        id = 1;
    }
    format!("{id:032x}")
}

/// Generates a random 64-bit span ID formatted as a 16-character lowercase hex string.
pub fn generate_span_id() -> String {
    let mut id = (Uuid::new_v4().as_u128() >> 64) as u64;
    if id == 0 {
        id = 1;
    }
    format!("{id:016x}")
}

/// Returns the current Unix timestamp in nanoseconds.
pub(crate) fn now_unix_nano() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Converts a Unix epoch millisecond timestamp to nanoseconds.
pub(crate) fn ms_to_unix_nano(ms: u64) -> u128 {
    u128::from(ms) * 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_traceparent() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let ctx = JobTraceContext::from_headers(Some(tp));
        assert_eq!(ctx.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(ctx.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert_eq!(ctx.trace_flags, "01");
        assert_eq!(ctx.root_span_id.len(), 16);
    }

    #[test]
    fn generates_fresh_on_missing_or_invalid_traceparent() {
        for invalid in [
            "",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01", // all zeroes trace_id
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01", // all zeroes parent_id
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // invalid version
            "not-a-traceparent",
            "00-short-id-01",
        ] {
            let ctx = JobTraceContext::from_headers(Some(invalid));
            assert_eq!(ctx.trace_id.len(), 32);
            assert_eq!(ctx.root_span_id.len(), 16);
            assert!(ctx.parent_span_id.is_none());
            assert_eq!(ctx.trace_flags, "01");
        }
    }

    #[test]
    fn formats_outbound_traceparent() {
        let ctx = JobTraceContext {
            trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".into(),
            parent_span_id: None,
            root_span_id: "00f067aa0ba902b7".into(),
            trace_flags: "01".into(),
        };
        assert_eq!(
            ctx.to_traceparent(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn generates_valid_hex_trace_and_span_ids() {
        let tid = generate_trace_id();
        let sid = generate_span_id();
        assert_eq!(tid.len(), 32);
        assert_eq!(sid.len(), 16);
        assert!(tid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert!(sid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }
}
