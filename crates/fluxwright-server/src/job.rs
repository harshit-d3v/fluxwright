use std::time::Duration;

use base64::Engine;
use fluxwright::{
    netguard, parse_modifiers, CheckOptions, ClickOptions, Clip, HoverOptions, ImageFormat,
    JobOptions, MarkdownOptions, MouseButton, PageLease, PdfMargin, PdfOptions, Priority,
    ResourcePreset, ResourceType, ScreenshotOptions, SelectEntry, SnapshotOptions, WaitUntil,
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
#[derive(Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub resource_preset: Option<ServerResourcePreset>,
    #[serde(default)]
    pub block_resources: Vec<ServerResourceType>,
    #[serde(default)]
    pub block_url_patterns: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerResourcePreset {
    Scraping,
    Minimal,
    #[serde(alias = "no-media")]
    NoMedia,
}

impl ServerResourcePreset {
    fn into_core(self) -> ResourcePreset {
        match self {
            Self::Scraping => ResourcePreset::Scraping,
            Self::Minimal => ResourcePreset::Minimal,
            Self::NoMedia => ResourcePreset::NoMedia,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerResourceType {
    Image,
    Font,
    Media,
    Stylesheet,
    Script,
}

impl ServerResourceType {
    fn into_core(self) -> ResourceType {
        match self {
            Self::Image => ResourceType::Image,
            Self::Font => ResourceType::Font,
            Self::Media => ResourceType::Media,
            Self::Stylesheet => ResourceType::Stylesheet,
            Self::Script => ResourceType::Script,
        }
    }
}

fn default_timeout() -> u64 {
    60_000
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
        if self.block_url_patterns.len() > 64 {
            return Err("block_url_patterns must contain at most 64 patterns".into());
        }
        for pat in &self.block_url_patterns {
            if pat.trim().is_empty()
                || pat.len() > 512
                || pat.chars().any(char::is_control)
            {
                return Err(
                    "block_url_patterns entries must be 1-512 non-control characters".into(),
                );
            }
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
                Step::Click {
                    selector,
                    delay_ms,
                    position,
                    modifiers,
                    ..
                }
                | Step::Dblclick {
                    selector,
                    delay_ms,
                    position,
                    modifiers,
                    ..
                } => {
                    validate_selector(selector)?;
                    validate_delay(*delay_ms)?;
                    validate_point(*position)?;
                    validate_modifiers(modifiers)?;
                }
                Step::Hover {
                    selector,
                    position,
                    modifiers,
                    ..
                }
                | Step::Tap {
                    selector,
                    position,
                    modifiers,
                    ..
                } => {
                    validate_selector(selector)?;
                    validate_point(*position)?;
                    validate_modifiers(modifiers)?;
                }
                Step::Check {
                    selector,
                    position,
                    ..
                }
                | Step::Uncheck {
                    selector,
                    position,
                    ..
                } => {
                    validate_selector(selector)?;
                    validate_point(*position)?;
                }
                Step::Press {
                    selector,
                    key,
                    delay_ms,
                } => {
                    validate_selector(selector)?;
                    if key.is_empty() {
                        return Err("press key must not be empty".into());
                    }
                    validate_delay(*delay_ms)?;
                }
                Step::Type {
                    selector, delay_ms, ..
                } => {
                    validate_selector(selector)?;
                    validate_delay(*delay_ms)?;
                }
                Step::SelectOption {
                    selector, values, ..
                } => {
                    validate_selector(selector)?;
                    values.validate()?;
                }
                Step::GetAttribute { selector, name, .. } => {
                    validate_selector(selector)?;
                    if name.trim().is_empty() {
                        return Err("attribute name must not be empty".into());
                    }
                }
                Step::Fill { selector, .. }
                | Step::Focus { selector }
                | Step::Blur { selector }
                | Step::Clear { selector, .. }
                | Step::WaitForSelector { selector }
                | Step::TextContent { selector, .. }
                | Step::InnerText { selector, .. }
                | Step::InnerHtml { selector, .. }
                | Step::InputValue { selector, .. }
                | Step::Count { selector }
                | Step::AllTextContents { selector }
                | Step::AllInnerTexts { selector }
                | Step::IsVisible { selector, .. }
                | Step::IsHidden { selector, .. }
                | Step::IsEnabled { selector, .. }
                | Step::IsDisabled { selector, .. }
                | Step::IsChecked { selector, .. }
                | Step::IsEditable { selector, .. } => {
                    validate_selector(selector)?;
                }
                Step::Evaluate { expression } if expression.trim().is_empty() => {
                    return Err("evaluate expression must not be empty".into());
                }
                Step::WaitForUrl { url, .. } if url.trim().is_empty() => {
                    return Err("wait_for_url url must not be empty".into());
                }
                Step::WaitForFunction { expression, polling_ms } => {
                    if expression.trim().is_empty() {
                        return Err("wait_for_function expression must not be empty".into());
                    }
                    if polling_ms.is_some_and(|ms| ms == 0 || ms > 60_000) {
                        return Err("polling_ms must be between 1 and 60000".into());
                    }
                }
                Step::AddInitScript { script } if script.trim().is_empty() => {
                    return Err("add_init_script script must not be empty".into());
                }
                Step::AddScriptTag { url, content, .. } => {
                    validate_tag_source("add_script_tag", url.as_deref(), content.as_deref())?;
                }
                Step::AddStyleTag { url, content } => {
                    validate_tag_source("add_style_tag", url.as_deref(), content.as_deref())?;
                }
                Step::Title {} => {}
                Step::Content {} => {}
                Step::Snapshot { max_depth, max_nodes, .. } => {
                    if max_depth.is_some_and(|d| d == 0 || d > 64) {
                        return Err("max_depth must be between 1 and 64".into());
                    }
                    if max_nodes.is_some_and(|n| n == 0 || n > 5000) {
                        return Err("max_nodes must be between 1 and 5000".into());
                    }
                }
                Step::Markdown { selector, .. } => {
                    if let Some(sel) = selector {
                        validate_selector(sel)?;
                    }
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
        let mut opts = JobOptions::default()
            .priority(priority)
            .retries(self.retries)
            .timeout(Duration::from_millis(self.timeout_ms));
        if let Some(preset) = self.resource_preset {
            opts = opts.resource_preset(preset.into_core());
        }
        if !self.block_resources.is_empty() {
            opts = opts.block_resources(self.block_resources.iter().copied().map(ServerResourceType::into_core));
        }
        if !self.block_url_patterns.is_empty() {
            opts = opts.block_url_patterns(self.block_url_patterns.iter().cloned());
        }
        opts
    }
}

fn validate_tag_source(what: &str, url: Option<&str>, content: Option<&str>) -> Result<(), String> {
    if url.is_some() == content.is_some() {
        return Err(format!("{what} requires exactly one of url or content"));
    }
    if let Some(u) = url {
        let trimmed = u.trim();
        if trimmed.is_empty() {
            return Err(format!("{what} url must not be empty"));
        }
        if trimmed.starts_with("//") {
            return Err(format!("{what} url is not allowed"));
        }
        if trimmed.contains(':') && (!web_url(trimmed) || netguard::url_allowed(trimmed).is_err()) {
            return Err(format!("{what} url is not allowed"));
        }
    }
    Ok(())
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    Goto {
        url: String,
        #[serde(default)]
        wait_until: NavigationWait,
    },
    Click {
        selector: String,
        #[serde(default)]
        button: Option<ServerMouseButton>,
        #[serde(default)]
        click_count: Option<u32>,
        #[serde(default)]
        delay_ms: Option<u64>,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
    },
    Dblclick {
        selector: String,
        #[serde(default)]
        button: Option<ServerMouseButton>,
        #[serde(default)]
        delay_ms: Option<u64>,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
    },
    Hover {
        selector: String,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
    },
    Focus {
        selector: String,
    },
    Blur {
        selector: String,
    },
    Press {
        selector: String,
        key: String,
        #[serde(default)]
        delay_ms: Option<u64>,
    },
    Type {
        selector: String,
        text: String,
        #[serde(default)]
        delay_ms: Option<u64>,
    },
    Clear {
        selector: String,
        #[serde(default)]
        force: bool,
    },
    Check {
        selector: String,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
    },
    Uncheck {
        selector: String,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
    },
    SelectOption {
        selector: String,
        values: ServerSelectValues,
        #[serde(default)]
        force: bool,
    },
    Tap {
        selector: String,
        #[serde(default)]
        position: Option<ServerPoint>,
        #[serde(default)]
        modifiers: Vec<String>,
        #[serde(default)]
        force: bool,
        #[serde(default)]
        trial: bool,
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
        #[serde(default)]
        strict: bool,
    },
    InnerText {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    InnerHtml {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    InputValue {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    GetAttribute {
        selector: String,
        name: String,
        #[serde(default)]
        strict: bool,
    },
    Count {
        selector: String,
    },
    AllTextContents {
        selector: String,
    },
    AllInnerTexts {
        selector: String,
    },
    IsVisible {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    IsHidden {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    IsEnabled {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    IsDisabled {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    IsChecked {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    IsEditable {
        selector: String,
        #[serde(default)]
        strict: bool,
    },
    Evaluate {
        expression: String,
    },
    // Empty struct variants (rather than unit variants) make serde enforce unknown-field
    // rejection for these actions too.
    Url {},
    Reload {
        #[serde(default)]
        wait_until: NavigationWait,
    },
    GoBack {
        #[serde(default)]
        wait_until: NavigationWait,
    },
    GoForward {
        #[serde(default)]
        wait_until: NavigationWait,
    },
    SetContent {
        html: String,
        #[serde(default)]
        wait_until: NavigationWait,
    },
    WaitForLoadState {
        #[serde(default)]
        state: LoadStateWait,
    },
    WaitForUrl {
        url: String,
        #[serde(default)]
        wait_until: NavigationWait,
    },
    WaitForFunction {
        expression: String,
        #[serde(default)]
        polling_ms: Option<u64>,
    },
    AddInitScript {
        script: String,
    },
    AddScriptTag {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        content: Option<String>,
        #[serde(rename = "type", default)]
        script_type: Option<String>,
    },
    AddStyleTag {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        content: Option<String>,
    },
    Title {},
    Content {},
    Snapshot {
        #[serde(default)]
        interactive_only: bool,
        #[serde(default = "default_true")]
        compact: bool,
        #[serde(default)]
        max_depth: Option<usize>,
        #[serde(default)]
        max_nodes: Option<usize>,
    },
    Markdown {
        #[serde(default)]
        selector: Option<String>,
        #[serde(default)]
        main_content_only: bool,
        #[serde(default = "default_true")]
        include_links: bool,
        #[serde(default)]
        include_images: bool,
    },
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

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerMouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl ServerMouseButton {
    fn into_core(self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Right => MouseButton::Right,
            Self::Middle => MouseButton::Middle,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerSelectValues {
    Single(ServerSelectValue),
    Many(Vec<ServerSelectValue>),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerSelectValue {
    Str(String),
    Obj(ServerSelectObj),
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSelectObj {
    pub value: Option<String>,
    pub label: Option<String>,
    pub index: Option<usize>,
}

impl ServerSelectValue {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Str(_) => Ok(()),
            Self::Obj(o) => {
                let count = usize::from(o.value.is_some())
                    + usize::from(o.label.is_some())
                    + usize::from(o.index.is_some());
                if count != 1 {
                    Err("select_option entries must specify exactly one of value, label, or index".into())
                } else {
                    Ok(())
                }
            }
        }
    }

    fn to_core(&self) -> SelectEntry {
        match self {
            Self::Str(s) => SelectEntry::Value(s.clone()),
            Self::Obj(o) => {
                if let Some(v) = &o.value {
                    SelectEntry::Value(v.clone())
                } else if let Some(l) = &o.label {
                    SelectEntry::Label(l.clone())
                } else {
                    SelectEntry::Index(o.index.expect("validated"))
                }
            }
        }
    }
}

impl ServerSelectValues {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Single(v) => v.validate(),
            Self::Many(list) => {
                for v in list {
                    v.validate()?;
                }
                Ok(())
            }
        }
    }

    fn to_core(&self) -> Vec<SelectEntry> {
        match self {
            Self::Single(v) => vec![v.to_core()],
            Self::Many(list) => list.iter().map(ServerSelectValue::to_core).collect(),
        }
    }
}

fn validate_selector(selector: &str) -> Result<(), String> {
    if selector.trim().is_empty() {
        Err("selectors must not be empty".into())
    } else {
        Ok(())
    }
}

fn validate_delay(delay_ms: Option<u64>) -> Result<(), String> {
    if delay_ms.is_some_and(|ms| ms > 10_000) {
        Err("delay_ms must be between 0 and 10000".into())
    } else {
        Ok(())
    }
}

fn validate_point(pt: Option<ServerPoint>) -> Result<(), String> {
    if pt.is_some_and(|p| !p.x.is_finite() || !p.y.is_finite()) {
        Err("position x and y must be finite numbers".into())
    } else {
        Ok(())
    }
}

fn validate_modifiers(modifiers: &[String]) -> Result<u8, String> {
    parse_modifiers(modifiers).map_err(|e| e.to_string())
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerClip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Serialize, Deserialize)]
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

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerPdfMargin {
    pub top: Option<ServerLength>,
    pub right: Option<ServerLength>,
    pub bottom: Option<ServerLength>,
    pub left: Option<ServerLength>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigationWait {
    #[default]
    Load,
    DomContentLoaded,
    NetworkIdle,
    Commit,
}

impl NavigationWait {
    fn into_core(self) -> WaitUntil {
        match self {
            Self::Load => WaitUntil::Load,
            Self::DomContentLoaded => WaitUntil::DomContentLoaded,
            Self::NetworkIdle => WaitUntil::NetworkIdle,
            Self::Commit => WaitUntil::Commit,
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoadStateWait {
    #[default]
    Load,
    DomContentLoaded,
    NetworkIdle,
}

impl LoadStateWait {
    fn into_core(self) -> WaitUntil {
        match self {
            Self::Load => WaitUntil::Load,
            Self::DomContentLoaded => WaitUntil::DomContentLoaded,
            Self::NetworkIdle => WaitUntil::NetworkIdle,
        }
    }
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
                page.goto_with(url, wait_until.into_core()).await?;
                Ok(Value::Null)
            }
            Step::Click {
                selector,
                button,
                click_count,
                delay_ms,
                position,
                modifiers,
                force,
                trial,
            } => {
                let mods = parse_modifiers(modifiers)?;
                page.click_with(
                    selector,
                    &ClickOptions {
                        button: button.unwrap_or_default().into_core(),
                        click_count: click_count.unwrap_or(1),
                        delay: delay_ms.map(Duration::from_millis),
                        position: position.map(|p| (p.x, p.y)),
                        modifiers: mods,
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Step::Dblclick {
                selector,
                button,
                delay_ms,
                position,
                modifiers,
                force,
                trial,
            } => {
                let mods = parse_modifiers(modifiers)?;
                page.dblclick(
                    selector,
                    &ClickOptions {
                        button: button.unwrap_or_default().into_core(),
                        click_count: 2,
                        delay: delay_ms.map(Duration::from_millis),
                        position: position.map(|p| (p.x, p.y)),
                        modifiers: mods,
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Step::Hover {
                selector,
                position,
                modifiers,
                force,
                trial,
            } => {
                let mods = parse_modifiers(modifiers)?;
                page.hover(
                    selector,
                    &HoverOptions {
                        position: position.map(|p| (p.x, p.y)),
                        modifiers: mods,
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Step::Focus { selector } => {
                page.focus(selector).await?;
                Ok(Value::Null)
            }
            Step::Blur { selector } => {
                page.blur(selector).await?;
                Ok(Value::Null)
            }
            Step::Press {
                selector,
                key,
                delay_ms,
            } => {
                page.press(selector, key, delay_ms.map(Duration::from_millis))
                    .await?;
                Ok(Value::Null)
            }
            Step::Type {
                selector,
                text,
                delay_ms,
            } => {
                page.press_sequentially(selector, text, delay_ms.map(Duration::from_millis))
                    .await?;
                Ok(Value::Null)
            }
            Step::Clear { selector, force } => {
                page.clear(selector, *force).await?;
                Ok(Value::Null)
            }
            Step::Check {
                selector,
                position,
                force,
                trial,
            } => {
                page.check(
                    selector,
                    &CheckOptions {
                        position: position.map(|p| (p.x, p.y)),
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Step::Uncheck {
                selector,
                position,
                force,
                trial,
            } => {
                page.uncheck(
                    selector,
                    &CheckOptions {
                        position: position.map(|p| (p.x, p.y)),
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Step::SelectOption {
                selector,
                values,
                force,
            } => {
                let selected = page.select_option(selector, &values.to_core(), *force).await?;
                Ok(serde_json::to_value(selected).expect("Vec<String> is JSON"))
            }
            Step::Tap {
                selector,
                position,
                modifiers,
                force,
                trial,
            } => {
                let mods = parse_modifiers(modifiers)?;
                page.tap(
                    selector,
                    &HoverOptions {
                        position: position.map(|p| (p.x, p.y)),
                        modifiers: mods,
                        force: *force,
                        trial: *trial,
                    },
                )
                .await?;
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
            Step::TextContent { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(serde_json::to_value(page.text_content(s).await?).expect("an optional string is JSON"))
            }
            Step::InnerText { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::String(page.inner_text(s).await?))
            }
            Step::InnerHtml { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::String(page.inner_html(s).await?))
            }
            Step::InputValue { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::String(page.input_value(s).await?))
            }
            Step::GetAttribute { selector, name, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(serde_json::to_value(page.get_attribute(s, name).await?).expect("an optional string is JSON"))
            }
            Step::Count { selector } => Ok(Value::from(page.count(selector.as_str()).await?)),
            Step::AllTextContents { selector } => {
                Ok(serde_json::to_value(page.all_text_contents(selector.as_str()).await?)
                    .expect("a string vec is JSON"))
            }
            Step::AllInnerTexts { selector } => {
                Ok(serde_json::to_value(page.all_inner_texts(selector.as_str()).await?)
                    .expect("a string vec is JSON"))
            }
            Step::IsVisible { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_visible(s).await?))
            }
            Step::IsHidden { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_hidden(s).await?))
            }
            Step::IsEnabled { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_enabled(s).await?))
            }
            Step::IsDisabled { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_disabled(s).await?))
            }
            Step::IsChecked { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_checked(s).await?))
            }
            Step::IsEditable { selector, strict } => {
                let s = fluxwright::Selector::from(selector.as_str()).strict(*strict);
                Ok(Value::Bool(page.is_editable(s).await?))
            }
            Step::Evaluate { expression } => page.evaluate(expression).await,
            Step::Url {} => Ok(Value::String(page.url())),
            Step::Reload { wait_until } => {
                page.reload_with(wait_until.into_core()).await?;
                Ok(Value::Null)
            }
            Step::GoBack { wait_until } => Ok(Value::Bool(page.go_back_with(wait_until.into_core()).await?)),
            Step::GoForward { wait_until } => Ok(Value::Bool(page.go_forward_with(wait_until.into_core()).await?)),
            Step::SetContent { html, wait_until } => {
                page.set_content_with(html, wait_until.into_core()).await?;
                Ok(Value::Null)
            }
            Step::WaitForLoadState { state } => {
                page.wait_for_load_state(state.into_core()).await?;
                Ok(Value::Null)
            }
            Step::WaitForUrl { url, wait_until } => {
                page.wait_for_url(url, wait_until.into_core()).await?;
                Ok(Value::Null)
            }
            Step::WaitForFunction { expression, polling_ms } => {
                let polling = polling_ms
                    .map(|ms| fluxwright::WaitPolling::Interval(Duration::from_millis(ms)))
                    .unwrap_or_default();
                page.wait_for_function_with(expression, polling).await
            }
            Step::AddInitScript { script } => {
                page.add_init_script(script).await?;
                Ok(Value::Null)
            }
            Step::AddScriptTag { url, content, script_type } => {
                page.add_script_tag(&fluxwright::ScriptTagOptions {
                    url: url.clone(),
                    content: content.clone(),
                    script_type: script_type.clone(),
                })
                .await?;
                Ok(Value::Null)
            }
            Step::AddStyleTag { url, content } => {
                page.add_style_tag(&fluxwright::StyleTagOptions {
                    url: url.clone(),
                    content: content.clone(),
                })
                .await?;
                Ok(Value::Null)
            }
            Step::Title {} => Ok(Value::String(page.title().await?)),
            Step::Content {} => Ok(Value::String(page.content().await?)),
            Step::Snapshot {
                interactive_only,
                compact,
                max_depth,
                max_nodes,
            } => {
                let mut opts = SnapshotOptions {
                    interactive_only: *interactive_only,
                    compact: *compact,
                    max_depth: *max_depth,
                    ..Default::default()
                };
                if let Some(limit) = max_nodes {
                    opts.max_nodes = *limit;
                }
                let snap = page.aria_snapshot_with(&opts).await?;
                Ok(serde_json::to_value(snap).unwrap_or(Value::Null))
            }
            Step::Markdown {
                selector,
                main_content_only,
                include_links,
                include_images,
            } => {
                let md = page
                    .markdown_with(&MarkdownOptions {
                        selector: selector.clone(),
                        main_content_only: *main_content_only,
                        include_links: *include_links,
                        include_images: *include_images,
                    })
                    .await?;
                Ok(Value::String(md))
            }
            Step::Screenshot { kind, .. } => {
                let opts = self.screenshot_options().map_err(fluxwright::Error::Other)?;
                let bytes = page.screenshot_with(&opts).await?;
                encoded_blob(kind.mime(), &bytes, MAX_SCREENSHOT_BYTES, "screenshot")
            }
            Step::Pdf { .. } => {
                let opts = self.pdf_options().map_err(fluxwright::Error::Other)?;
                let bytes = page.pdf_within(&opts, MAX_PDF_BYTES).await?;
                encoded_blob("application/pdf", &bytes, MAX_PDF_BYTES, "pdf")
            }
        }
    }
    pub fn action_name(&self) -> &'static str {
        match self {
            Step::Goto { .. } => "goto",
            Step::Click { .. } => "click",
            Step::Dblclick { .. } => "dblclick",
            Step::Hover { .. } => "hover",
            Step::Focus { .. } => "focus",
            Step::Blur { .. } => "blur",
            Step::Press { .. } => "press",
            Step::Type { .. } => "type",
            Step::Clear { .. } => "clear",
            Step::Check { .. } => "check",
            Step::Uncheck { .. } => "uncheck",
            Step::SelectOption { .. } => "select_option",
            Step::Tap { .. } => "tap",
            Step::Fill { .. } => "fill",
            Step::WaitForSelector { .. } => "wait_for_selector",
            Step::TextContent { .. } => "text_content",
            Step::InnerText { .. } => "inner_text",
            Step::InnerHtml { .. } => "inner_html",
            Step::InputValue { .. } => "input_value",
            Step::GetAttribute { .. } => "get_attribute",
            Step::Count { .. } => "count",
            Step::AllTextContents { .. } => "all_text_contents",
            Step::AllInnerTexts { .. } => "all_inner_texts",
            Step::IsVisible { .. } => "is_visible",
            Step::IsHidden { .. } => "is_hidden",
            Step::IsEnabled { .. } => "is_enabled",
            Step::IsDisabled { .. } => "is_disabled",
            Step::IsChecked { .. } => "is_checked",
            Step::IsEditable { .. } => "is_editable",
            Step::Evaluate { .. } => "evaluate",
            Step::Url {} => "url",
            Step::Reload { .. } => "reload",
            Step::GoBack { .. } => "go_back",
            Step::GoForward { .. } => "go_forward",
            Step::SetContent { .. } => "set_content",
            Step::WaitForLoadState { .. } => "wait_for_load_state",
            Step::WaitForUrl { .. } => "wait_for_url",
            Step::WaitForFunction { .. } => "wait_for_function",
            Step::AddInitScript { .. } => "add_init_script",
            Step::AddScriptTag { .. } => "add_script_tag",
            Step::AddStyleTag { .. } => "add_style_tag",
            Step::Title {} => "title",
            Step::Content {} => "content",
            Step::Snapshot { .. } => "snapshot",
            Step::Markdown { .. } => "markdown",
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
            json!({ "steps": [{ "action": "click", "selector": "a", "modifiers": ["Bogus"] }] }),
            json!({ "steps": [{ "action": "press", "selector": "input", "key": "" }] }),
            json!({ "steps": [{ "action": "press", "selector": "input", "key": "a", "delay_ms": 20000 }] }),
            json!({ "steps": [{ "action": "select_option", "selector": "select", "values": {} }] }),
            json!({ "steps": [{ "action": "select_option", "selector": "select", "values": { "value": "a", "label": "b" } }] }),
            json!({ "steps": [{ "action": "get_attribute", "selector": "a", "name": " " }] }),
        ] {
            let job: JobRequest = serde_json::from_value(value).unwrap();
            assert!(job.validate().is_err());
        }
        assert!(serde_json::from_value::<Step>(json!({
            "action": "click", "selector": "#b", "button": "right", "modifiers": ["Shift"], "position": { "x": 2, "y": 3 }, "force": true
        }))
        .is_ok());
        assert!(serde_json::from_value::<JobRequest>(json!({
            "steps": [
                { "action": "dblclick", "selector": "#d" },
                { "action": "hover", "selector": "#h" },
                { "action": "focus", "selector": "#f" },
                { "action": "blur", "selector": "#f" },
                { "action": "press", "selector": "#i", "key": "Enter" },
                { "action": "type", "selector": "#i", "text": "hi", "delay_ms": 5 },
                { "action": "clear", "selector": "#i" },
                { "action": "check", "selector": "#c" },
                { "action": "uncheck", "selector": "#c" },
                { "action": "select_option", "selector": "select", "values": [{ "label": "Two" }, { "index": 2 }] },
                { "action": "tap", "selector": "#t" },
                { "action": "inner_text", "selector": "p", "strict": true },
                { "action": "inner_html", "selector": "p" },
                { "action": "input_value", "selector": "input" },
                { "action": "get_attribute", "selector": "a", "name": "href" },
                { "action": "count", "selector": "li" },
                { "action": "all_text_contents", "selector": "li" },
                { "action": "all_inner_texts", "selector": "li" },
                { "action": "is_visible", "selector": "#v" },
                { "action": "is_hidden", "selector": "#v" },
                { "action": "is_enabled", "selector": "#v" },
                { "action": "is_disabled", "selector": "#v" },
                { "action": "is_checked", "selector": "#c" },
                { "action": "is_editable", "selector": "#i" }
            ]
        }))
        .unwrap()
        .validate()
        .is_ok());
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

        assert!(serde_json::from_value::<JobRequest>(json!({
            "steps": [
                { "action": "snapshot", "interactive_only": true, "max_depth": 4, "max_nodes": 500 },
                { "action": "markdown", "main_content_only": true, "include_images": true }
            ]
        }))
        .unwrap()
        .validate()
        .is_ok());
        for bad in [
            json!({ "steps": [{ "action": "snapshot", "max_depth": 0 }] }),
            json!({ "steps": [{ "action": "snapshot", "max_nodes": 5001 }] }),
            json!({ "steps": [{ "action": "markdown", "selector": "   " }] }),
        ] {
            let job: JobRequest = serde_json::from_value(bad).unwrap();
            assert!(job.validate().is_err());
        }
    }
}
