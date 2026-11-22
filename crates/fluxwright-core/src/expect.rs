use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::pool::{with_timeout, Locator, PageLease};
use crate::visual::{
    artifact_paths, compare_images, decode_png, should_update_snapshots, Animations, RgbaImage,
    ToHaveScreenshotOptions, DISABLE_ANIMATIONS_LOCATOR_JS, DISABLE_ANIMATIONS_PAGE_JS,
    RESTORE_ANIMATIONS_LOCATOR_JS, RESTORE_ANIMATIONS_PAGE_JS,
};

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

    /// Asserts that the element screenshot matches the golden PNG at `path` (auto-retrying).
    pub async fn to_have_screenshot(self, path: impl AsRef<Path>) -> Result<()> {
        self.to_have_screenshot_with(path, &ToHaveScreenshotOptions::default())
            .await
    }

    /// Asserts that the element screenshot matches the golden PNG at `path` with options.
    pub async fn to_have_screenshot_with(
        self,
        path: impl AsRef<Path>,
        options: &ToHaveScreenshotOptions,
    ) -> Result<()> {
        let target = self.target_label();
        run_to_have_screenshot(
            &target,
            path.as_ref(),
            options,
            self.negated,
            self.timeout,
            self.poll_interval,
            self.locator.lease(),
            Some(self.locator),
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

    /// Asserts that the page screenshot matches the golden PNG at `path` (auto-retrying).
    pub async fn to_have_screenshot(self, path: impl AsRef<Path>) -> Result<()> {
        self.to_have_screenshot_with(path, &ToHaveScreenshotOptions::default())
            .await
    }

    /// Asserts that the page screenshot matches the golden PNG at `path` with options.
    pub async fn to_have_screenshot_with(
        self,
        path: impl AsRef<Path>,
        options: &ToHaveScreenshotOptions,
    ) -> Result<()> {
        run_to_have_screenshot(
            "expect(page)",
            path.as_ref(),
            options,
            self.negated,
            self.timeout,
            self.poll_interval,
            self.page,
            None,
        )
        .await
    }
}

async fn apply_animation_guard(
    page: &PageLease,
    locator: Option<&Locator<'_>>,
    disable: bool,
) {
    if !disable || page.is_closed() {
        return;
    }
    let _ = with_timeout(
        Some(Duration::from_millis(500)),
        page.evaluate(DISABLE_ANIMATIONS_PAGE_JS),
    )
    .await;
    if let Some(loc) = locator.filter(|l| !l.selector().frames.is_empty()) {
        let _ = with_timeout(
            Some(Duration::from_millis(500)),
            loc.evaluate(DISABLE_ANIMATIONS_LOCATOR_JS, None),
        )
        .await;
    }
}

async fn clear_animation_guard(
    page: &PageLease,
    locator: Option<&Locator<'_>>,
    disable: bool,
) {
    if !disable || page.is_closed() {
        return;
    }
    let _ = with_timeout(
        Some(Duration::from_millis(500)),
        page.evaluate(RESTORE_ANIMATIONS_PAGE_JS),
    )
    .await;
    if let Some(loc) = locator.filter(|l| !l.selector().frames.is_empty()) {
        let _ = with_timeout(
            Some(Duration::from_millis(500)),
            loc.evaluate(RESTORE_ANIMATIONS_LOCATOR_JS, None),
        )
        .await;
    }
}

async fn capture_png_shot(
    page: &PageLease,
    locator: Option<&Locator<'_>>,
    options: &ToHaveScreenshotOptions,
    limit: Duration,
) -> Result<Vec<u8>> {
    if page.is_closed() {
        return Err(Error::Other("page is closed".into()));
    }
    if let Some(loc) = locator {
        let shot_opts = fluxwright_cdp::ScreenshotOptions {
            omit_background: options.omit_background,
            ..Default::default()
        };
        with_timeout(Some(limit), loc.screenshot_with(&shot_opts)).await
    } else {
        let shot_opts = fluxwright_cdp::ScreenshotOptions {
            full_page: options.full_page,
            clip: options.clip,
            omit_background: options.omit_background,
            ..Default::default()
        };
        with_timeout(Some(limit), page.screenshot_with(&shot_opts)).await
    }
}

async fn write_file_mkdir(path: PathBuf, bytes: Vec<u8>) -> Result<()> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &bytes)
    })
    .await
    .map_err(|e| Error::Other(e.to_string()))?
    .map_err(|e| Error::Other(format!("write snapshot artifact: {e}")))
}

#[allow(clippy::too_many_arguments)]
async fn run_to_have_screenshot(
    target: &str,
    path: &Path,
    options: &ToHaveScreenshotOptions,
    negated: bool,
    timeout: Duration,
    poll_interval: Duration,
    page: &PageLease,
    locator: Option<&Locator<'_>>,
) -> Result<()> {
    if negated {
        return Err(Error::Other(format!(
            "{target}.to_have_screenshot: .not() is not supported"
        )));
    }
    options.validate()?;
    if locator.is_some() && (options.full_page || options.clip.is_some()) {
        return Err(Error::Other(
            "expect(locator).to_have_screenshot: full_page and clip are for page screenshots only"
                .into(),
        ));
    }

    let disable_animations = options.animations == Animations::Disabled;
    let res = run_to_have_screenshot_inner(
        target,
        path,
        options,
        timeout,
        poll_interval,
        page,
        locator,
        disable_animations,
    )
    .await;
    clear_animation_guard(page, locator, disable_animations).await;
    res
}

#[allow(clippy::too_many_arguments)]
async fn run_to_have_screenshot_inner(
    target: &str,
    path: &Path,
    options: &ToHaveScreenshotOptions,
    timeout: Duration,
    poll_interval: Duration,
    page: &PageLease,
    locator: Option<&Locator<'_>>,
    disable_animations: bool,
) -> Result<()> {
    let (baseline_path, actual_path, diff_path) = artifact_paths(path);
    let shot_limit = |remaining: Duration| -> Duration {
        remaining.clamp(Duration::from_millis(100), Duration::from_secs(2))
    };

    if should_update_snapshots(options.update) {
        let start = Instant::now();
        let mut prev_img: Option<RgbaImage> = None;
        let mut last_err: Option<String>;
        let mut attempts = 0usize;
        loop {
            apply_animation_guard(page, locator, disable_animations).await;
            let remaining = timeout.saturating_sub(start.elapsed());
            match capture_png_shot(page, locator, options, shot_limit(remaining)).await {
                Ok(bytes) => {
                    attempts += 1;
                    let img = decode_png(&bytes)?;
                    if let Some(ref prev) = prev_img {
                        let diff = compare_images(prev, &img, options.threshold, false)?;
                        if diff.is_within_tolerance(options) {
                            write_file_mkdir(baseline_path, bytes).await?;
                            return Ok(());
                        }
                    } else if timeout.is_zero() {
                        write_file_mkdir(baseline_path, bytes).await?;
                        return Ok(());
                    }
                    prev_img = Some(img);
                    last_err = None;
                }
                Err(e) if is_fatal_error(&e) => return Err(e),
                Err(e) => last_err = Some(e.to_string()),
            }

            let elapsed = start.elapsed();
            if elapsed >= timeout {
                if attempts >= 2 {
                    return Err(Error::Cdp(fluxwright_cdp::Error::timeout(
                        format!(
                            "{target}.to_have_screenshot: screenshot did not stabilize for snapshot update at {}",
                            baseline_path.display()
                        ),
                        timeout.as_millis() as u64,
                    )));
                }
                if attempts == 1 {
                    // Single shot exhausted a tight timeout; capture one confirmation shot.
                    let confirm_limit = Duration::from_secs(1);
                    if let Ok(bytes) = capture_png_shot(page, locator, options, confirm_limit).await {
                        let img = decode_png(&bytes)?;
                        if let Some(ref prev) = prev_img {
                            let diff = compare_images(prev, &img, options.threshold, false)?;
                            if diff.is_within_tolerance(options) {
                                write_file_mkdir(baseline_path, bytes).await?;
                                return Ok(());
                            }
                        }
                    }
                }
                return Err(Error::Cdp(fluxwright_cdp::Error::timeout(
                    format!(
                        "{target}.to_have_screenshot: failed to capture snapshot ({})",
                        last_err.unwrap_or_else(|| "did not stabilize".into())
                    ),
                    timeout.as_millis() as u64,
                )));
            }
            let wait = poll_interval.min(timeout - elapsed);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
    }

    let baseline_read = {
        let p = baseline_path.clone();
        tokio::task::spawn_blocking(move || std::fs::read(&p))
            .await
            .map_err(|e| Error::Other(e.to_string()))?
    };
    let baseline_bytes = match baseline_read {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            apply_animation_guard(page, locator, disable_animations).await;
            if let Ok(actual) = capture_png_shot(page, locator, options, Duration::from_secs(2)).await {
                let _ = write_file_mkdir(actual_path.clone(), actual).await;
            }
            return Err(Error::Other(format!(
                "{target}.to_have_screenshot: baseline snapshot does not exist at {} (run with FLUXWRIGHT_UPDATE_SNAPSHOTS=1 or update(true) to create it; actual={})",
                baseline_path.display(),
                actual_path.display()
            )));
        }
        Err(e) => {
            return Err(Error::Other(format!(
                "{target}.to_have_screenshot: failed to read baseline {}: {e}",
                baseline_path.display()
            )))
        }
    };

    let baseline_img = decode_png(&baseline_bytes)?;
    let start = Instant::now();
    let mut last_mismatch: Option<(Vec<u8>, RgbaImage)> = None;
    let mut last_err: Option<String>;

    loop {
        apply_animation_guard(page, locator, disable_animations).await;
        let remaining = timeout.saturating_sub(start.elapsed());
        match capture_png_shot(page, locator, options, shot_limit(remaining)).await {
            Ok(bytes) => {
                let actual_img = decode_png(&bytes)?;
                let diff = compare_images(&baseline_img, &actual_img, options.threshold, false)?;
                if diff.is_within_tolerance(options) {
                    return Ok(());
                }
                last_mismatch = Some((bytes, actual_img));
                last_err = None;
            }
            Err(e) if is_fatal_error(&e) => return Err(e),
            Err(e) => last_err = Some(e.to_string()),
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            break;
        }
        let wait = poll_interval.min(timeout - elapsed);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    if let Some((actual_bytes, actual_img)) = last_mismatch {
        let final_diff = compare_images(&baseline_img, &actual_img, options.threshold, true)?;
        write_file_mkdir(actual_path.clone(), actual_bytes).await?;
        if let Some(diff_png) = final_diff.diff_png {
            write_file_mkdir(diff_path.clone(), diff_png).await?;
        }
        return Err(Error::Cdp(fluxwright_cdp::Error::timeout(
            format!(
                "{target}.to_have_screenshot: screenshot mismatch ({} of {} pixels differ, ratio {:.6}; expected {}x{}, actual {}x{}; baseline={}, actual={}, diff={})",
                final_diff.diff_pixels,
                final_diff.total_pixels,
                final_diff.diff_ratio,
                final_diff.expected_size.0,
                final_diff.expected_size.1,
                final_diff.actual_size.0,
                final_diff.actual_size.1,
                baseline_path.display(),
                actual_path.display(),
                diff_path.display(),
            ),
            timeout.as_millis() as u64,
        )));
    }

    Err(Error::Cdp(fluxwright_cdp::Error::timeout(
        format!(
            "{target}.to_have_screenshot: {}",
            last_err.unwrap_or_else(|| "screenshot comparison timed out".into())
        ),
        timeout.as_millis() as u64,
    )))
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
