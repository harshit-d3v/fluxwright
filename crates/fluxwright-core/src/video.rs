//! Per-page video recording via CDP `Page.startScreencast`, muxing into WebM (`ffmpeg`)
//! or falling back cleanly to a pure-Rust MJPEG-in-AVI (`RIFF....AVI `) container.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use fluxwright_cdp::CdpSession;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::{Error, Result};

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const SCREENCAP_SETTLE_DELAY: Duration = Duration::from_millis(25);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(60);

/// Valid 2x2 black baseline JPEG used only when a page closes before any frame can be captured.
const FALLBACK_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x06, 0x04, 0x05, 0x06, 0x05, 0x04, 0x06,
    0x06, 0x05, 0x06, 0x07, 0x07, 0x06, 0x08, 0x0a, 0x10, 0x0a, 0x0a, 0x09, 0x09, 0x0a, 0x14, 0x0e,
    0x0f, 0x0c, 0x10, 0x17, 0x14, 0x18, 0x18, 0x17, 0x14, 0x16, 0x16, 0x1a, 0x1d, 0x25, 0x1f, 0x1a,
    0x1b, 0x23, 0x1c, 0x16, 0x16, 0x20, 0x2c, 0x20, 0x23, 0x26, 0x27, 0x29, 0x2a, 0x29, 0x19, 0x1f,
    0x2d, 0x30, 0x2d, 0x28, 0x30, 0x25, 0x28, 0x29, 0x28, 0xff, 0xdb, 0x00, 0x43, 0x01, 0x07, 0x07,
    0x07, 0x0a, 0x08, 0x0a, 0x13, 0x0a, 0x0a, 0x13, 0x28, 0x1a, 0x16, 0x1a, 0x28, 0x28, 0x28, 0x28,
    0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28,
    0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28,
    0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0xff, 0xc0, 0x00, 0x11,
    0x08, 0x00, 0x02, 0x00, 0x02, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff,
    0xc4, 0x00, 0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
    0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04,
    0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41,
    0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1,
    0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19,
    0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44,
    0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64,
    0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84,
    0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2,
    0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9,
    0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7,
    0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3,
    0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xc4, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03,
    0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01,
    0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02,
    0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32,
    0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72,
    0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29,
    0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53,
    0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73,
    0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a,
    0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8,
    0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6,
    0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4,
    0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff,
    0xda, 0x00, 0x0c, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00, 0xf9, 0x52, 0x8a,
    0x28, 0xa0, 0x0f, 0xff, 0xd9,
];

/// Options for recording page videos in a browser context (`recordVideo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordVideoOptions {
    /// Directory where recorded video files will be saved.
    pub dir: PathBuf,
    /// Optional target maximum video dimensions `(width, height)`.
    pub size: Option<(u32, u32)>,
}

impl RecordVideoOptions {
    /// Creates video recording options saving into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            size: None,
        }
    }

    /// Sets the target maximum video dimensions `(width, height)`.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }

    pub fn validate(&self) -> Result<()> {
        if self.dir.as_os_str().is_empty() {
            return Err(Error::Other("record_video: dir must not be empty".into()));
        }
        if let Some((w, h)) = self.size {
            if w == 0 || h == 0 {
                return Err(Error::Other(
                    "record_video: size width and height must be positive".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Handle to a page's recorded video (`page.video()`).
#[derive(Clone)]
pub struct Video {
    state: watch::Receiver<Option<std::result::Result<PathBuf, String>>>,
}

impl Video {
    /// Waits for video recording to finish (on page/context close) and returns the saved file path.
    pub async fn path(&self) -> Result<PathBuf> {
        let mut rx = self.state.clone();
        loop {
            if let Some(res) = rx.borrow().as_ref() {
                return res.clone().map_err(Error::Other);
            }
            if rx.changed().await.is_err() {
                return Err(Error::Other("video recorder closed before finalizing".into()));
            }
        }
    }

    /// Waits for recording to finish and copies the video file to `dest`, creating parent folders.
    pub async fn save_as(&self, dest: impl AsRef<Path>) -> Result<()> {
        let src = self.path().await?;
        let dest = dest.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dest)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
        .map_err(|e| Error::Other(format!("video save_as: {e}")))
    }

    /// Waits for recording to finish and deletes the recorded video file if it exists.
    pub async fn delete(&self) -> Result<()> {
        let src = self.path().await?;
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&src) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
        .map_err(|e| Error::Other(format!("video delete: {e}")))
    }
}

#[derive(Debug, Clone)]
pub struct RecordedFrame {
    pub jpeg: Vec<u8>,
    pub timestamp: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

struct VideoRecorderInner {
    dir: PathBuf,
    size: Option<(u32, u32)>,
    id: Uuid,
    session: CdpSession,
    frames: Arc<Mutex<Vec<RecordedFrame>>>,
    collector: Mutex<Option<JoinHandle<()>>>,
    finish_lock: tokio::sync::Mutex<()>,
    done_tx: watch::Sender<Option<std::result::Result<PathBuf, String>>>,
    finished: AtomicBool,
}

/// Active CDP screencast recorder attached to a single page.
#[derive(Clone)]
pub struct VideoRecorder {
    inner: Arc<VideoRecorderInner>,
}

impl VideoRecorder {
    /// Starts capturing screencast JPEG frames over `session`.
    pub async fn start(session: CdpSession, opts: RecordVideoOptions) -> Result<(Self, Video)> {
        opts.validate()?;
        let dir = opts.dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir))
            .await
            .map_err(|e| Error::Other(e.to_string()))?
            .map_err(|e| Error::Other(format!("record_video create_dir_all: {e}")))?;

        let mut events = session.subscribe_events();
        let _ = session.send("Page.enable", json!({}), CDP_TIMEOUT).await;
        let mut params = json!({
            "format": "jpeg",
            "quality": 80,
            "everyNthFrame": 1,
        });
        if let Some((w, h)) = opts.size {
            params["maxWidth"] = json!(w);
            params["maxHeight"] = json!(h);
        }
        session
            .send("Page.startScreencast", params, CDP_TIMEOUT)
            .await?;

        let frames = Arc::new(Mutex::new(Vec::new()));
        let frames_c = Arc::clone(&frames);
        let session_c = session.clone();
        let collector = tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if ev.method != "Page.screencastFrame" {
                    continue;
                }
                if let Some(b64) = ev.params["data"].as_str() {
                    if let Ok(jpeg) = STANDARD.decode(b64) {
                        let meta = &ev.params["metadata"];
                        let timestamp = meta["timestamp"].as_f64();
                        let width = meta["deviceWidth"]
                            .as_f64()
                            .map(|v| v.round().max(1.0) as u32);
                        let height = meta["deviceHeight"]
                            .as_f64()
                            .map(|v| v.round().max(1.0) as u32);
                        frames_c.lock().unwrap().push(RecordedFrame {
                            jpeg,
                            timestamp,
                            width,
                            height,
                        });
                    }
                }
                if let Some(ack_id) = ev.params.get("sessionId").cloned() {
                    let _ = session_c
                        .send(
                            "Page.screencastFrameAck",
                            json!({ "sessionId": ack_id }),
                            Duration::from_secs(2),
                        )
                        .await;
                }
            }
        });

        let (done_tx, done_rx) = watch::channel(None);
        let recorder = Self {
            inner: Arc::new(VideoRecorderInner {
                dir: opts.dir,
                size: opts.size,
                id: Uuid::new_v4(),
                session,
                frames,
                collector: Mutex::new(Some(collector)),
                finish_lock: tokio::sync::Mutex::new(()),
                done_tx,
                finished: AtomicBool::new(false),
            }),
        };
        Ok((recorder, Video { state: done_rx }))
    }

    pub fn is_finished(&self) -> bool {
        self.inner.finished.load(Ordering::SeqCst)
    }

    /// Stops screencast capture, muxes recorded frames to `.webm` or `.avi`, and resolves `Video`.
    pub async fn finish(&self) -> Result<PathBuf> {
        let _guard = self.inner.finish_lock.lock().await;
        if let Some(res) = self.inner.done_tx.borrow().as_ref() {
            return res.clone().map_err(Error::Other);
        }

        let _ = self
            .inner
            .session
            .send("Page.stopScreencast", json!({}), Duration::from_secs(2))
            .await;
        tokio::time::sleep(SCREENCAP_SETTLE_DELAY).await;

        let handle = self.inner.collector.lock().unwrap().take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }

        let mut frames = std::mem::take(&mut *self.inner.frames.lock().unwrap());
        if frames.is_empty() && !self.inner.session.is_detached() {
            if let Ok(val) = self
                .inner
                .session
                .send(
                    "Page.captureScreenshot",
                    json!({ "format": "jpeg", "quality": 80 }),
                    Duration::from_secs(3),
                )
                .await
            {
                if let Some(b64) = val["data"].as_str() {
                    if let Ok(jpeg) = STANDARD.decode(b64) {
                        frames.push(RecordedFrame {
                            jpeg,
                            timestamp: None,
                            width: self.inner.size.map(|s| s.0),
                            height: self.inner.size.map(|s| s.1),
                        });
                    }
                }
            }
        }
        let _ = self.inner.session.detach(Duration::from_secs(3)).await;

        let dir = self.inner.dir.clone();
        let id = self.inner.id;
        let size = self.inner.size;
        let write_res = tokio::task::spawn_blocking(move || mux_frames(&dir, id, size, frames))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.map_err(|e| e.to_string()));

        let _ = self.inner.done_tx.send(Some(write_res.clone()));
        self.inner.finished.store(true, Ordering::SeqCst);
        write_res.map_err(Error::Other)
    }
}

fn compute_fps(frames: &[RecordedFrame]) -> u32 {
    if frames.len() < 2 {
        return 10;
    }
    let first = frames.iter().filter_map(|f| f.timestamp).next();
    let last = frames.iter().rev().filter_map(|f| f.timestamp).next();
    match (first, last) {
        (Some(t0), Some(t1)) if t1 > t0 => {
            let duration = (t1 - t0).max(0.1);
            ((frames.len() as f64) / duration).round().clamp(1.0, 30.0) as u32
        }
        _ => 10,
    }
}

/// Parses width and height from a JPEG frame's SOF0/SOF1/SOF2 marker if present.
pub fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < jpeg.len() && jpeg[i] == 0xFF {
            i += 1;
        }
        if i >= jpeg.len() {
            break;
        }
        let marker = jpeg[i];
        i += 1;
        // Standalone markers without length field (RSTn, SOI, EOI, TEM)
        if marker == 0x00 || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            continue;
        }
        if i + 1 >= jpeg.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([jpeg[i], jpeg[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > jpeg.len() {
            break;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) && seg_len >= 7 {
            let h = u16::from_be_bytes([jpeg[i + 3], jpeg[i + 4]]) as u32;
            let w = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        i += seg_len;
    }
    None
}

fn resolve_avi_dims(first_dims: Option<(u32, u32)>, size: Option<(u32, u32)>) -> (u32, u32) {
    first_dims.or(size).unwrap_or((800, 600))
}

fn has_webm_magic(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && magic == [0x1a, 0x45, 0xdf, 0xa3]
}

fn wait_child_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn mux_frames(
    dir: &Path,
    id: Uuid,
    size: Option<(u32, u32)>,
    mut frames: Vec<RecordedFrame>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    if frames.is_empty() {
        frames.push(RecordedFrame {
            jpeg: FALLBACK_JPEG.to_vec(),
            timestamp: None,
            width: Some(2),
            height: Some(2),
        });
    }
    let fps = compute_fps(&frames);
    let first_dims = frames
        .first()
        .and_then(|f| jpeg_dimensions(&f.jpeg).or_else(|| f.width.zip(f.height)));
    let (width, height) = resolve_avi_dims(first_dims, size);
    let frames = Arc::new(frames);

    let webm_path = dir.join(format!("{id}.webm"));
    if try_mux_webm(&webm_path, &frames, fps, size, first_dims) {
        return Ok(webm_path);
    }
    let _ = std::fs::remove_file(&webm_path);

    let avi_path = dir.join(format!("{id}.avi"));
    let avi_bytes = encode_mjpeg_avi(
        &frames
            .iter()
            .map(|f| f.jpeg.as_slice())
            .collect::<Vec<_>>(),
        fps,
        width,
        height,
    );
    std::fs::write(&avi_path, avi_bytes)?;
    Ok(avi_path)
}

fn try_mux_webm(
    out_path: &Path,
    frames: &Arc<Vec<RecordedFrame>>,
    fps: u32,
    size: Option<(u32, u32)>,
    first_dims: Option<(u32, u32)>,
) -> bool {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-loglevel",
        "error",
        "-f",
        "image2pipe",
        "-vcodec",
        "mjpeg",
        "-framerate",
        &fps.to_string(),
        "-i",
        "-",
    ]);

    let has_mixed_dims = first_dims.is_some_and(|(w0, h0)| {
        frames
            .iter()
            .filter_map(|f| jpeg_dimensions(&f.jpeg))
            .any(|(w, h)| (w, h) != (w0, h0))
    });
    if let Some((w, h)) = size.or(if has_mixed_dims { first_dims } else { None }) {
        let vf = format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
        );
        cmd.args(["-vf", &vf]);
    }
    cmd.args(["-c:v", "libvpx", "-b:v", "1M", "-pix_fmt", "yuv420p", "-an"]);
    cmd.arg(out_path);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    let writer = child.stdin.take().map(|mut stdin| {
        let frames_for_writer = Arc::clone(frames);
        std::thread::spawn(move || {
            for frame in frames_for_writer.iter() {
                if stdin.write_all(&frame.jpeg).is_err() {
                    break;
                }
            }
            let _ = stdin.flush();
        })
    });
    let status = wait_child_with_timeout(&mut child, FFMPEG_TIMEOUT);
    if let Some(handle) = writer {
        let _ = handle.join();
    }
    let Some(status) = status else {
        return false;
    };
    if !status.success() {
        return false;
    }
    has_webm_magic(out_path)
}

/// Encodes raw JPEG frames into a valid RIFF MJPEG-in-AVI (`RIFF....AVI `) container.
pub fn encode_mjpeg_avi(frames: &[&[u8]], fps: u32, width: u32, height: u32) -> Vec<u8> {
    let fps = fps.max(1);
    let width = width.max(1);
    let height = height.max(1);
    let num_frames = frames.len() as u32;
    let max_frame_size = frames.iter().map(|f| f.len() as u32).max().unwrap_or(0);
    let microsec_per_frame = 1_000_000u32 / fps;

    // Build `avih` (56 bytes payload)
    let mut avih = Vec::with_capacity(56);
    avih.extend_from_slice(&microsec_per_frame.to_le_bytes()); // dwMicroSecPerFrame
    avih.extend_from_slice(&(max_frame_size.saturating_mul(fps)).to_le_bytes()); // dwMaxBytesPerSec
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwPaddingGranularity
    avih.extend_from_slice(&0x10u32.to_le_bytes()); // dwFlags (AVIF_HASINDEX)
    avih.extend_from_slice(&num_frames.to_le_bytes()); // dwTotalFrames
    avih.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    avih.extend_from_slice(&1u32.to_le_bytes()); // dwStreams
    avih.extend_from_slice(&max_frame_size.to_le_bytes()); // dwSuggestedBufferSize
    avih.extend_from_slice(&width.to_le_bytes()); // dwWidth
    avih.extend_from_slice(&height.to_le_bytes()); // dwHeight
    avih.extend_from_slice(&[0u8; 16]); // dwReserved[4]

    // Build `strh` (56 bytes payload)
    let mut strh = Vec::with_capacity(56);
    strh.extend_from_slice(b"vids"); // fccType
    strh.extend_from_slice(b"MJPG"); // fccHandler
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    strh.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    strh.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    strh.extend_from_slice(&1u32.to_le_bytes()); // dwScale
    strh.extend_from_slice(&fps.to_le_bytes()); // dwRate
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    strh.extend_from_slice(&num_frames.to_le_bytes()); // dwLength
    strh.extend_from_slice(&max_frame_size.to_le_bytes()); // dwSuggestedBufferSize
    strh.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // dwQuality
    strh.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.left
    strh.extend_from_slice(&0i16.to_le_bytes()); // rcFrame.top
    strh.extend_from_slice(&(width.min(i16::MAX as u32) as i16).to_le_bytes()); // rcFrame.right
    strh.extend_from_slice(&(height.min(i16::MAX as u32) as i16).to_le_bytes()); // rcFrame.bottom

    // Build `strf` BITMAPINFOHEADER (40 bytes payload)
    let mut strf = Vec::with_capacity(40);
    strf.extend_from_slice(&40u32.to_le_bytes()); // biSize
    strf.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    strf.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight
    strf.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    strf.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    strf.extend_from_slice(b"MJPG"); // biCompression
    strf.extend_from_slice(&(width.saturating_mul(height).saturating_mul(3)).to_le_bytes()); // biSizeImage
    strf.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    strf.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    strf.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // LIST 'strl'
    let strl_payload_len = 4 + (8 + strh.len()) + (8 + strf.len());
    // LIST 'hdrl'
    let hdrl_payload_len = 4 + (8 + avih.len()) + (8 + strl_payload_len);

    // Compute `movi` payload and `idx1`
    let mut movi_data_len = 4usize; // "movi"
    let mut offsets = Vec::with_capacity(frames.len());
    for frame in frames {
        offsets.push((movi_data_len as u32, frame.len() as u32));
        movi_data_len += 8 + frame.len() + (frame.len() & 1);
    }
    let idx1_len = frames.len() * 16;
    let riff_payload_len = 4 + (8 + hdrl_payload_len) + (8 + movi_data_len) + (8 + idx1_len);

    let mut out = Vec::with_capacity(8 + riff_payload_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_payload_len as u32).to_le_bytes());
    out.extend_from_slice(b"AVI ");

    // hdrl
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(hdrl_payload_len as u32).to_le_bytes());
    out.extend_from_slice(b"hdrl");

    out.extend_from_slice(b"avih");
    out.extend_from_slice(&(avih.len() as u32).to_le_bytes());
    out.extend_from_slice(&avih);

    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(strl_payload_len as u32).to_le_bytes());
    out.extend_from_slice(b"strl");

    out.extend_from_slice(b"strh");
    out.extend_from_slice(&(strh.len() as u32).to_le_bytes());
    out.extend_from_slice(&strh);

    out.extend_from_slice(b"strf");
    out.extend_from_slice(&(strf.len() as u32).to_le_bytes());
    out.extend_from_slice(&strf);

    // movi
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(movi_data_len as u32).to_le_bytes());
    out.extend_from_slice(b"movi");
    for frame in frames {
        out.extend_from_slice(b"00dc");
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        out.extend_from_slice(frame);
        if (frame.len() & 1) == 1 {
            out.push(0);
        }
    }

    // idx1
    out.extend_from_slice(b"idx1");
    out.extend_from_slice(&(idx1_len as u32).to_le_bytes());
    for (offset, size) in offsets {
        out.extend_from_slice(b"00dc");
        out.extend_from_slice(&0x10u32.to_le_bytes()); // AVIIF_KEYFRAME
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jpeg_dimensions_and_writes_valid_avi_header() {
        assert_eq!(jpeg_dimensions(FALLBACK_JPEG), Some((2, 2)));
        let avi = encode_mjpeg_avi(&[FALLBACK_JPEG, FALLBACK_JPEG], 10, 2, 2);
        assert_eq!(&avi[0..4], b"RIFF");
        assert_eq!(&avi[8..12], b"AVI ");
        let declared_size = u32::from_le_bytes(avi[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared_size + 8, avi.len());
        assert!(avi.windows(4).any(|w| w == b"hdrl"));
        assert!(avi.windows(4).any(|w| w == b"MJPG"));
        assert!(avi.windows(4).any(|w| w == b"movi"));
        assert!(avi.windows(4).any(|w| w == b"00dc"));
        assert!(avi.windows(4).any(|w| w == b"idx1"));
    }

    #[test]
    fn validates_record_video_options() {
        assert!(RecordVideoOptions::new("").validate().is_err());
        assert!(RecordVideoOptions::new("videos").size(0, 480).validate().is_err());
        assert!(RecordVideoOptions::new("videos").size(640, 480).validate().is_ok());
    }

    #[test]
    fn avi_dims_prefer_first_frame_and_webm_magic_reads_four_bytes() {
        assert_eq!(resolve_avi_dims(Some((320, 180)), Some((640, 480))), (320, 180));
        assert_eq!(resolve_avi_dims(None, Some((640, 480))), (640, 480));
        assert_eq!(resolve_avi_dims(None, None), (800, 600));

        let dir = std::env::temp_dir().join(format!("fw-webm-magic-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let valid = dir.join("ok.webm");
        let short = dir.join("short.webm");
        let wrong = dir.join("wrong.webm");
        std::fs::write(&valid, [0x1a, 0x45, 0xdf, 0xa3, 0x00, 0x01]).unwrap();
        std::fs::write(&short, [0x1a, 0x45, 0xdf]).unwrap();
        std::fs::write(&wrong, b"RIFF0000AVI ").unwrap();

        assert!(has_webm_magic(&valid));
        assert!(!has_webm_magic(&short));
        assert!(!has_webm_magic(&wrong));
        assert!(!has_webm_magic(&dir.join("missing.webm")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
