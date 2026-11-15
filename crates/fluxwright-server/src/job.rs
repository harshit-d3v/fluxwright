use std::time::Duration;

use base64::Engine;
use fluxwright::{
    netguard, Clip, ImageFormat, JobOptions, PageLease, PdfMargin, PdfOptions, Priority,
    ScreenshotOptions, WaitUntil,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The limit bounds retained results, not Chrome's own memory allocations.
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_CONSOLE_ENTRIES: usize = 256;
pub const MAX_CONSOLE_TEXT_BYTES: usize = 2048;
pub const MAX_NETWORK_ENTRIES: usize = 256;
pub const MAX_NETWORK_URL_BYTES: usize = 2048;
pub const MAX_SCREENSHOT_BYTES: usize = MAX_RESULT_BYTES / 4 * 3;
/// Shares the screenshot raw-byte cap: retained job results sit in memory up to `MAX_RESULT_BYTES`
/// (1 MiB) JSON, and base64 encoding adds 4/3 expansion (~768 KiB raw fills the 1 MiB envelope).
pub const MAX_PDF_BYTES: usize = MAX_SCREENSHOT_BYTES;
const MAX_STEPS: usize = 64;

/// Steps run in order on one fresh page. No host-side scripts or filesystem paths are accepted.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub steps: Vec<Step>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Retries replay the entire job, including clicks and other side effects. Off by default.
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub priority: JobPriority,
}

fn default_timeout() -> u64 {
    60_000
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPriority {
    Low,
    #[default]
    Normal,
    High,
}

impl JobRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.steps.is_empty() || self.steps.len() > MAX_STEPS {
            return Err(format!(
                "steps must contain between 1 and {MAX_STEPS} entries"
            ));
        }
        if !(1..=300_000).contains(&self.timeout_ms) {
            return Err("timeout_ms must be between 1 and 300000".into());
        }
        if self.retries > 3 {
            return Err("retries must be between 0 and 3; retries replay the whole job".into());
        }
        for step in &self.steps {
            match step {
                Step::Goto { url, .. } if !web_url(url) => {
                    return Err("goto accepts only http://, https://, or about:blank URLs".into());
                }
                Step::Goto { url, .. } if netguard::url_allowed(url).is_err() => {
                    return Err(
                        "goto url is not allowed (link-local and non-web URLs are blocked)".into(),
                    );
                }
                Step::Click { selector }
                | Step::Fill { selector, .. }
                | Step::WaitForSelector { selector }
                | Step::TextContent { selector }
                    if selector.trim().is_empty() =>
                {
                    return Err("selectors must not be empty".into());
                }
                Step::Evaluate { expression } if expression.trim().is_empty() => {
                    return Err("evaluate expression must not be empty".into());
                }
                Step::Screenshot { .. } => {
                    step.screenshot_options().and_then(|o| o.validate().map_err(|e| e.to_string()))?;
                }
                Step::Pdf { .. } => {
                    step.pdf_options().and_then(|o| o.validate().map_err(|e| e.to_string()))?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn options(&self) -> JobOptions {
        let priority = match self.priority {
            JobPriority::Low => Priority::Low,
            JobPriority::Normal => Priority::Normal,
            JobPriority::High => Priority::High,
        };
        JobOptions::default()
            .priority(priority)
            .retries(self.retries)
            .timeout(Duration::from_millis(self.timeout_ms))
    }
}

fn web_url(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    url.split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && !rest.is_empty()
    })
}

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Goto {
        url: String,
        #[serde(default)]
        wait_until: NavigationWait,
    },
    Click {
        selector: String,
    },
    Fill {
        selector: String,
        value: String,
    },
    WaitForSelector {
        selector: String,
    },
    TextContent {
        selector: String,
    },
    Evaluate {
        expression: String,
    },
    // Empty struct variants (rather than unit variants) make serde enforce unknown-field
    // rejection for these actions too.
    Title {},
    Content {},
    Screenshot {
        #[serde(default)]
        full_page: bool,
        #[serde(rename = "type", default)]
        kind: ServerImageFormat,
        quality: Option<u8>,
        clip: Option<ServerClip>,
        #[serde(default)]
        omit_background: bool,
    },
    Pdf {
        format: Option<String>,
        width: Option<ServerLength>,
        height: Option<ServerLength>,
        margin: Option<ServerPdfMargin>,
        #[serde(default)]
        landscape: bool,
        #[serde(default)]
        print_background: bool,
        scale: Option<f64>,
        page_ranges: Option<String>,
        #[serde(default)]
        prefer_css_page_size: bool,
        #[serde(default)]
        display_header_footer: bool,
        header_template: Option<String>,
        footer_template: Option<String>,
        #[serde(default)]
        outline: bool,
        #[serde(default)]
        tagged: bool,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

impl ServerImageFormat {
    fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    fn into_core(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Webp => ImageFormat::Webp,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerClip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum ServerLength {
    String(String),
    Number(f64),
}

impl ServerLength {
    fn into_string(&self) -> Result<String, String> {
        match self {
            Self::String(s) => Ok(s.clone()),
            Self::Number(n) if n.is_finite() => Ok(n.to_string()),
            Self::Number(_) => Err("pdf: invalid non-finite length".into()),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerPdfMargin {
    pub top: Option<ServerLength>,
    pub right: Option<ServerLength>,
    pub bottom: Option<ServerLength>,
    pub left: Option<ServerLength>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigationWait {
    #[default]
    Load,
    DomContentLoaded,
    NetworkIdle,
    Commit,
}

impl Step {
    fn screenshot_options(&self) -> Result<ScreenshotOptions, String> {
        let Step::Screenshot {
            full_page,
            kind,
            quality,
            clip,
            omit_background,
        } = self
        else {
            unreachable!();
        };
        Ok(ScreenshotOptions {
            full_page: *full_page,
            format: kind.into_core(),
            quality: *quality,
            clip: clip.map(|c| Clip {
                x: c.x,
                y: c.y,
                width: c.width,
                height: c.height,
            }),
            omit_background: *omit_background,
        })
    }

    fn pdf_options(&self) -> Result<PdfOptions, String> {
        let Step::Pdf {
            format,
            width,
            height,
            margin,
            landscape,
            print_background,
            scale,
            page_ranges,
            prefer_css_page_size,
            display_header_footer,
            header_template,
            footer_template,
            outline,
            tagged,
        } = self
        else {
            unreachable!();
        };
        let len = |v: &Option<ServerLength>| v.as_ref().map(ServerLength::into_string).transpose();
        let m = match margin {
            Some(m) => PdfMargin {
                top: len(&m.top)?,
                right: len(&m.right)?,
                bottom: len(&m.bottom)?,
                left: len(&m.left)?,
            },
            None => PdfMargin::default(),
        };
        Ok(PdfOptions {
            format: format.clone(),
            width: len(width)?,
            height: len(height)?,
            margin: m,
            landscape: *landscape,
            print_background: *print_background,
            scale: *scale,
            page_ranges: page_ranges.clone(),
            prefer_css_page_size: *prefer_css_page_size,
            display_header_footer: *display_header_footer,
            header_template: header_template.clone(),
            footer_template: footer_template.clone(),
            outline: *outline,
            tagged: *tagged,
        })
    }

    pub(crate) async fn execute(&self, page: &PageLease) -> fluxwright::Result<Value> {
        match self {
            Step::Goto { url, wait_until } => {
                let wait = match wait_until {
                    NavigationWait::Load => WaitUntil::Load,
                    NavigationWait::DomContentLoaded => WaitUntil::DomContentLoaded,
                    NavigationWait::NetworkIdle => WaitUntil::NetworkIdle,
                    NavigationWait::Commit => WaitUntil::Commit,
                };
                page.goto_with(url, wait).await?;
                Ok(Value::Null)
            }
            Step::Click { selector } => {
                page.click(selector).await?;
                Ok(Value::Null)
            }
            Step::Fill { selector, value } => {
                page.fill(selector, value).await?;
                Ok(Value::Null)
            }
            Step::WaitForSelector { selector } => {
                page.wait_for_selector(selector).await?;
                Ok(Value::Null)
            }
            Step::TextContent { selector } => {
                Ok(serde_json::to_value(page.text_content(selector).await?)
                    .expect("an optional string is JSON"))
            }
            Step::Evaluate { expression } => page.evaluate(expression).await,
            Step::Title {} => Ok(Value::String(page.title().await?)),
            Step::Content {} => Ok(Value::String(page.content().await?)),
            Step::Screenshot { kind, .. } => {
                let opts = self.screenshot_options().map_err(fluxwright::Error::Other)?;
                let bytes = page.screenshot_with(&opts).await?;
                encoded_blob(kind.mime(), &bytes, MAX_SCREENSHOT_BYTES, "screenshot")
            }
            Step::Pdf { .. } => {
                let opts = self.pdf_options().map_err(fluxwright::Error::Other)?;
                let bytes = page.pdf(&opts).await?;
                encoded_blob("application/pdf", &bytes, MAX_PDF_BYTES, "pdf")
            }
        }
    }
    pub fn action_name(&self) -> &'static str {
        match self {
            Step::Goto { .. } => "goto",
            Step::Click { .. } => "click",
            Step::Fill { .. } => "fill",
            Step::WaitForSelector { .. } => "wait_for_selector",
            Step::TextContent { .. } => "text_content",
            Step::Evaluate { .. } => "evaluate",
            Step::Title {} => "title",
            Step::Content {} => "content",
            Step::Screenshot { .. } => "screenshot",
            Step::Pdf { .. } => "pdf",
        }
    }
}

fn encoded_blob(mime: &str, bytes: &[u8], limit: usize, what: &str) -> fluxwright::Result<Value> {
    if bytes.len() > limit {
        return Err(fluxwright::Error::Other(format!(
            "{what} exceeds the result limit ({} bytes > {limit})",
            bytes.len()
        )));
    }
    Ok(serde_json::json!({
        "mime_type": mime,
        "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Single console message or page error captured during a job.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsoleEntry {
    /// `log`, `error`, `warning`, `info`, `debug`, etc.
    pub kind: String,
    pub text: String,
}

impl ConsoleEntry {
    pub fn new(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: text.into(),
        }
    }
}

/// Recorded network request and optional response status or failure reason.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkEntry {
    pub url: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Diagnostic failure evidence collected for a job.
///
/// Contains captured console logs, network entries, and an optional screenshot (base64 PNG)
/// taken on failure before page context disposal. All collections are bounded to prevent
/// memory exhaustion.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailureEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console: Vec<ConsoleEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<NetworkEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

impl FailureEvidence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.console.is_empty() && self.network.is_empty() && self.screenshot.is_none()
    }

    /// Enforces size bounds to guarantee total evidence stays within MAX_RESULT_BYTES.
    pub fn enforce_bounds(&mut self) {
        if self.console.len() > MAX_CONSOLE_ENTRIES {
            self.console.truncate(MAX_CONSOLE_ENTRIES);
        }
        for entry in &mut self.console {
            truncate_in_place(&mut entry.text, MAX_CONSOLE_TEXT_BYTES);
        }
        if self.network.len() > MAX_NETWORK_ENTRIES {
            self.network.truncate(MAX_NETWORK_ENTRIES);
        }
        for entry in &mut self.network {
            truncate_in_place(&mut entry.url, MAX_NETWORK_URL_BYTES);
            if let Some(fail) = &mut entry.failure {
                truncate_in_place(fail, 512);
            }
        }
        if let Some(ss) = &self.screenshot {
            if ss.len() > MAX_RESULT_BYTES {
                self.screenshot = None;
            }
        }
        let Ok(mut bytes_len) = serde_json::to_vec(self).map(|b| b.len()) else {
            return;
        };
        if bytes_len <= MAX_RESULT_BYTES {
            return;
        }
        if self.screenshot.take().is_some() {
            let Ok(len) = serde_json::to_vec(self).map(|b| b.len()) else {
                return;
            };
            bytes_len = len;
            if bytes_len <= MAX_RESULT_BYTES {
                return;
            }
        }
        for _ in 0..2 {
            let keep = MAX_RESULT_BYTES.saturating_sub(4096);
            self.console.truncate(self.console.len() * keep / bytes_len);
            self.network.truncate(self.network.len() * keep / bytes_len);
            let Ok(len) = serde_json::to_vec(self).map(|b| b.len()) else {
                return;
            };
            if len <= MAX_RESULT_BYTES {
                return;
            }
            bytes_len = len;
        }
        self.console.clear();
        self.network.clear();
    }
}

pub(crate) fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    s[..s.floor_char_boundary(max_bytes)].to_string()
}

fn truncate_in_place(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        s.truncate(s.floor_char_boundary(max_bytes));
    }
}

fn is_none_or_empty(evidence: &Option<FailureEvidence>) -> bool {
    evidence.as_ref().is_none_or(|e| e.is_empty())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub status: JobStatus,
    /// Unix epoch milliseconds. The first start is preserved across retries.
    pub submitted_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub attempts: u32,
    /// Zero-based index of the step currently executing (or the last one attempted).
    pub step_index: Option<usize>,
    /// One result per step on success. Actions without a return value produce null.
    pub results: Option<Vec<Value>>,
    pub error: Option<String>,
    /// W3C trace ID (32 hex characters) for distributed tracing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Diagnostic failure evidence: console logs, network entries, failure screenshot.
    #[serde(default, skip_serializing_if = "is_none_or_empty")]
    pub failure_evidence: Option<FailureEvidence>,
}

impl Job {
    pub fn evidence(&self) -> Option<&FailureEvidence> {
        self.failure_evidence.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_requests_and_defaults() {
        let job: JobRequest = serde_json::from_value(json!({
            "steps": [{ "action": "goto", "url": "https://example.com", "wait_until": "domcontentloaded" },
                      { "action": "title" }]
        })).unwrap();
        assert!(job.validate().is_ok());
        assert_eq!(job.timeout_ms, 60_000);
        assert_eq!(job.retries, 0);
        assert_eq!(job.options().priority, Priority::Normal);
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            " https://example.com",
            "https://exa\nmple.com",
        ] {
            assert!(!web_url(url), "{url}");
        }
        assert!(web_url("HTTP://localhost:8080/"));
        assert!(web_url("about:blank"));
    }

    #[test]
    fn rejects_unbounded_and_unknown_options() {
        for value in [
            json!({ "steps": [] }),
            json!({ "steps": vec![json!({ "action": "title" }); 65] }),
            json!({ "steps": [{ "action": "click", "selector": " " }] }),
            json!({ "steps": [{ "action": "evaluate", "expression": "" }] }),
            json!({ "steps": [{ "action": "title" }], "timeout_ms": 0 }),
            json!({ "steps": [{ "action": "title" }], "timeout_ms": 300001 }),
            json!({ "steps": [{ "action": "title" }], "retries": 4 }),
            json!({ "steps": [{ "action": "screenshot", "quality": 80 }] }),
            json!({ "steps": [{ "action": "screenshot", "clip": { "x": 0, "y": 0, "width": 0, "height": 10 } }] }),
            json!({ "steps": [{ "action": "pdf", "format": "A9" }] }),
            json!({ "steps": [{ "action": "pdf", "scale": 3.0 }] }),
            json!({ "steps": [{ "action": "pdf", "margin": { "top": "10pt" } }] }),
        ] {
            let job: JobRequest = serde_json::from_value(value).unwrap();
            assert!(job.validate().is_err());
        }
        assert!(serde_json::from_value::<JobRequest>(json!({
            "steps": [{ "action": "title" }], "script": "host code"
        }))
        .is_err());
        for action in ["screenshot", "pdf", "title", "content"] {
            assert!(serde_json::from_value::<Step>(json!({
                "action": action, "path": "secret.png"
            }))
            .is_err());
        }
        assert!(serde_json::from_value::<Step>(json!({ "action": "pdf", "format": "A4", "width": 800, "margin": { "top": "10mm", "left": 24 } })).is_ok());
        assert!(serde_json::from_value::<Step>(json!({ "action": "pdf", "margin": { "bogus": 1 } })).is_err());
    }
}
