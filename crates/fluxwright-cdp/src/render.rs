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

impl ScreenshotOptions {
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

    /// `Page.captureScreenshot` with `params` (clip, captureBeyondViewport) plus the format,
    /// quality and background of `options`.
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
        if options.omit_background {
            let clear = json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } });
            self.call("Emulation.setDefaultBackgroundColorOverride", clear, timeout)
                .await?;
        }
        let res = self.call("Page.captureScreenshot", params, timeout).await;
        if options.omit_background {
            // Reset even when the capture failed, or later screenshots stay transparent.
            let _ = self
                .call("Emulation.setDefaultBackgroundColorOverride", json!({}), timeout)
                .await;
        }
        let res = res?;
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
            context_id: ContextId("ctx".into()),
            target_id: TargetId("tgt".into()),
            session_id: SessionId("sid".into()),
            targets: Default::default(),
            worlds: Default::default(),
            setups: Default::default(),
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
}
