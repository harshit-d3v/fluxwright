//! Screenshots in PNG, JPEG or WebP, and PDFs, with Playwright's options.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::page::CdpPage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Webp,
}

/// An area in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Shared CSS rule body for freezing animations and transitions during screenshots.
pub const FREEZE_ANIMATIONS_CSS: &str = "*, *::before, *::after { animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; animation-play-state: paused !important; transition-duration: 0s !important; transition-delay: 0s !important; scroll-behavior: auto !important; }";

/// Shared CSS rule body for hiding the blinking text caret during screenshots.
pub const HIDE_CARET_CSS: &str = "*, *::before, *::after { caret-color: transparent !important; }";

/// Whether CSS animations and transitions are allowed or disabled during a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotAnimations {
    Allow,
    Disabled,
}

/// Whether the text caret is hidden or left at its initial appearance during a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenshotCaret {
    Hide,
    Initial,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenshotOptions {
    /// The whole scrollable page, not just the viewport. Not for element screenshots.
    pub full_page: bool,
    pub format: ImageFormat,
    /// 0–100, JPEG only.
    pub quality: Option<u8>,
    /// Relative to the viewport, or to the page with `full_page`. Not for element screenshots.
    pub clip: Option<Clip>,
    /// A transparent background instead of the page's default white one (PNG).
    pub omit_background: bool,
    /// Overlay Set-of-Marks `@eN` labels on live interactive elements before capture.
    pub annotate: bool,
    /// Selectors for elements to cover with colored mask boxes during capture.
    pub mask: Vec<String>,
    /// CSS color for mask overlays (default `#FF00FF`).
    pub mask_color: Option<String>,
    /// Whether CSS animations/transitions are allowed or disabled during capture.
    pub animations: Option<ScreenshotAnimations>,
    /// Whether the text caret is hidden or left at its initial appearance during capture.
    pub caret: Option<ScreenshotCaret>,
}

/// Lengths as in Playwright: a bare number is pixels, or "1in", "2.5cm", "10mm", "40px".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PdfMargin {
    pub top: Option<String>,
    pub right: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PdfOptions {
    /// Letter (the default), Legal, Tabloid, Ledger or A0–A6. `width`/`height` win over it.
    pub format: Option<String>,
    /// Lengths as in [`PdfMargin`].
    pub width: Option<String>,
    pub height: Option<String>,
    pub margin: PdfMargin,
    pub landscape: bool,
    pub print_background: bool,
    /// 0.1–2, default 1.
    pub scale: Option<f64>,
    /// Such as "1-5, 8". All pages when unset.
    pub page_ranges: Option<String>,
    pub prefer_css_page_size: bool,
    pub display_header_footer: bool,
    pub header_template: Option<String>,
    pub footer_template: Option<String>,
    pub outline: bool,
    pub tagged: bool,
}

/// Playwright's paper sizes, in inches.
fn paper(format: &str) -> Option<(f64, f64)> {
    Some(match format.to_ascii_lowercase().as_str() {
        "letter" => (8.5, 11.0),
        "legal" => (8.5, 14.0),
        "tabloid" => (11.0, 17.0),
        "ledger" => (17.0, 11.0),
        "a0" => (33.1, 46.8),
        "a1" => (23.4, 33.1),
        "a2" => (16.54, 23.4),
        "a3" => (11.7, 16.54),
        "a4" => (8.27, 11.7),
        "a5" => (5.83, 8.27),
        "a6" => (4.13, 5.83),
        _ => return None,
    })
}

/// A Playwright length in inches. A bare number is pixels, 96 to the inch.
fn inches(len: &str) -> Result<f64> {
    let s = len.trim().to_ascii_lowercase();
    let (num, per_inch) = [("px", 96.0), ("in", 1.0), ("cm", 2.54), ("mm", 25.4)]
        .iter()
        .find_map(|(unit, per)| s.strip_suffix(unit).map(|n| (n, *per)))
        .unwrap_or((s.as_str(), 96.0));
    match num.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v / per_inch),
        _ => Err(Error::Other(format!("pdf: invalid length {len:?}"))),
    }
}

/// The largest PDF `pdf()` reads when the caller sets no limit of its own.
pub const MAX_PDF_BYTES: usize = 256 * 1024 * 1024;
/// A clip side, in CSS pixels (Chrome's texture limit), and a clip's area in pixels: more
/// would have the browser allocate gigabytes for one image.
const MAX_CLIP_SIDE: f64 = 16_384.0;
const MAX_CLIP_AREA: f64 = 64_000_000.0;
/// Paper sides and margins, in inches: a poster, not a way to make Chrome lay out kilometres.
const MAX_PAPER_INCHES: f64 = 200.0;

/// Validates a CSS color string for screenshot masking, rejecting empty/control/injection inputs.
pub fn validate_mask_color(color: &str) -> Result<()> {
    if color.chars().any(char::is_control) {
        return Err(Error::Other(
            "screenshot: mask_color must not contain control characters".into(),
        ));
    }
    let trimmed = color.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(Error::Other(
            "screenshot: mask_color must be a 1-128 character CSS color".into(),
        ));
    }
    if trimmed.contains(':')
        || trimmed.contains(';')
        || trimmed.contains('{')
        || trimmed.contains('}')
        || trimmed.contains('<')
        || trimmed.contains('>')
        || trimmed.contains('"')
        || trimmed.contains('\'')
        || trimmed.contains('\\')
        || trimmed.contains("/*")
        || trimmed.contains("*/")
        || trimmed.contains("//")
    {
        return Err(Error::Other(format!(
            "screenshot: invalid mask_color {color:?}"
        )));
    }
    if let Some(hex) = trimmed.strip_prefix('#') {
        if matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(());
        }
        return Err(Error::Other(format!(
            "screenshot: invalid hex mask_color {color:?}"
        )));
    }
    if let Some((fn_name, rest)) = trimmed.split_once('(') {
        let Some(inner) = rest.strip_suffix(')') else {
            return Err(Error::Other(format!(
                "screenshot: invalid mask_color {color:?}"
            )));
        };
        let fname = fn_name.trim().to_ascii_lowercase();
        if !matches!(
            fname.as_str(),
            "rgb"
                | "rgba"
                | "hsl"
                | "hsla"
                | "hwb"
                | "lab"
                | "lch"
                | "oklab"
                | "oklch"
                | "color"
                | "color-mix"
        ) {
            return Err(Error::Other(format!(
                "screenshot: unsupported mask_color function {color:?}"
            )));
        }
        let lower = inner.to_ascii_lowercase();
        if inner.trim().is_empty()
            || lower.contains("url(")
            || lower.contains("expression(")
            || lower.contains("var(")
        {
            return Err(Error::Other(format!(
                "screenshot: invalid mask_color {color:?}"
            )));
        }
        let mut depth = 1i32;
        for ch in inner.chars() {
            match ch {
                '(' => {
                    if fname != "color-mix" {
                        return Err(Error::Other(format!(
                            "screenshot: invalid mask_color {color:?}"
                        )));
                    }
                    depth += 1;
                }
                ')' => {
                    depth -= 1;
                    if depth < 1 {
                        return Err(Error::Other(format!(
                            "screenshot: invalid mask_color {color:?}"
                        )));
                    }
                }
                c if c.is_ascii_alphanumeric()
                    || matches!(c, ' ' | ',' | '.' | '%' | '-' | '+' | '/' | '#') => {}
                _ => {
                    return Err(Error::Other(format!(
                        "screenshot: invalid mask_color {color:?}"
                    )));
                }
            }
        }
        if depth != 1 {
            return Err(Error::Other(format!(
                "screenshot: invalid mask_color {color:?}"
            )));
        }
        return Ok(());
    }
    if trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Ok(());
    }
    Err(Error::Other(format!(
        "screenshot: invalid mask_color {color:?}"
    )))
}

impl ScreenshotOptions {
    pub(crate) fn style_css(&self) -> Option<String> {
        let mut css = String::new();
        if self.animations == Some(ScreenshotAnimations::Disabled) {
            css.push_str(FREEZE_ANIMATIONS_CSS);
        }
        if self.caret == Some(ScreenshotCaret::Hide) {
            if !css.is_empty() {
                css.push(' ');
            }
            css.push_str(HIDE_CARET_CSS);
        }
        if css.is_empty() {
            None
        } else {
            Some(css)
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(quality) = self.quality {
            if self.format == ImageFormat::Png {
                return Err(Error::Other(
                    "screenshot: quality is for JPEG and WebP only".into(),
                ));
            }
            if quality > 100 {
                return Err(Error::Other(format!(
                    "screenshot: quality {quality} is over 100"
                )));
            }
        }
        if let Some(c) = self.clip {
            if !(c.x.is_finite()
                && c.y.is_finite()
                && c.width.is_finite()
                && c.height.is_finite()
                && c.width > 0.0
                && c.height > 0.0)
            {
                return Err(Error::Other("screenshot: the clip is empty".into()));
            }
            if c.width > MAX_CLIP_SIDE
                || c.height > MAX_CLIP_SIDE
                || c.width * c.height > MAX_CLIP_AREA
                || c.x.abs() > 1e7
                || c.y.abs() > 1e7
            {
                return Err(Error::Other(format!(
                    "screenshot: the clip is too large (at most {MAX_CLIP_SIDE} px a side and {MAX_CLIP_AREA} px in all)"
                )));
            }
        }
        for sel in &self.mask {
            if sel.trim().is_empty() {
                return Err(Error::Other(
                    "screenshot: mask selector must not be empty".into(),
                ));
            }
        }
        if let Some(ref color) = self.mask_color {
            validate_mask_color(color)?;
        }
        Ok(())
    }
}

impl PdfOptions {
    pub fn validate(&self) -> Result<()> {
        self.params().map(|_| ())
    }

    fn params(&self) -> Result<Value> {
        let (mut width, mut height) = match &self.format {
            Some(f) => paper(f).ok_or_else(|| Error::Other(format!("pdf: unknown format {f:?}")))?,
            None => (8.5, 11.0),
        };
        if let Some(w) = &self.width {
            width = inches(w)?;
        }
        if let Some(h) = &self.height {
            height = inches(h)?;
        }
        let scale = self.scale.unwrap_or(1.0);
        if !(0.1..=2.0).contains(&scale) {
            return Err(Error::Other(format!("pdf: scale {scale} is outside 0.1–2")));
        }
        let margin = |m: &Option<String>| match m.as_deref().map_or(Ok(0.0), inches) {
            Ok(v) if v > MAX_PAPER_INCHES => Err(Error::Other(format!(
                "pdf: a margin of {v} in is over {MAX_PAPER_INCHES} in"
            ))),
            other => other,
        };
        if width > MAX_PAPER_INCHES || height > MAX_PAPER_INCHES {
            return Err(Error::Other(format!(
                "pdf: paper of {width} x {height} in is over {MAX_PAPER_INCHES} in a side"
            )));
        }
        let mut params = json!({
            "landscape": self.landscape,
            "displayHeaderFooter": self.display_header_footer,
            "printBackground": self.print_background,
            "scale": scale,
            "paperWidth": width,
            "paperHeight": height,
            "marginTop": margin(&self.margin.top)?,
            "marginRight": margin(&self.margin.right)?,
            "marginBottom": margin(&self.margin.bottom)?,
            "marginLeft": margin(&self.margin.left)?,
            "preferCSSPageSize": self.prefer_css_page_size,
            "generateTaggedPDF": self.tagged,
            "generateDocumentOutline": self.outline,
            // A big PDF as one JSON message would sit in memory twice; read it in chunks instead.
            "transferMode": "ReturnAsStream",
        });
        if let Some(r) = &self.page_ranges {
            params["pageRanges"] = json!(r);
        }
        if let Some(t) = &self.header_template {
            params["headerTemplate"] = json!(t);
        }
        if let Some(t) = &self.footer_template {
            params["footerTemplate"] = json!(t);
        }
        Ok(params)
    }
}

fn decode(b64: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Other(e.to_string()))
}

pub(crate) const REMOVE_SCREENSHOT_STYLE_JS: &str = r#"(() => {
    for (const el of document.querySelectorAll('#__fluxwright_screenshot_style')) el.remove();
})()"#;

const CLEANUP_OVERLAYS_AND_STYLE_JS: &str = r#"(() => {
    for (const id of ['__fluxwright_som_overlay', '__fluxwright_mask_overlay', '__fluxwright_screenshot_style']) {
        for (const el of document.querySelectorAll('#' + id)) el.remove();
    }
})()"#;

pub(crate) fn inject_style_js(css: &str) -> String {
    format!(
        r#"((css) => {{
            const root = document.head || document.documentElement;
            if (!root) return false;
            let s = document.getElementById('__fluxwright_screenshot_style');
            if (!s) {{
                s = document.createElement('style');
                s.id = '__fluxwright_screenshot_style';
                root.appendChild(s);
            }}
            s.textContent = css;
            void document.documentElement?.offsetHeight;
            return true;
        }})({})"#,
        serde_json::to_string(css).unwrap(),
    )
}

fn inject_mask_overlay_js(rects: &[(f64, f64, f64, f64)], color: &str) -> String {
    let rects_json: Vec<Value> = rects
        .iter()
        .map(|&(x, y, width, height)| json!({ "x": x, "y": y, "width": width, "height": height }))
        .collect();
    format!(
        r#"((rects, color) => {{
            const root = document.documentElement || document.body;
            if (!root || !rects.length) return false;
            document.querySelectorAll('#__fluxwright_mask_overlay').forEach((e) => e.remove());
            const host = document.createElement('div');
            host.id = '__fluxwright_mask_overlay';
            host.style.cssText = 'position:fixed!important;top:0!important;left:0!important;width:0!important;height:0!important;overflow:visible!important;pointer-events:none!important;z-index:2147483647!important;display:block!important;visibility:visible!important;opacity:1!important;margin:0!important;padding:0!important;border:0!important;';
            const container = typeof host.attachShadow === 'function' ? host.attachShadow({{ mode: 'open' }}) : host;
            for (const r of rects) {{
                const box = document.createElement('div');
                box.style.cssText = 'position:fixed;pointer-events:none;box-sizing:border-box;margin:0;padding:0;border:0;';
                box.style.left = r.x + 'px';
                box.style.top = r.y + 'px';
                box.style.width = r.width + 'px';
                box.style.height = r.height + 'px';
                box.style.backgroundColor = color;
                container.appendChild(box);
            }}
            root.appendChild(host);
            void document.documentElement?.offsetHeight;
            return true;
        }})({}, {})"#,
        Value::Array(rects_json),
        serde_json::to_string(color).unwrap(),
    )
}

fn inject_som_overlay_js(boxes: &[(String, f64, f64, f64, f64)]) -> String {
    let boxes_json: Vec<Value> = boxes
        .iter()
        .map(|(id, x, y, width, height)| {
            json!({ "id": id, "x": x, "y": y, "width": width, "height": height })
        })
        .collect();
    format!(
        r#"((boxes) => {{
            const root = document.documentElement || document.body;
            if (!root || !boxes.length) return false;
            document.querySelectorAll('#__fluxwright_som_overlay').forEach((e) => e.remove());
            const host = document.createElement('div');
            host.id = '__fluxwright_som_overlay';
            host.style.cssText = 'position:fixed!important;top:0!important;left:0!important;width:0!important;height:0!important;overflow:visible!important;pointer-events:none!important;z-index:2147483646!important;display:block!important;visibility:visible!important;opacity:1!important;margin:0!important;padding:0!important;border:0!important;';
            const container = typeof host.attachShadow === 'function' ? host.attachShadow({{ mode: 'open' }}) : host;
            const palette = ['#EF4444', '#2563EB', '#16A34A', '#D97706', '#9333EA', '#0891B2', '#DB2777', '#4F46E5'];
            for (let i = 0; i < boxes.length; i++) {{
                const b = boxes[i];
                const color = palette[i % palette.length];
                const box = document.createElement('div');
                box.style.cssText = `position:fixed;left:${{b.x}}px;top:${{b.y}}px;width:${{b.width}}px;height:${{b.height}}px;box-sizing:border-box;border:2px solid ${{color}};background:${{color}}1A;pointer-events:none;margin:0;padding:0;`;
                const pill = document.createElement('span');
                pill.textContent = '@' + b.id;
                const pillTop = b.y >= 18 ? -16 : 0;
                pill.style.cssText = `position:absolute;left:0;top:${{pillTop}}px;background:${{color}};color:#ffffff;font:700 11px/1 monospace;padding:2px 4px;border-radius:3px;white-space:nowrap;box-shadow:0 1px 2px rgba(0,0,0,0.35);pointer-events:none;`;
                box.appendChild(pill);
                container.appendChild(box);
            }}
            root.appendChild(host);
            void document.documentElement?.offsetHeight;
            return true;
        }})({})"#,
        Value::Array(boxes_json),
    )
}

impl CdpPage {
    /// The page printed to PDF, as for print media. Chrome prints only when headless.
    /// Fails, without reading on, once the PDF is over `max_bytes`.
    pub async fn pdf(&self, options: &PdfOptions, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>> {
        if !self.headless {
            return Err(Error::Other("pdf: only works in headless mode".into()));
        }
        let res = self.call("Page.printToPDF", options.params()?, timeout).await?;
        let handle = res["stream"]
            .as_str()
            .ok_or_else(|| Error::Other("pdf: no stream".into()))?
            .to_string();
        let read = self.read_stream(&handle, max_bytes, timeout).await;
        let _ = self.call("IO.close", json!({ "handle": handle }), timeout).await;
        read
    }

    async fn read_stream(&self, handle: &str, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let chunk = self
                .call("IO.read", json!({ "handle": handle, "size": 1 << 20 }), timeout)
                .await?;
            let data = chunk["data"].as_str().unwrap_or_default();
            if chunk["base64Encoded"].as_bool() == Some(true) {
                out.extend(decode(data)?);
            } else {
                out.extend_from_slice(data.as_bytes());
            }
            if out.len() > max_bytes {
                return Err(Error::Other(format!("pdf: larger than the limit of {max_bytes} bytes")));
            }
            // Anything but an explicit `eof: false` ends it, so a malformed answer can't loop.
            if chunk["eof"].as_bool() != Some(false) {
                return Ok(out);
            }
        }
    }

    async fn prepare_capture_overlays(&self, options: &ScreenshotOptions, timeout: Duration) -> Result<bool> {
        let style_css = options.style_css();
        let needs_dom = style_css.is_some() || !options.mask.is_empty() || options.annotate;
        if !needs_dom {
            return Ok(false);
        }

        if let Some(ref css) = style_css {
            let js = inject_style_js(css);
            self.eval_isolated(&js, timeout).await?;
        }

        if !options.mask.is_empty() {
            let rects = self.collect_mask_rects(&options.mask, timeout).await?;
            if !rects.is_empty() {
                let color = options.mask_color.as_deref().unwrap_or("#FF00FF");
                let js = inject_mask_overlay_js(&rects, color);
                self.eval_isolated(&js, timeout).await?;
            }
        }

        if options.annotate {
            let include_offscreen = options.full_page || options.clip.is_some();
            if let Ok(boxes) = self.collect_som_boxes(include_offscreen, timeout).await {
                if !boxes.is_empty() {
                    let js = inject_som_overlay_js(&boxes);
                    let _ = self.eval_isolated(&js, timeout).await;
                }
            }
        }

        Ok(true)
    }

    /// `Page.captureScreenshot` with `params` (clip, captureBeyondViewport) plus the format,
    /// quality, background, style guards, mask overlays, and Set-of-Marks annotations of `options`.
    pub(crate) async fn capture(
        &self,
        mut params: Value,
        options: &ScreenshotOptions,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        options.validate()?;
        params["format"] = json!(match options.format {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Webp => "webp",
        });
        if let Some(quality) = options.quality {
            params["quality"] = json!(quality);
        }

        let needs_dom = options.style_css().is_some() || !options.mask.is_empty() || options.annotate;
        let prep_res = self.prepare_capture_overlays(options, timeout).await;
        let mut bg_overridden = false;
        let cap_res = if let Err(e) = prep_res {
            Err(e)
        } else {
            let bg_res = if options.omit_background {
                let clear = json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } });
                match self
                    .call("Emulation.setDefaultBackgroundColorOverride", clear, timeout)
                    .await
                {
                    Ok(_) => {
                        bg_overridden = true;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            } else {
                Ok(())
            };
            match bg_res {
                Err(e) => Err(e),
                Ok(()) => self.call("Page.captureScreenshot", params, timeout).await,
            }
        };

        let cleanup_timeout = timeout.max(Duration::from_secs(2));
        if needs_dom && !self.is_closed() {
            let _ = self
                .eval_isolated(CLEANUP_OVERLAYS_AND_STYLE_JS, cleanup_timeout)
                .await;
        }
        if bg_overridden && !self.is_closed() {
            // Reset even when the capture failed, or later screenshots stay transparent.
            let _ = self
                .call(
                    "Emulation.setDefaultBackgroundColorOverride",
                    json!({}),
                    cleanup_timeout,
                )
                .await;
        }

        let res = cap_res?;
        decode(
            res["data"]
                .as_str()
                .ok_or_else(|| Error::Other("screenshot: no data".into()))?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_read_as_playwright_does() {
        assert_eq!(inches("96").unwrap(), 1.0);
        assert_eq!(inches("48px").unwrap(), 0.5);
        assert_eq!(inches("2in").unwrap(), 2.0);
        assert_eq!(inches(" 2.54cm ").unwrap(), 1.0);
        assert_eq!(inches("25.4MM").unwrap(), 1.0);
        for bad in ["", "px", "-1in", "1pt", "abc", "NaN", "inf", "1é"] {
            assert!(inches(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pdf_params_follow_format_size_and_margins() {
        let p = PdfOptions::default().params().unwrap();
        assert_eq!((p["paperWidth"].as_f64(), p["paperHeight"].as_f64()), (Some(8.5), Some(11.0)));
        assert_eq!(p["scale"], 1.0);
        assert_eq!(p["transferMode"], "ReturnAsStream");

        let p = PdfOptions {
            format: Some("A4".into()),
            height: Some("5in".into()),
            margin: PdfMargin { top: Some("1in".into()), left: Some("96".into()), ..Default::default() },
            ..Default::default()
        }
        .params()
        .unwrap();
        assert_eq!((p["paperWidth"].as_f64(), p["paperHeight"].as_f64()), (Some(8.27), Some(5.0)));
        assert_eq!((p["marginTop"].as_f64(), p["marginLeft"].as_f64()), (Some(1.0), Some(1.0)));
        assert_eq!(p["marginBottom"].as_f64(), Some(0.0));

        assert!(PdfOptions { format: Some("A7".into()), ..Default::default() }.params().is_err());
        assert!(PdfOptions { scale: Some(3.0), ..Default::default() }.params().is_err());
        assert!(PdfOptions { scale: Some(f64::NAN), ..Default::default() }.params().is_err());
        assert!(PdfOptions { scale: Some(0.05), ..Default::default() }.params().is_err());

        let rich = PdfOptions {
            landscape: true,
            print_background: true,
            scale: Some(1.5),
            page_ranges: Some("1-2".into()),
            prefer_css_page_size: true,
            display_header_footer: true,
            header_template: Some("<span>H</span>".into()),
            footer_template: Some("<span>F</span>".into()),
            outline: true,
            tagged: true,
            ..Default::default()
        }
        .params()
        .unwrap();
        assert_eq!(rich["landscape"], true);
        assert_eq!(rich["printBackground"], true);
        assert_eq!(rich["scale"], 1.5);
        assert_eq!(rich["pageRanges"], "1-2");
        assert_eq!(rich["preferCSSPageSize"], true);
        assert_eq!(rich["displayHeaderFooter"], true);
        assert_eq!(rich["headerTemplate"], "<span>H</span>");
        assert_eq!(rich["footerTemplate"], "<span>F</span>");
        assert_eq!(rich["generateDocumentOutline"], true);
        assert_eq!(rich["generateTaggedPDF"], true);
    }

    #[tokio::test]
    async fn headed_pdf_fails_and_stream_closes_on_read_error() {
        use std::sync::Arc;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use crate::browser::{BrowserId, ContextId, SessionId, TargetId};
        use crate::connection::Connection;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("");
                let _ = seen_tx.send(method.to_string());
                let reply = match method {
                    "Page.printToPDF" => json!({ "id": id, "result": { "stream": "s-1" } }),
                    "IO.read" => json!({ "id": id, "error": { "code": -32000, "message": "read failed" } }),
                    _ => json!({ "id": id, "result": {} }),
                };
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        });

        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let mut page = CdpPage {
            browser: Arc::clone(&conn),
            browser_pid: 0,
            browser_id: BrowserId(uuid::Uuid::new_v4()),
            headless: false,
            is_remote: false,
            context_id: ContextId("ctx".into()),
            target_id: TargetId("tgt".into()),
            opener_id: None,
            session_id: SessionId("sid".into()),
            targets: Default::default(),
            worlds: Default::default(),
            setups: Default::default(),
            page_setups: Default::default(),
            frames: Default::default(),
            workers: Default::default(),
            closed: Default::default(),
            mouse: Default::default(),
            keyboard: Default::default(),
            url: Arc::new(tokio::sync::watch::Sender::new("about:blank".into())),
            net: Default::default(),
            ax_refs: Default::default(),
            last_snapshot: Default::default(),
            early_log_events: Default::default(),
        };

        let err = page
            .pdf(&PdfOptions::default(), MAX_PDF_BYTES, Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("headless"), "{err}");

        page.headless = true;
        let err = page
            .pdf(&PdfOptions::default(), MAX_PDF_BYTES, Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("read failed"), "{err}");

        let mut methods = Vec::new();
        while let Ok(m) = seen_rx.try_recv() {
            methods.push(m);
        }
        assert_eq!(methods, vec!["Page.printToPDF", "IO.read", "IO.close"]);
    }

    #[test]
    fn validate_mask_color_and_screenshot_options() {
        for ok in [
            "#f0f",
            "#FF00FF",
            "#ff00ff80",
            "magenta",
            "transparent",
            "rgb(255, 0, 255)",
            "rgba(255, 0, 255, 0.5)",
            "hsl(300, 100%, 50%)",
            "oklch(0.6 0.2 300 / 50%)",
            "color-mix(in srgb, red 50%, blue)",
        ] {
            assert!(validate_mask_color(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "   ",
            "red\nblue",
            "red\rblue",
            "red\x00blue",
            "red; background: url(evil)",
            "red:blue",
            "red{color:blue}",
            "<script>",
            "url(https://evil.example/x)",
            "expression(alert(1))",
            "var(--secret)",
            "#12",
            "#zzzzzz",
            "red !important",
        ] {
            assert!(validate_mask_color(bad).is_err(), "{bad:?}");
        }
        assert!(ScreenshotOptions {
            mask: vec!["   ".into()],
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(ScreenshotOptions {
            mask_color: Some("red;color:blue".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[tokio::test]
    async fn capture_cleans_up_overlays_even_when_screenshot_fails() {
        use std::sync::Arc;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use crate::browser::{BrowserId, ContextId, SessionId, TargetId};
        use crate::connection::Connection;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();

        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("");
                let params = req["params"].clone();
                let _ = seen_tx.send((method.to_string(), params));
                let reply = match method {
                    "Page.createIsolatedWorld" => json!({ "id": id, "result": { "executionContextId": 7 } }),
                    "Page.captureScreenshot" => json!({ "id": id, "error": { "code": -32000, "message": "capture boom" } }),
                    "Runtime.evaluate" => json!({ "id": id, "result": { "result": { "type": "boolean", "value": true } } }),
                    _ => json!({ "id": id, "result": {} }),
                };
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        });

        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let page = CdpPage {
            browser: Arc::clone(&conn),
            browser_pid: 0,
            browser_id: BrowserId(uuid::Uuid::new_v4()),
            headless: true,
            is_remote: false,
            context_id: ContextId("ctx".into()),
            target_id: TargetId("tgt".into()),
            opener_id: None,
            session_id: SessionId("sid".into()),
            targets: Default::default(),
            worlds: Default::default(),
            setups: Default::default(),
            page_setups: Default::default(),
            frames: Default::default(),
            workers: Default::default(),
            closed: Default::default(),
            mouse: Default::default(),
            keyboard: Default::default(),
            url: Arc::new(tokio::sync::watch::Sender::new("about:blank".into())),
            net: Default::default(),
            ax_refs: Default::default(),
            last_snapshot: Default::default(),
            early_log_events: Default::default(),
        };

        let err = page
            .capture(
                json!({}),
                &ScreenshotOptions {
                    animations: Some(ScreenshotAnimations::Disabled),
                    caret: Some(ScreenshotCaret::Hide),
                    omit_background: true,
                    ..Default::default()
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("capture boom"), "{err}");

        let mut events = Vec::new();
        while let Ok(item) = seen_rx.try_recv() {
            events.push(item);
        }
        let methods: Vec<&str> = events.iter().map(|(m, _)| m.as_str()).collect();
        assert!(methods.contains(&"Page.captureScreenshot"), "{methods:?}");
        let cap_idx = methods.iter().position(|m| *m == "Page.captureScreenshot").unwrap();
        assert!(
            events[cap_idx + 1..].iter().any(|(m, p)| {
                m == "Runtime.evaluate"
                    && p["expression"]
                        .as_str()
                        .is_some_and(|e| e.contains("__fluxwright_som_overlay") && e.contains(".remove()"))
            }),
            "expected overlay/style cleanup after captureScreenshot failure: {events:?}"
        );
        assert!(
            methods[cap_idx + 1..].contains(&"Emulation.setDefaultBackgroundColorOverride"),
            "expected background override reset after failure: {methods:?}"
        );
    }
}
