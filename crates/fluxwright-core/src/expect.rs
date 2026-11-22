use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::pool::{with_timeout, Locator, PageLease};

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MIN_PROBE_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Creates an auto-retrying assertion builder for a [`Locator`].
pub fn expect_locator<'a>(locator: &'a Locator<'a>) -> LocatorExpect<'a> {
    LocatorExpect {
        locator,
        timeout: DEFAULT_TIMEOUT,
        poll_interval: DEFAULT_POLL_INTERVAL,
        negated: false,
    }
}

/// Creates an auto-retrying assertion builder for a [`PageLease`].
pub fn expect_page<'a>(page: &'a PageLease) -> PageExpect<'a> {
    PageExpect {
        page,
        timeout: DEFAULT_TIMEOUT,
        poll_interval: DEFAULT_POLL_INTERVAL,
        negated: false,
    }
}

/// Auto-retrying Playwright-style assertions for a [`Locator`].
#[derive(Clone, Copy)]
pub struct LocatorExpect<'a> {
    locator: &'a Locator<'a>,
    timeout: Duration,
    poll_interval: Duration,
    negated: bool,
}

/// Auto-retrying Playwright-style assertions for a [`PageLease`].
#[derive(Clone, Copy)]
pub struct PageExpect<'a> {
    page: &'a PageLease,
    timeout: Duration,
    poll_interval: Duration,
    negated: bool,
}

#[derive(Clone, Copy)]
enum BoolCheck {
    Visible,
    Hidden,
    Enabled,
    Disabled,
    Checked,
    Editable,
}

enum Probe {
    Value { matched: bool, actual: String },
    NotAttached,
    Transient(String),
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_url(base: Option<&str>, url: &str) -> String {
    let Some(base) = base.filter(|_| !url.split(['/', '?', '#']).next().unwrap_or("").contains(':')) else {
        return url.to_string();
    };
    if url.starts_with("//") {
        if let Some((scheme, _)) = base.split_once("://") {
            return format!("{scheme}:{url}");
        }
    }
    if url.starts_with('/') {
        if let Some((scheme, rest)) = base.split_once("://") {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            return format!("{scheme}://{authority}{url}");
        }
    }
    let dir = match base.find("://").map(|i| i + 3) {
        Some(after_scheme) => match base[after_scheme..].rfind('/') {
            Some(slash) => &base[..=after_scheme + slash],
            None => return format!("{base}/{}", url.trim_start_matches("./")),
        },
        None => base,
    };
    format!("{dir}{}", url.trim_start_matches("./"))
}

fn is_fatal_error(err: &Error) -> bool {
    if err.is_retryable() {
        return true;
    }
    match err {
        Error::ShuttingDown => true,
        Error::Other(msg) => msg.contains("page is closed") || msg.contains("page closed"),
        Error::Cdp(fluxwright_cdp::Error::NotActionable { reason, .. }) => {
            reason.starts_with("invalid selector:")
        }
        _ => false,
    }
}

fn probe_timeout(remaining: Duration) -> Duration {
    remaining.clamp(MIN_PROBE_TIMEOUT, MAX_PROBE_TIMEOUT)
}

fn format_expect_error(
    target: &str,
    matcher: &str,
    expected: &str,
    actual: &str,
    negated: bool,
    timeout: Duration,
) -> Error {
    let expected_desc = if negated {
        format!("not {expected}")
    } else {
        expected.to_string()
    };
    Error::Cdp(fluxwright_cdp::Error::timeout(
        format!("{target}.{matcher}: expected {expected_desc}, actual {actual}"),
        timeout.as_millis() as u64,
    ))
}

async fn ensure_attached(locator: &Locator<'_>, limit: Duration) -> Result<Option<Probe>> {
    match with_timeout(Some(limit), locator.count()).await {
        Ok(0) => Ok(Some(Probe::NotAttached)),
        Ok(_) => Ok(None),
        Err(e) if is_fatal_error(&e) => Err(e),
        Err(e) => Ok(Some(Probe::Transient(e.to_string()))),
    }
}

async fn poll_until<F, Fut>(
    target: &str,
    matcher: &str,
    expected: &str,
    negated: bool,
    timeout: Duration,
    poll_interval: Duration,
    mut probe: F,
) -> Result<()>
where
    F: FnMut(Duration) -> Fut,
    Fut: std::future::Future<Output = Result<Probe>>,
{
    let start = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        let actual = match probe(probe_timeout(remaining)).await? {
            Probe::Value { matched, actual } => {
                let ok = if negated { !matched } else { matched };
                if ok {
                    return Ok(());
                }
                actual
            }
            Probe::NotAttached => "element not found (count: 0)".to_string(),
            Probe::Transient(msg) => format!("error: {msg}"),
        };

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(format_expect_error(
                target, matcher, expected, &actual, negated, timeout,
            ));
        }

        let wait = poll_interval.min(timeout - elapsed);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

impl<'a> LocatorExpect<'a> {
    /// Sets the maximum time to wait for the assertion to pass (default: 5000ms).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the interval between assertion polls (default: 50ms).
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Negates the assertion.
    pub fn not(mut self) -> Self {
        self.negated = !self.negated;
        self
    }

    fn target_label(&self) -> String {
        format!("expect(locator({}))", self.locator.selector())
    }

    async fn run_bool_check(
        self,
        check: BoolCheck,
        matcher: &str,
        expected: &str,
        true_label: &str,
        false_label: &str,
        require_attached: bool,
    ) -> Result<()> {
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            matcher,
            expected,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| async move {
                if require_attached {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                }
                let res = with_timeout(Some(limit), async {
                    match check {
                        BoolCheck::Visible => locator.is_visible().await,
                        BoolCheck::Hidden => locator.is_hidden().await,
                        BoolCheck::Enabled => locator.is_enabled().await,
                        BoolCheck::Disabled => locator.is_disabled().await,
                        BoolCheck::Checked => locator.is_checked().await,
                        BoolCheck::Editable => locator.is_editable().await,
                    }
                })
                .await;
                match res {
                    Ok(val) => Ok(Probe::Value {
                        matched: val,
                        actual: if val { true_label } else { false_label }.to_string(),
                    }),
                    Err(e) if is_fatal_error(&e) => Err(e),
                    Err(e) => Ok(Probe::Transient(e.to_string())),
                }
            },
        )
        .await
    }

    /// Asserts that the locator points to a visible element.
    pub async fn to_be_visible(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Visible,
            "to_be_visible",
            "visible",
            "visible",
            "hidden",
            false,
        )
        .await
    }

    /// Asserts that the locator points to a hidden or detached element.
    pub async fn to_be_hidden(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Hidden,
            "to_be_hidden",
            "hidden",
            "hidden",
            "visible",
            false,
        )
        .await
    }

    /// Asserts that the locator points to an element attached to the DOM.
    pub async fn to_be_attached(self) -> Result<()> {
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_be_attached",
            "attached",
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| async move {
                match with_timeout(Some(limit), locator.count()).await {
                    Ok(0) => Ok(Probe::Value {
                        matched: false,
                        actual: "detached (count: 0)".to_string(),
                    }),
                    Ok(1) => Ok(Probe::Value {
                        matched: true,
                        actual: "attached".to_string(),
                    }),
                    Ok(n) => match with_timeout(Some(limit), locator.is_visible()).await {
                        Ok(_) => Ok(Probe::Value {
                            matched: true,
                            actual: format!("attached (count: {n})"),
                        }),
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    },
                    Err(e) if is_fatal_error(&e) => Err(e),
                    Err(e) => Ok(Probe::Transient(e.to_string())),
                }
            },
        )
        .await
    }

    /// Asserts that the locator points to an enabled element.
    pub async fn to_be_enabled(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Enabled,
            "to_be_enabled",
            "enabled",
            "enabled",
            "disabled",
            true,
        )
        .await
    }

    /// Asserts that the locator points to a disabled element.
    pub async fn to_be_disabled(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Disabled,
            "to_be_disabled",
            "disabled",
            "disabled",
            "enabled",
            true,
        )
        .await
    }

    /// Asserts that the checkbox, radio, or switch element is checked.
    pub async fn to_be_checked(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Checked,
            "to_be_checked",
            "checked",
            "checked",
            "unchecked",
            true,
        )
        .await
    }

    /// Asserts that the element is editable.
    pub async fn to_be_editable(self) -> Result<()> {
        self.run_bool_check(
            BoolCheck::Editable,
            "to_be_editable",
            "editable",
            "editable",
            "not editable",
            true,
        )
        .await
    }

    /// Asserts that the element's normalized `textContent` equals `expected`.
    pub async fn to_have_text(self, expected: impl AsRef<str>) -> Result<()> {
        let expected_norm = normalize_whitespace(expected.as_ref());
        let expected_desc = format!("{expected_norm:?}");
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_have_text",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let expected_norm = expected_norm.clone();
                async move {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                    match with_timeout(Some(limit), locator.text_content()).await {
                        Ok(text) => {
                            let actual_norm = normalize_whitespace(text.as_deref().unwrap_or(""));
                            Ok(Probe::Value {
                                matched: actual_norm == expected_norm,
                                actual: format!("{actual_norm:?}"),
                            })
                        }
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the element's normalized `textContent` contains `expected`.
    pub async fn to_contain_text(self, expected: impl AsRef<str>) -> Result<()> {
        let expected_norm = normalize_whitespace(expected.as_ref());
        let expected_desc = format!("substring {expected_norm:?}");
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_contain_text",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let expected_norm = expected_norm.clone();
                async move {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                    match with_timeout(Some(limit), locator.text_content()).await {
                        Ok(text) => {
                            let actual_norm = normalize_whitespace(text.as_deref().unwrap_or(""));
                            Ok(Probe::Value {
                                matched: actual_norm.contains(&expected_norm),
                                actual: format!("{actual_norm:?}"),
                            })
                        }
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the form control's value equals `expected`.
    pub async fn to_have_value(self, expected: impl AsRef<str>) -> Result<()> {
        let expected = expected.as_ref().to_string();
        let expected_desc = format!("{expected:?}");
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_have_value",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let expected = expected.clone();
                async move {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                    match with_timeout(Some(limit), locator.input_value()).await {
                        Ok(val) => Ok(Probe::Value {
                            matched: val == expected,
                            actual: format!("{val:?}"),
                        }),
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the element has attribute `name` equal to `expected`.
    pub async fn to_have_attribute(self, name: impl AsRef<str>, expected: impl AsRef<str>) -> Result<()> {
        let name = name.as_ref().to_string();
        let expected = expected.as_ref().to_string();
        let expected_desc = format!("{name}={expected:?}");
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_have_attribute",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let name = name.clone();
                let expected = expected.clone();
                async move {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                    match with_timeout(Some(limit), locator.get_attribute(&name)).await {
                        Ok(Some(val)) => Ok(Probe::Value {
                            matched: val == expected,
                            actual: format!("{name}={val:?}"),
                        }),
                        Ok(None) => Ok(Probe::Value {
                            matched: false,
                            actual: format!("{name}=<absent>"),
                        }),
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the element's `class` attribute equals `expected`.
    pub async fn to_have_class(self, expected: impl AsRef<str>) -> Result<()> {
        let expected = expected.as_ref().to_string();
        let expected_desc = format!("{expected:?}");
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_have_class",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let expected = expected.clone();
                async move {
                    if let Some(early) = ensure_attached(locator, limit).await? {
                        return Ok(early);
                    }
                    match with_timeout(Some(limit), locator.get_attribute("class")).await {
                        Ok(cls) => {
                            let actual = cls.unwrap_or_default();
                            Ok(Probe::Value {
                                matched: actual == expected,
                                actual: format!("{actual:?}"),
                            })
                        }
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the locator resolves to exactly `expected` elements.
    pub async fn to_have_count(self, expected: usize) -> Result<()> {
        let expected_desc = expected.to_string();
        let target = self.target_label();
        let locator = self.locator;
        poll_until(
            &target,
            "to_have_count",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| async move {
                match with_timeout(Some(limit), locator.count()).await {
                    Ok(actual) => Ok(Probe::Value {
                        matched: actual == expected,
                        actual: actual.to_string(),
                    }),
                    Err(e) if is_fatal_error(&e) => Err(e),
                    Err(e) => Ok(Probe::Transient(e.to_string())),
                }
            },
        )
        .await
    }
}

impl<'a> PageExpect<'a> {
    /// Sets the maximum time to wait for the assertion to pass (default: 5000ms).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the interval between assertion polls (default: 50ms).
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Negates the assertion.
    pub fn not(mut self) -> Self {
        self.negated = !self.negated;
        self
    }

    /// Asserts that the page's normalized title equals `expected`.
    pub async fn to_have_title(self, expected: impl AsRef<str>) -> Result<()> {
        let expected_norm = normalize_whitespace(expected.as_ref());
        let expected_desc = format!("{expected_norm:?}");
        let page = self.page;
        poll_until(
            "expect(page)",
            "to_have_title",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |limit| {
                let expected_norm = expected_norm.clone();
                async move {
                    if page.is_closed() {
                        return Err(Error::Other("page is closed".into()));
                    }
                    match with_timeout(Some(limit), page.title()).await {
                        Ok(title) => {
                            let actual_norm = normalize_whitespace(&title);
                            Ok(Probe::Value {
                                matched: actual_norm == expected_norm,
                                actual: format!("{actual_norm:?}"),
                            })
                        }
                        Err(e) if is_fatal_error(&e) => Err(e),
                        Err(e) => Ok(Probe::Transient(e.to_string())),
                    }
                }
            },
        )
        .await
    }

    /// Asserts that the page's URL equals `expected` (resolving relative paths against `base_url`).
    pub async fn to_have_url(self, expected: impl AsRef<str>) -> Result<()> {
        let base = self.page.base_url();
        let resolved = resolve_url(base.as_deref(), expected.as_ref());
        let expected_desc = format!("{resolved:?}");
        let page = self.page;
        poll_until(
            "expect(page)",
            "to_have_url",
            &expected_desc,
            self.negated,
            self.timeout,
            self.poll_interval,
            |_| {
                let resolved = resolved.clone();
                async move {
                    if page.is_closed() {
                        return Err(Error::Other("page is closed".into()));
                    }
                    let actual = page.url();
                    Ok(Probe::Value {
                        matched: actual == resolved,
                        actual: format!("{actual:?}"),
                    })
                }
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_whitespace_collapses_and_trims() {
        assert_eq!(normalize_whitespace("  hello   world \n\t ! "), "hello world !");
        assert_eq!(normalize_whitespace("   "), "");
        assert_eq!(normalize_whitespace("plain"), "plain");
    }

    #[test]
    fn resolve_url_joins_relative_paths() {
        assert_eq!(
            resolve_url(Some("http://127.0.0.1:8080/sub/page"), "/done"),
            "http://127.0.0.1:8080/done"
        );
        assert_eq!(
            resolve_url(Some("http://127.0.0.1:8080/sub/page"), "next"),
            "http://127.0.0.1:8080/sub/next"
        );
        assert_eq!(
            resolve_url(Some("http://127.0.0.1:8080"), "https://other.test/x"),
            "https://other.test/x"
        );
        assert_eq!(resolve_url(None, "/done"), "/done");
    }

    #[test]
    fn format_expect_error_includes_expected_actual_and_timeout() {
        let err = format_expect_error(
            "expect(locator(#btn))",
            "to_have_text",
            "\"Save\"",
            "\"Cancel\"",
            false,
            Duration::from_millis(250),
        );
        assert!(err.is_timeout());
        let msg = err.to_string();
        assert!(msg.contains("expect(locator(#btn)).to_have_text"), "{msg}");
        assert!(msg.contains("expected \"Save\""), "{msg}");
        assert!(msg.contains("actual \"Cancel\""), "{msg}");
        assert!(msg.contains("250ms"), "{msg}");

        let neg_err = format_expect_error(
            "expect(locator(#btn))",
            "to_be_visible",
            "visible",
            "visible",
            true,
            Duration::from_millis(100),
        );
        assert!(neg_err.to_string().contains("expected not visible"), "{neg_err}");
    }
}
