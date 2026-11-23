//! Pixel-by-pixel RGBA PNG comparison and visual regression artifact helpers (`toHaveScreenshot`).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use fluxwright_cdp::{
    validate_mask_color, Clip, ScreenshotCaret, FREEZE_ANIMATIONS_CSS, HIDE_CARET_CSS,
};

use crate::error::{Error, Result};

/// Whether CSS animations and transitions are paused during `toHaveScreenshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Animations {
    /// Inject CSS pausing animations, transitions, and caret blinking (default).
    #[default]
    Disabled,
    /// Allow animations and transitions to run normally.
    Allow,
}

/// Configuration for `expect_page` / `expect_locator` `.to_have_screenshot_with(...)`.
#[derive(Debug, Clone)]
pub struct ToHaveScreenshotOptions {
    /// Per-pixel YIQ perceptual color difference threshold in `0.0..=1.0` (default: `0.2`).
    pub threshold: f64,
    /// Maximum number of differing pixels allowed (`None` requires 0 unless ratio is set).
    pub max_diff_pixels: Option<u64>,
    /// Maximum fraction of differing pixels allowed in `0.0..=1.0`.
    pub max_diff_pixel_ratio: Option<f64>,
    /// Whether CSS animations are disabled during capture (default: [`Animations::Disabled`]).
    pub animations: Animations,
    /// Create or overwrite the golden baseline file (`None` defers to
    /// `FLUXWRIGHT_UPDATE_SNAPSHOTS`, `Some(true)` forces update, `Some(false)` disables update
    /// even when the environment variable is set).
    pub update: Option<bool>,
    /// Capture full scrollable page (page screenshots only).
    pub full_page: bool,
    /// Omit default white background (transparent PNG).
    pub omit_background: bool,
    /// Optional clip rectangle (page screenshots only).
    pub clip: Option<Clip>,
    /// Selectors for elements to cover with solid colored mask boxes.
    pub mask: Vec<String>,
    /// CSS color for mask overlays (default `#FF00FF`).
    pub mask_color: Option<String>,
    /// Text caret mode during capture (`Hide` or `Initial`).
    pub caret: Option<ScreenshotCaret>,
}

impl Default for ToHaveScreenshotOptions {
    fn default() -> Self {
        Self {
            threshold: 0.2,
            max_diff_pixels: None,
            max_diff_pixel_ratio: None,
            animations: Animations::Disabled,
            update: None,
            full_page: false,
            omit_background: false,
            clip: None,
            mask: Vec::new(),
            mask_color: None,
            caret: None,
        }
    }
}

impl ToHaveScreenshotOptions {
    pub fn threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    pub fn max_diff_pixels(mut self, max_diff_pixels: u64) -> Self {
        self.max_diff_pixels = Some(max_diff_pixels);
        self
    }

    pub fn max_diff_pixel_ratio(mut self, ratio: f64) -> Self {
        self.max_diff_pixel_ratio = Some(ratio);
        self
    }

    pub fn animations(mut self, animations: Animations) -> Self {
        self.animations = animations;
        self
    }

    pub fn update(mut self, update: bool) -> Self {
        self.update = Some(update);
        self
    }

    pub fn full_page(mut self, full_page: bool) -> Self {
        self.full_page = full_page;
        self
    }

    pub fn omit_background(mut self, omit_background: bool) -> Self {
        self.omit_background = omit_background;
        self
    }

    pub fn clip(mut self, clip: Clip) -> Self {
        self.clip = Some(clip);
        self
    }

    pub fn mask<I, S>(mut self, selectors: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.mask = selectors.into_iter().map(Into::into).collect();
        self
    }

    pub fn mask_color(mut self, color: impl Into<String>) -> Self {
        self.mask_color = Some(color.into());
        self
    }

    pub fn caret(mut self, caret: ScreenshotCaret) -> Self {
        self.caret = Some(caret);
        self
    }

    pub fn validate(&self) -> Result<()> {
        if !self.threshold.is_finite() || !(0.0..=1.0).contains(&self.threshold) {
            return Err(Error::Other(format!(
                "to_have_screenshot: threshold must be in 0.0..=1.0, got {}",
                self.threshold
            )));
        }
        if let Some(ratio) = self.max_diff_pixel_ratio {
            if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                return Err(Error::Other(format!(
                    "to_have_screenshot: max_diff_pixel_ratio must be in 0.0..=1.0, got {ratio}"
                )));
            }
        }
        for sel in &self.mask {
            if sel.trim().is_empty() {
                return Err(Error::Other(
                    "to_have_screenshot: mask selector must not be empty".into(),
                ));
            }
        }
        if let Some(ref color) = self.mask_color {
            validate_mask_color(color)?;
        }
        Ok(())
    }
}

/// Decoded normalized 8-bit RGBA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Outcome of comparing two PNG screenshots.
#[derive(Debug, Clone)]
pub struct VisualDiff {
    pub expected_size: (u32, u32),
    pub actual_size: (u32, u32),
    pub total_pixels: u64,
    pub diff_pixels: u64,
    pub diff_ratio: f64,
    pub diff_png: Option<Vec<u8>>,
}

impl VisualDiff {
    pub fn is_within_tolerance(&self, opts: &ToHaveScreenshotOptions) -> bool {
        if self.expected_size != self.actual_size {
            return false;
        }
        match (opts.max_diff_pixels, opts.max_diff_pixel_ratio) {
            (None, None) => self.diff_pixels == 0,
            (Some(max_px), None) => self.diff_pixels <= max_px,
            (None, Some(max_ratio)) => self.diff_ratio <= max_ratio,
            (Some(max_px), Some(max_ratio)) => {
                self.diff_pixels <= max_px && self.diff_ratio <= max_ratio
            }
        }
    }
}

fn animation_freeze_css(hide_caret: bool) -> String {
    if hide_caret {
        format!("{FREEZE_ANIMATIONS_CSS} {HIDE_CARET_CSS}")
    } else {
        FREEZE_ANIMATIONS_CSS.to_string()
    }
}

pub(crate) fn disable_animations_page_js(hide_caret: bool) -> String {
    let css = serde_json::to_string(&animation_freeze_css(hide_caret)).unwrap();
    format!(
        r#"(() => {{
    if (!document.getElementById('__fw_disable_animations')) {{
        const style = document.createElement('style');
        style.id = '__fw_disable_animations';
        style.textContent = {css};
        (document.head || document.documentElement).appendChild(style);
    }}
}})()"#
    )
}

pub(crate) const RESTORE_ANIMATIONS_PAGE_JS: &str = r#"(() => {
    const el = document.getElementById('__fw_disable_animations');
    if (el) el.remove();
})()"#;

pub(crate) fn disable_animations_locator_js(hide_caret: bool) -> String {
    let css = serde_json::to_string(&animation_freeze_css(hide_caret)).unwrap();
    format!(
        r#"(el) => {{
    const doc = (el && el.ownerDocument) || document;
    if (!doc.getElementById('__fw_disable_animations')) {{
        const style = doc.createElement('style');
        style.id = '__fw_disable_animations';
        style.textContent = {css};
        (doc.head || doc.documentElement).appendChild(style);
    }}
}}"#
    )
}

pub(crate) const RESTORE_ANIMATIONS_LOCATOR_JS: &str = r#"(el) => {
    const doc = (el && el.ownerDocument) || document;
    const s = doc.getElementById('__fw_disable_animations');
    if (s) s.remove();
}"#;

pub fn should_update_snapshots(explicit_update: Option<bool>) -> bool {
    match explicit_update {
        Some(v) => v,
        None => std::env::var("FLUXWRIGHT_UPDATE_SNAPSHOTS")
            .ok()
            .is_some_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "all"
                )
            }),
    }
}

/// Returns `(baseline_path, actual_path, diff_path)` where artifacts share `<stem>-actual.png` / `<stem>-diff.png`.
/// Rejects empty paths, missing filenames, and parent-directory (`..`) traversal components.
pub fn artifact_paths(baseline: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let raw = baseline.to_string_lossy();
    if raw.is_empty() {
        return Err(Error::Other(
            "to_have_screenshot: snapshot path must not be empty".into(),
        ));
    }
    if raw.ends_with('/') || raw.ends_with('\\') {
        return Err(Error::Other(format!(
            "to_have_screenshot: snapshot path must include a filename: {}",
            baseline.display()
        )));
    }
    if baseline
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error::Other(format!(
            "to_have_screenshot: snapshot path must not contain '..': {}",
            baseline.display()
        )));
    }
    let Some(stem) = baseline
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    else {
        return Err(Error::Other(format!(
            "to_have_screenshot: snapshot path must include a valid filename: {}",
            baseline.display()
        )));
    };
    let parent = baseline.parent();
    let join = |suffix: &str| match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(format!("{stem}{suffix}")),
        _ => PathBuf::from(format!("{stem}{suffix}")),
    };
    Ok((
        baseline.to_path_buf(),
        join("-actual.png"),
        join("-diff.png"),
    ))
}

/// Decodes any valid PNG into normalized 8-bit RGBA pixels.
pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND
            | png::Transformations::STRIP_16
            | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Other(format!("decode PNG header: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::Other(format!("decode PNG frame: {e}")))?;
    let raw = &buf[..info.buffer_size()];
    let pixel_count = (info.width as usize).saturating_mul(info.height as usize);
    let data = match info.color_type {
        png::ColorType::Rgba => raw.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for c in raw.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for c in raw.chunks_exact(2) {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for &g in raw {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::Indexed => {
            return Err(Error::Other(
                "unexpected indexed PNG after EXPAND transformation".into(),
            ));
        }
    };
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        data,
    })
}

/// Encodes an RGBA8 image into PNG bytes.
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, img.width, img.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::Other(format!("encode PNG header: {e}")))?;
        writer
            .write_image_data(&img.data)
            .map_err(|e| Error::Other(format!("encode PNG data: {e}")))?;
    }
    Ok(out)
}

#[inline]
fn blend_channel(c: u8, alpha: u8, bg: f64) -> f64 {
    let a = (alpha as f64) / 255.0;
    bg + ((c as f64) - bg) * a
}

#[inline]
fn rgb_yiq_delta_sq(dr: f64, dg: f64, db: f64) -> f64 {
    if dr == 0.0 && dg == 0.0 && db == 0.0 {
        return 0.0;
    }
    let y = dr * 0.29889531 + dg * 0.58662247 + db * 0.11448223;
    let i = dr * 0.59597799 - dg * 0.27417610 - db * 0.32180189;
    let q = dr * 0.21147017 - dg * 0.52261711 + db * 0.31114694;
    0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q
}

#[inline]
fn yiq_delta_sq(p1: &[u8], p2: &[u8]) -> f64 {
    if p1 == p2 {
        return 0.0;
    }
    let (a1, a2) = (p1[3], p2[3]);
    if a1 == 255 && a2 == 255 {
        return rgb_yiq_delta_sq(
            (p1[0] as f64) - (p2[0] as f64),
            (p1[1] as f64) - (p2[1] as f64),
            (p1[2] as f64) - (p2[2] as f64),
        );
    }
    // Dual-background blending (black + white) guarantees alpha differences can never cancel
    // against a single fixed backdrop (e.g. opaque #C0C0C0 vs fully transparent).
    let on_black = rgb_yiq_delta_sq(
        blend_channel(p1[0], a1, 0.0) - blend_channel(p2[0], a2, 0.0),
        blend_channel(p1[1], a1, 0.0) - blend_channel(p2[1], a2, 0.0),
        blend_channel(p1[2], a1, 0.0) - blend_channel(p2[2], a2, 0.0),
    );
    let on_white = rgb_yiq_delta_sq(
        blend_channel(p1[0], a1, 255.0) - blend_channel(p2[0], a2, 255.0),
        blend_channel(p1[1], a1, 255.0) - blend_channel(p2[1], a2, 255.0),
        blend_channel(p1[2], a1, 255.0) - blend_channel(p2[2], a2, 255.0),
    );
    on_black.max(on_white)
}

/// Compares `expected_img` and `actual_img` pixel-by-pixel using YIQ perceptual distance.
pub fn compare_images(
    expected: &RgbaImage,
    actual: &RgbaImage,
    threshold: f64,
    render_diff: bool,
) -> Result<VisualDiff> {
    let max_delta = 35_215.0 * threshold * threshold;
    let out_w = expected.width.max(actual.width);
    let out_h = expected.height.max(actual.height);
    let total_pixels = (out_w as u64).saturating_mul(out_h as u64);

    if expected.width == actual.width && expected.height == actual.height && expected.data == actual.data {
        return Ok(VisualDiff {
            expected_size: (expected.width, expected.height),
            actual_size: (actual.width, actual.height),
            total_pixels,
            diff_pixels: 0,
            diff_ratio: 0.0,
            diff_png: None,
        });
    }

    let mut diff_pixels = 0u64;
    let mut diff_rgba = if render_diff {
        vec![0u8; (out_w as usize) * (out_h as usize) * 4]
    } else {
        Vec::new()
    };

    for y in 0..out_h {
        let in_exp_y = y < expected.height;
        let in_act_y = y < actual.height;
        for x in 0..out_w {
            let in_both = in_exp_y && in_act_y && x < expected.width && x < actual.width;
            let out_idx = ((y as usize) * (out_w as usize) + (x as usize)) * 4;
            if !in_both {
                diff_pixels += 1;
                if render_diff {
                    diff_rgba[out_idx..out_idx + 4].copy_from_slice(&[255, 0, 255, 255]);
                }
                continue;
            }
            let e_idx = ((y as usize) * (expected.width as usize) + (x as usize)) * 4;
            let a_idx = ((y as usize) * (actual.width as usize) + (x as usize)) * 4;
            let ep = &expected.data[e_idx..e_idx + 4];
            let ap = &actual.data[a_idx..a_idx + 4];
            let delta = yiq_delta_sq(ep, ap);
            if delta > max_delta {
                diff_pixels += 1;
                if render_diff {
                    diff_rgba[out_idx..out_idx + 4].copy_from_slice(&[255, 0, 255, 255]);
                }
            } else if render_diff {
                let gray =
                    0.299 * (ap[0] as f64) + 0.587 * (ap[1] as f64) + 0.114 * (ap[2] as f64);
                let faded = (255.0 + (gray - 255.0) * 0.25).round().clamp(0.0, 255.0) as u8;
                diff_rgba[out_idx..out_idx + 4].copy_from_slice(&[faded, faded, faded, 255]);
            }
        }
    }

    let diff_ratio = if total_pixels == 0 {
        0.0
    } else {
        (diff_pixels as f64) / (total_pixels as f64)
    };
    let diff_png = if render_diff && (diff_pixels > 0 || expected.width != actual.width || expected.height != actual.height) {
        Some(encode_png(&RgbaImage {
            width: out_w,
            height: out_h,
            data: diff_rgba,
        })?)
    } else {
        None
    };

    Ok(VisualDiff {
        expected_size: (expected.width, expected.height),
        actual_size: (actual.width, actual.height),
        total_pixels,
        diff_pixels,
        diff_ratio,
        diff_png,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            data.extend_from_slice(&rgba);
        }
        encode_png(&RgbaImage { width: w, height: h, data }).unwrap()
    }

    #[test]
    fn exact_threshold_and_magenta_diff_work() {
        let red = decode_png(&solid_png(4, 4, [255, 0, 0, 255])).unwrap();
        let near_red = decode_png(&solid_png(4, 4, [225, 0, 0, 255])).unwrap();
        let blue = decode_png(&solid_png(4, 4, [0, 0, 255, 255])).unwrap();

        let exact = compare_images(&red, &red, 0.2, true).unwrap();
        assert_eq!(exact.diff_pixels, 0);
        assert!(exact.diff_png.is_none());

        // Subtle shift passes at default threshold 0.2, fails at strict 0.0
        let subtle_pass = compare_images(&red, &near_red, 0.2, false).unwrap();
        assert_eq!(subtle_pass.diff_pixels, 0);
        let subtle_fail = compare_images(&red, &near_red, 0.0, true).unwrap();
        assert_eq!(subtle_fail.diff_pixels, 16);

        // Red vs blue fails with magenta diff PNG
        let mismatch = compare_images(&red, &blue, 0.2, true).unwrap();
        assert_eq!(mismatch.diff_pixels, 16);
        assert!((mismatch.diff_ratio - 1.0).abs() < 1e-9);
        let diff_decoded = decode_png(mismatch.diff_png.as_ref().unwrap()).unwrap();
        assert_eq!(&diff_decoded.data[0..4], &[255, 0, 255, 255]);

        // Dimension mismatch fails even with large max_diff_pixels
        let wider = decode_png(&solid_png(6, 4, [255, 0, 0, 255])).unwrap();
        let dim_diff = compare_images(&red, &wider, 0.2, true).unwrap();
        assert_eq!(dim_diff.diff_pixels, 8);
        assert!(!dim_diff.is_within_tolerance(&ToHaveScreenshotOptions::default().max_diff_pixels(100)));
    }

    #[test]
    fn artifact_paths_use_stem() {
        let (base, actual, diff) = artifact_paths(Path::new("shots/hero.banner.png")).unwrap();
        assert_eq!(base, Path::new("shots/hero.banner.png"));
        assert_eq!(actual, Path::new("shots/hero.banner-actual.png"));
        assert_eq!(diff, Path::new("shots/hero.banner-diff.png"));
    }

    #[test]
    fn artifact_paths_rejects_empty_and_parent_dir_traversal() {
        assert!(artifact_paths(Path::new("")).is_err());
        assert!(artifact_paths(Path::new(".")).is_err());
        assert!(artifact_paths(Path::new("..")).is_err());
        assert!(artifact_paths(Path::new("../escape.png")).is_err());
        assert!(artifact_paths(Path::new("shots/../../escape.png")).is_err());
        assert!(artifact_paths(Path::new("shots/")).is_err());
        assert!(artifact_paths(Path::new("/")).is_err());
    }

    #[test]
    fn yiq_delta_sq_detects_opaque_gray_vs_transparent() {
        let opaque_gray = [192, 192, 192, 255];
        let transparent = [0, 0, 0, 0];
        assert!(yiq_delta_sq(&opaque_gray, &transparent) > 0.0);

        let img_gray = decode_png(&solid_png(2, 2, opaque_gray)).unwrap();
        let img_clear = decode_png(&solid_png(2, 2, transparent)).unwrap();
        let strict = compare_images(&img_gray, &img_clear, 0.0, false).unwrap();
        assert_eq!(strict.diff_pixels, 4);
        let default_thresh = compare_images(&img_gray, &img_clear, 0.2, false).unwrap();
        assert_eq!(default_thresh.diff_pixels, 4);

        // Two fully transparent pixels with different RGB channels remain visually identical.
        assert_eq!(yiq_delta_sq(&[255, 0, 0, 0], &[0, 0, 255, 0]), 0.0);
    }

    #[test]
    fn should_update_snapshots_explicit_false_overrides_env() {
        let prev = std::env::var_os("FLUXWRIGHT_UPDATE_SNAPSHOTS");
        std::env::set_var("FLUXWRIGHT_UPDATE_SNAPSHOTS", "1");
        let explicit_false = should_update_snapshots(Some(false));
        let explicit_true = should_update_snapshots(Some(true));
        let env_enabled = should_update_snapshots(None);
        std::env::remove_var("FLUXWRIGHT_UPDATE_SNAPSHOTS");
        let env_disabled = should_update_snapshots(None);
        let explicit_true_without_env = should_update_snapshots(Some(true));
        match prev {
            Some(v) => std::env::set_var("FLUXWRIGHT_UPDATE_SNAPSHOTS", v),
            None => std::env::remove_var("FLUXWRIGHT_UPDATE_SNAPSHOTS"),
        }
        assert!(!explicit_false);
        assert!(explicit_true);
        assert!(env_enabled);
        assert!(!env_disabled);
        assert!(explicit_true_without_env);
    }
}
