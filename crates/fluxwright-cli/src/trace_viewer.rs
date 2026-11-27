use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use axum::{response::Html, routing::get, Router};
use base64::Engine as _;
use flate2::read::DeflateDecoder;
use serde::Serialize;
use serde_json::Value;

const MAX_ENTRIES: usize = 10_000;
const MAX_ENTRY_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceBundle {
    pub title: String,
    pub browser_name: String,
    pub platform: String,
    pub actions: Vec<ActionRow>,
    pub frames: Vec<FrameRow>,
    pub logs: Vec<LogRow>,
    pub network: Vec<NetworkRow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRow {
    pub call_id: String,
    pub api_name: String,
    pub class: String,
    pub method: String,
    pub params: String,
    pub start_ms: f64,
    pub end_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub error: Option<String>,
    pub page_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameRow {
    pub timestamp_ms: f64,
    pub width: u64,
    pub height: u64,
    pub page_id: String,
    pub data_uri: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRow {
    pub kind: String,
    pub text: String,
    pub time_ms: f64,
    pub page_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRow {
    pub method: String,
    pub url: String,
    pub status: u64,
    pub status_text: String,
    pub mime_type: String,
    pub size: i64,
    pub duration_ms: f64,
    pub failed_text: Option<String>,
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16> {
    let slice = bytes
        .get(offset..offset + 2)
        .context("unexpected end of zip while reading u16")?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .context("unexpected end of zip while reading u32")?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Reads Store (0) and Deflate (8) entries from a standard ZIP archive with bounded sizes.
pub fn read_zip_entries(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    if bytes.len() < 22 {
        bail!("invalid zip archive: smaller than 22-byte EOCD header");
    }
    let min_eocd_pos = bytes.len().saturating_sub(65_557);
    let mut eocd_pos = None;
    let mut pos = bytes.len() - 22;
    loop {
        if read_u32_le(bytes, pos)? == 0x0605_4b50 {
            let comment_len = usize::from(read_u16_le(bytes, pos + 20)?);
            if pos + 22 + comment_len == bytes.len() {
                eocd_pos = Some(pos);
                break;
            }
        }
        if pos <= min_eocd_pos {
            break;
        }
        pos -= 1;
    }
    let eocd = eocd_pos.context("invalid zip archive: EOCD record not found")?;
    let total_entries = usize::from(read_u16_le(bytes, eocd + 10)?);
    if total_entries > MAX_ENTRIES {
        bail!("zip archive exceeds maximum {MAX_ENTRIES} entries");
    }
    let cd_size = usize::try_from(read_u32_le(bytes, eocd + 12)?)
        .context("zip central directory size overflow")?;
    let cd_offset = usize::try_from(read_u32_le(bytes, eocd + 16)?)
        .context("zip central directory offset overflow")?;
    let cd_end = cd_offset
        .checked_add(cd_size)
        .filter(|&end| end <= eocd)
        .context("zip central directory out of bounds")?;

    let mut entries = BTreeMap::new();
    let mut cursor = cd_offset;
    let mut total_decoded = 0usize;

    for _ in 0..total_entries {
        if cursor.checked_add(46).is_none_or(|end| end > cd_end) {
            bail!("truncated zip central directory header");
        }
        if read_u32_le(bytes, cursor)? != 0x0201_4b50 {
            bail!("invalid zip central directory signature at offset {cursor}");
        }
        let method = read_u16_le(bytes, cursor + 10)?;
        let compressed_size = usize::try_from(read_u32_le(bytes, cursor + 20)?)
            .context("zip compressed size overflow")?;
        let uncompressed_size = usize::try_from(read_u32_le(bytes, cursor + 24)?)
            .context("zip uncompressed size overflow")?;
        let name_len = usize::from(read_u16_le(bytes, cursor + 28)?);
        let extra_len = usize::from(read_u16_le(bytes, cursor + 30)?);
        let comment_len = usize::from(read_u16_le(bytes, cursor + 32)?);
        let lfh_offset = usize::try_from(read_u32_le(bytes, cursor + 42)?)
            .context("zip local header offset overflow")?;

        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        let next_cursor = name_end + extra_len + comment_len;
        if next_cursor > cd_end {
            bail!("truncated zip central directory entry");
        }
        let name = std::str::from_utf8(&bytes[name_start..name_end])
            .context("zip entry filename is not valid UTF-8")?
            .to_string();
        cursor = next_cursor;

        if name.ends_with('/') {
            continue;
        }
        if uncompressed_size > MAX_ENTRY_BYTES {
            bail!("zip entry `{name}` exceeds {MAX_ENTRY_BYTES}-byte limit");
        }
        if lfh_offset.checked_add(30).is_none_or(|end| end > cd_offset) {
            bail!("zip local header for `{name}` out of bounds");
        }
        if read_u32_le(bytes, lfh_offset)? != 0x0403_4b50 {
            bail!("invalid zip local file header signature for `{name}`");
        }
        let lfh_name_len = usize::from(read_u16_le(bytes, lfh_offset + 26)?);
        let lfh_extra_len = usize::from(read_u16_le(bytes, lfh_offset + 28)?);
        let data_start = lfh_offset
            .checked_add(30)
            .and_then(|v| v.checked_add(lfh_name_len))
            .and_then(|v| v.checked_add(lfh_extra_len))
            .context("zip entry data offset overflow")?;
        let data_end = data_start
            .checked_add(compressed_size)
            .filter(|&end| end <= cd_offset)
            .context("zip entry payload out of bounds")?;
        let payload = &bytes[data_start..data_end];

        let decoded = match method {
            0 => {
                if payload.len() > MAX_ENTRY_BYTES {
                    bail!("zip entry `{name}` exceeds {MAX_ENTRY_BYTES}-byte limit");
                }
                payload.to_vec()
            }
            8 => {
                let mut decoder = DeflateDecoder::new(payload).take((MAX_ENTRY_BYTES + 1) as u64);
                let mut out = Vec::with_capacity(uncompressed_size.min(MAX_ENTRY_BYTES));
                decoder
                    .read_to_end(&mut out)
                    .with_context(|| format!("failed to inflate zip entry `{name}`"))?;
                if out.len() > MAX_ENTRY_BYTES {
                    bail!("zip entry `{name}` exceeds {MAX_ENTRY_BYTES}-byte limit");
                }
                out
            }
            other => bail!("unsupported zip compression method {other} for `{name}`"),
        };

        total_decoded = total_decoded
            .checked_add(decoded.len())
            .filter(|&tot| tot <= MAX_TOTAL_BYTES)
            .context("zip archive exceeds 256 MiB uncompressed limit")?;
        entries.insert(name, decoded);
    }

    Ok(entries)
}

fn find_ndjson<'a>(
    entries: &'a BTreeMap<String, Vec<u8>>,
    preferred: &str,
    suffix: &str,
) -> Option<&'a [u8]> {
    if let Some(exact) = entries.get(preferred) {
        return Some(exact.as_slice());
    }
    entries
        .iter()
        .find(|(name, _)| name.ends_with(suffix))
        .map(|(_, data)| data.as_slice())
}

fn frame_mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

fn format_error_value(err: &Value) -> String {
    if let Some(msg) = err.get("message").and_then(Value::as_str) {
        return msg.to_string();
    }
    if let Some(nested) = err
        .get("error")
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
    {
        return nested.to_string();
    }
    if let Some(s) = err.as_str() {
        return s.to_string();
    }
    err.to_string()
}

/// Parses a Fluxwright or Playwright `.zip` trace bundle into structured timeline data.
pub fn parse_trace_archive(bytes: &[u8]) -> Result<TraceBundle> {
    let entries = read_zip_entries(bytes)?;
    let trace_bytes = find_ndjson(&entries, "trace.trace", ".trace")
        .context("trace archive does not contain trace.trace")?;
    let trace_text = std::str::from_utf8(trace_bytes).context("trace.trace is not valid UTF-8")?;

    let mut title = String::from("Fluxwright Trace");
    let mut browser_name = String::from("chromium");
    let mut platform = String::new();
    let mut actions_by_id: HashMap<String, ActionRow> = HashMap::new();
    let mut action_order: Vec<String> = Vec::new();
    let mut frames = Vec::new();
    let mut logs = Vec::new();

    for line in trace_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = ev.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "context-options" => {
                if let Some(t) = ev.get("title").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    title = t.to_string();
                }
                if let Some(b) = ev.get("browserName").and_then(Value::as_str) {
                    browser_name = b.to_string();
                }
                if let Some(p) = ev.get("platform").and_then(Value::as_str) {
                    platform = p.to_string();
                }
            }
            "before" => {
                let call_id = ev
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if call_id.is_empty() {
                    continue;
                }
                let class = ev
                    .get("class")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let method = ev
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let api_name = ev
                    .get("apiName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| method.clone());
                let params = ev
                    .get("params")
                    .map(|p| serde_json::to_string_pretty(p).unwrap_or_else(|_| "{}".into()))
                    .unwrap_or_else(|| "{}".into());
                let start_ms = ev.get("startTime").and_then(Value::as_f64).unwrap_or(0.0);
                let page_id = ev
                    .get("pageId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();

                if !actions_by_id.contains_key(&call_id) {
                    action_order.push(call_id.clone());
                }
                actions_by_id.insert(
                    call_id.clone(),
                    ActionRow {
                        call_id,
                        api_name,
                        class,
                        method,
                        params,
                        start_ms,
                        end_ms: None,
                        duration_ms: None,
                        error: None,
                        page_id,
                    },
                );
            }
            "after" => {
                let Some(call_id) = ev.get("callId").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(row) = actions_by_id.get_mut(call_id) {
                    if let Some(end_ms) = ev.get("endTime").and_then(Value::as_f64) {
                        row.end_ms = Some(end_ms);
                        row.duration_ms = Some((end_ms - row.start_ms).max(0.0));
                    }
                    if let Some(err) = ev.get("error") {
                        row.error = Some(format_error_value(err));
                    }
                }
            }
            "screencast-frame" => {
                let Some(sha1) = ev.get("sha1").and_then(Value::as_str) else {
                    continue;
                };
                let rel_key = format!("resources/{sha1}");
                let Some((key, img_bytes)) = entries
                    .get_key_value(&rel_key)
                    .or_else(|| entries.get_key_value(sha1))
                else {
                    continue;
                };
                let mime = frame_mime_for_path(key);
                let b64 = base64::engine::general_purpose::STANDARD.encode(img_bytes);
                frames.push(FrameRow {
                    timestamp_ms: ev.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0),
                    width: ev.get("width").and_then(Value::as_u64).unwrap_or(0),
                    height: ev.get("height").and_then(Value::as_u64).unwrap_or(0),
                    page_id: ev
                        .get("pageId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    data_uri: format!("data:{mime};base64,{b64}"),
                });
            }
            "console" => {
                let text = ev
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let msg_kind = ev
                    .get("messageType")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .to_string();
                let time_ms = ev.get("time").and_then(Value::as_f64).unwrap_or(0.0);
                let page_id = ev
                    .get("pageId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                logs.push(LogRow {
                    kind: msg_kind,
                    text,
                    time_ms,
                    page_id,
                });
            }
            "event" if ev.get("method").and_then(Value::as_str) == Some("pageError") => {
                let err_val = ev
                    .get("params")
                    .and_then(|p| p.get("error"))
                    .unwrap_or(&Value::Null);
                let text = format_error_value(err_val);
                let time_ms = ev.get("time").and_then(Value::as_f64).unwrap_or(0.0);
                let page_id = ev
                    .get("pageId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                logs.push(LogRow {
                    kind: "pageerror".into(),
                    text,
                    time_ms,
                    page_id,
                });
            }
            _ => {}
        }
    }

    let mut actions: Vec<ActionRow> = action_order
        .into_iter()
        .filter_map(|id| actions_by_id.remove(&id))
        .collect();
    actions.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));
    frames.sort_by(|a, b| a.timestamp_ms.total_cmp(&b.timestamp_ms));
    logs.sort_by(|a, b| a.time_ms.total_cmp(&b.time_ms));

    let mut network = Vec::new();
    if let Some(net_bytes) = find_ndjson(&entries, "trace.network", ".network") {
        let net_text = std::str::from_utf8(net_bytes).unwrap_or("");
        for line in net_text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if ev.get("type").and_then(Value::as_str) != Some("resource-snapshot") {
                continue;
            }
            let snap = &ev["snapshot"];
            let req = &snap["request"];
            let resp = &snap["response"];
            network.push(NetworkRow {
                method: req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string(),
                url: req
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: resp.get("status").and_then(Value::as_u64).unwrap_or(0),
                status_text: resp
                    .get("statusText")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                mime_type: resp
                    .get("content")
                    .and_then(|c| c.get("mimeType"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                size: resp
                    .get("content")
                    .and_then(|c| c.get("size"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                duration_ms: snap.get("time").and_then(Value::as_f64).unwrap_or(0.0),
                failed_text: resp
                    .get("_failureText")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }

    Ok(TraceBundle {
        title,
        browser_name,
        platform,
        actions,
        frames,
        logs,
        network,
    })
}

fn escape_html_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Renders a standalone offline HTML viewer with trace JSON safely script-escaped.
pub fn render_html(bundle: &TraceBundle) -> Result<String> {
    let raw_json = serde_json::to_string(bundle).context("failed to serialize trace bundle")?;
    let safe_json = raw_json
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e");
    let safe_title = escape_html_text(&bundle.title);
    let safe_env = escape_html_text(&format!(
        "{}{}",
        bundle.browser_name,
        if bundle.platform.is_empty() {
            String::new()
        } else {
            format!(" · {}", bundle.platform)
        }
    ));

    let template = include_str!("trace_viewer_template.html");
    Ok(template
        .replace("__TITLE__", &safe_title)
        .replace("__ENV__", &safe_env)
        .replace("__TRACE_JSON__", &safe_json))
}

pub async fn run(trace: PathBuf, out: Option<PathBuf>, listen: SocketAddr) -> Result<()> {
    let bytes = std::fs::read(&trace)
        .with_context(|| format!("failed to read trace archive {}", trace.display()))?;
    let bundle = parse_trace_archive(&bytes)?;
    let html = render_html(&bundle)?;

    if let Some(out_path) = out {
        if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create output directory {}", parent.display())
            })?;
        }
        std::fs::write(&out_path, &html)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!(
            "Wrote trace viewer HTML to {} ({} actions, {} frames, {} network requests)",
            out_path.display(),
            bundle.actions.len(),
            bundle.frames.len(),
            bundle.network.len()
        );
        return Ok(());
    }

    let html_body = std::sync::Arc::new(html);
    let app = Router::new()
        .route(
            "/",
            get({
                let html_body = std::sync::Arc::clone(&html_body);
                move || {
                    let body = std::sync::Arc::clone(&html_body);
                    async move { Html(body.as_ref().clone()) }
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind {listen}"))?;
    let addr = listener.local_addr()?;
    println!(
        "Fluxwright Trace Viewer: http://{addr}/ ({} actions, {} frames, {} network requests) - Press Ctrl+C to stop",
        bundle.actions.len(),
        bundle.frames.len(),
        bundle.network.len()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    fn make_test_zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();

        for &(name, raw, use_deflate) in entries {
            let (method, compressed): (u16, Vec<u8>) = if use_deflate {
                let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
                enc.write_all(raw).unwrap();
                (8, enc.finish().unwrap())
            } else {
                (0, raw.to_vec())
            };
            let crc = crc32(raw);
            let offset = out.len() as u32;
            let name_bytes = name.as_bytes();
            let comp_len = compressed.len() as u32;
            let raw_len = raw.len() as u32;
            let name_len = name_bytes.len() as u16;

            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0x0800u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&comp_len.to_le_bytes());
            out.extend_from_slice(&raw_len.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(&compressed);

            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&0x0314u16.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&0x0800u16.to_le_bytes());
            cd.extend_from_slice(&method.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&crc.to_le_bytes());
            cd.extend_from_slice(&comp_len.to_le_bytes());
            cd.extend_from_slice(&raw_len.to_le_bytes());
            cd.extend_from_slice(&name_len.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&offset.to_le_bytes());
            cd.extend_from_slice(name_bytes);
        }

        let cd_offset = out.len() as u32;
        let cd_size = cd.len() as u32;
        let count = entries.len() as u16;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn zip_reader_handles_store_and_deflate_entries() {
        let zip = make_test_zip(&[
            ("a.txt", b"stored-payload", false),
            ("dir/b.txt", b"deflated-payload-repeating-repeating", true),
        ]);
        let decoded = read_zip_entries(&zip).expect("decode valid zip");
        assert_eq!(decoded.get("a.txt").unwrap(), b"stored-payload");
        assert_eq!(
            decoded.get("dir/b.txt").unwrap(),
            b"deflated-payload-repeating-repeating"
        );
        assert!(read_zip_entries(b"not-a-zip").is_err());
        assert!(read_zip_entries(&zip[..zip.len() - 10]).is_err());
    }

    #[test]
    fn render_html_parses_fluxwright_trace_and_escapes_script_tags() {
        let trace_ndjson = [
            r#"{"type":"context-options","title":"Checkout Smoke","browserName":"chromium","platform":"linux"}"#,
            r#"{"type":"before","callId":"c1","startTime":10.0,"class":"Page","method":"goto","apiName":"page.goto","params":{"url":"http://127.0.0.1/"},"pageId":"p1"}"#,
            r#"{"type":"after","callId":"c1","endTime":52.5}"#,
            r#"{"type":"screencast-frame","pageId":"p1","sha1":"f1.jpeg","width":1280,"height":720,"timestamp":25.0}"#,
            r#"{"type":"console","messageType":"log","text":"</script><img onerror=alert(1)>","pageId":"p1","time":30.0}"#,
            r#"{"type":"event","time":40.0,"class":"BrowserContext","method":"pageError","params":{"error":{"error":{"name":"Error","message":"boom"}}},"pageId":"p1"}"#,
        ]
        .join("\n");
        let net_ndjson = r#"{"type":"resource-snapshot","snapshot":{"time":18.4,"request":{"method":"GET","url":"http://127.0.0.1/"},"response":{"status":200,"statusText":"OK","content":{"size":512,"mimeType":"text/html"}}}}"#;

        let zip = make_test_zip(&[
            ("trace.trace", trace_ndjson.as_bytes(), false),
            ("trace.network", net_ndjson.as_bytes(), false),
            ("resources/f1.jpeg", &[0xff, 0xd8, 0xff, 0xe0], false),
        ]);
        let bundle = parse_trace_archive(&zip).expect("parse trace");
        assert_eq!(bundle.title, "Checkout Smoke");
        assert_eq!(bundle.actions.len(), 1);
        assert_eq!(bundle.actions[0].duration_ms, Some(42.5));
        assert_eq!(bundle.frames.len(), 1);
        assert_eq!(bundle.logs.len(), 2);
        assert_eq!(bundle.network.len(), 1);

        let html = render_html(&bundle).expect("render html");
        assert!(html.contains("data:image/jpeg;base64,"));
        assert!(!html.contains("</script><img"));
        assert!(html.contains("\\u003c/script\\u003e\\u003cimg"));
    }
}
