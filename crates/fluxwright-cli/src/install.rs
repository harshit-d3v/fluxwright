use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

const CFT_LAST_KNOWN_GOOD_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
const CFT_KNOWN_GOOD_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/known-good-versions-with-downloads.json";

pub struct InstallArgs {
    pub browser: String,
    pub channel: String,
    pub version: Option<String>,
    pub force: bool,
    pub dry_run: bool,
    pub path: Option<PathBuf>,
}

#[derive(Serialize)]
struct InstallRecord {
    browser: String,
    channel: String,
    version: String,
    platform: String,
    url: String,
    install_dir: String,
    executable: String,
    already_installed: bool,
    dry_run: bool,
}

struct TempCleanup {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
}

impl TempCleanup {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            dirs: Vec::new(),
        }
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
        for d in &self.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

pub fn normalize_targets(browser: &str) -> Result<Vec<&'static str>> {
    match browser.trim().to_ascii_lowercase().as_str() {
        "chrome-headless-shell" | "headless-shell" => Ok(vec!["chrome-headless-shell"]),
        "chrome" | "chromium" => Ok(vec!["chrome"]),
        "all" => Ok(vec!["chrome-headless-shell", "chrome"]),
        other => bail!(
            "unsupported browser `{other}`; expected `chrome-headless-shell`, `chrome`, or `all`"
        ),
    }
}

pub fn cft_platform(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("linux64"),
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("windows", "x86_64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        _ => bail!(
            "Chrome for Testing downloads are not published for {os}/{arch}; set FLUXWRIGHT_CHROMIUM to a system Chromium binary"
        ),
    }
}

pub fn sanitize_version(version: &str) -> Result<&str> {
    let v = version.trim();
    if v.is_empty()
        || v.contains("..")
        || !v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("invalid Chrome for Testing version `{version}`");
    }
    Ok(v)
}

pub fn expected_exe_rel(target: &str, platform: &str) -> PathBuf {
    let folder = format!("{target}-{platform}");
    let exe = match target {
        "chrome-headless-shell" => {
            if platform.starts_with("win") {
                Path::new("chrome-headless-shell.exe")
            } else {
                Path::new("chrome-headless-shell")
            }
        }
        _ => {
            if platform.starts_with("win") {
                Path::new("chrome.exe")
            } else if platform.starts_with("mac") {
                Path::new("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
            } else {
                Path::new("chrome")
            }
        }
    };
    Path::new(&folder).join(exe)
}

pub fn resolve_cft_download(
    manifest_json: &[u8],
    target: &str,
    channel: &str,
    pinned_version: Option<&str>,
    platform: &str,
) -> Result<(String, String)> {
    let root: Value =
        serde_json::from_slice(manifest_json).context("invalid Chrome for Testing JSON")?;

    let entry = if let Some(wanted) = pinned_version {
        let wanted = sanitize_version(wanted)?;
        root["versions"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .rev()
                    .find(|v| v["version"].as_str() == Some(wanted))
            })
            .with_context(|| {
                format!("version `{wanted}` not found in Chrome for Testing manifest")
            })?
    } else {
        let channels = root["channels"]
            .as_object()
            .context("manifest missing `channels` map")?;
        channels
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(channel.trim()))
            .map(|(_, v)| v)
            .with_context(|| {
                format!("channel `{channel}` not found (expected stable, beta, dev, or canary)")
            })?
    };

    let version = sanitize_version(
        entry["version"]
            .as_str()
            .context("manifest entry missing `version`")?,
    )?
    .to_string();

    let downloads = entry["downloads"][target]
        .as_array()
        .with_context(|| format!("manifest missing downloads for `{target}` {version}"))?;
    let url = downloads
        .iter()
        .find(|d| d["platform"].as_str() == Some(platform))
        .and_then(|d| d["url"].as_str())
        .with_context(|| format!("no `{target}` download for platform `{platform}`"))?
        .to_string();

    if !url.starts_with("https://") {
        bail!("refusing non-HTTPS Chrome download URL `{url}`");
    }
    Ok((version, url))
}

fn try_run(cmd: &mut std::process::Command) -> bool {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn extract_zip(zip_path: &Path, stage_dir: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let mut tar = std::process::Command::new("tar.exe");
        tar.arg("-xf").arg(zip_path).arg("-C").arg(stage_dir);
        if try_run(&mut tar) {
            return Ok(());
        }
        let mut ps = std::process::Command::new("powershell.exe");
        ps.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Expand-Archive -LiteralPath $env:FLUXWRIGHT_ZIP_SRC -DestinationPath $env:FLUXWRIGHT_ZIP_DST -Force",
        ])
        .env("FLUXWRIGHT_ZIP_SRC", zip_path)
        .env("FLUXWRIGHT_ZIP_DST", stage_dir);
        if try_run(&mut ps) {
            return Ok(());
        }
        bail!("failed to extract archive with `tar.exe` or PowerShell `Expand-Archive`")
    }
    #[cfg(not(windows))]
    {
        let mut unzip = std::process::Command::new("unzip");
        unzip.arg("-q").arg("-o").arg(zip_path).arg("-d").arg(stage_dir);
        if try_run(&mut unzip) {
            return Ok(());
        }
        let mut py = std::process::Command::new("python3");
        py.args(["-m", "zipfile", "-e"])
            .arg(zip_path)
            .arg(stage_dir);
        if try_run(&mut py) {
            return Ok(());
        }
        let mut tar = std::process::Command::new("tar");
        tar.arg("-xf").arg(zip_path).arg("-C").arg(stage_dir);
        if try_run(&mut tar) {
            return Ok(());
        }
        bail!("failed to extract archive (tried `unzip`, `python3 -m zipfile`, and `tar`)")
    }
}

#[cfg(unix)]
fn ensure_executable_permissions(exe_path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for path in std::iter::once(exe_path.to_path_buf()).chain(
        exe_path
            .parent()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some("chrome" | "chrome-headless-shell" | "chrome_crashpad_handler" | "chrome-sandbox")
                )
            }),
    ) {
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            let mode = perms.mode();
            if mode & 0o111 == 0 {
                perms.set_mode(mode | 0o755);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
    }
}

pub async fn run(args: InstallArgs) -> Result<()> {
    let targets = normalize_targets(&args.browser)?;
    let platform = cft_platform(std::env::consts::OS, std::env::consts::ARCH)?;
    let base_dir = match args.path {
        Some(p) => p,
        None => fluxwright::managed_browsers_dir()
            .context("unable to determine managed browser directory; pass --path or set FLUXWRIGHT_BROWSERS_PATH")?,
    };

    let manifest_url = if args.version.is_some() {
        CFT_KNOWN_GOOD_URL
    } else {
        CFT_LAST_KNOWN_GOOD_URL
    };
    let manifest_bytes = fluxwright::fetch_url_bytes(manifest_url, Duration::from_secs(60))
        .await
        .with_context(|| format!("failed to fetch Chrome for Testing manifest from {manifest_url}"))?;

    let mut records = Vec::with_capacity(targets.len());
    let pid = std::process::id();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for (idx, target) in targets.into_iter().enumerate() {
        let (version, url) = resolve_cft_download(
            &manifest_bytes,
            target,
            &args.channel,
            args.version.as_deref(),
            platform,
        )?;
        let target_root = base_dir.join(target);
        let install_dir = target_root.join(&version);
        let exe_rel = expected_exe_rel(target, platform);
        let exe_path = install_dir.join(&exe_rel);

        if args.dry_run {
            records.push(InstallRecord {
                browser: target.to_string(),
                channel: args.channel.clone(),
                version,
                platform: platform.to_string(),
                url,
                install_dir: install_dir.display().to_string(),
                executable: exe_path.display().to_string(),
                already_installed: exe_path.is_file(),
                dry_run: true,
            });
            continue;
        }

        if exe_path.is_file() && !args.force {
            records.push(InstallRecord {
                browser: target.to_string(),
                channel: args.channel.clone(),
                version,
                platform: platform.to_string(),
                url,
                install_dir: install_dir.display().to_string(),
                executable: exe_path.display().to_string(),
                already_installed: true,
                dry_run: false,
            });
            continue;
        }

        std::fs::create_dir_all(&target_root)
            .with_context(|| format!("creating {}", target_root.display()))?;

        let mut cleanup = TempCleanup::new();
        let zip_path = target_root.join(format!(".tmp-{pid}-{stamp}-{idx}.zip"));
        let stage_dir = target_root.join(format!(".stage-{pid}-{stamp}-{idx}"));
        cleanup.files.push(zip_path.clone());
        cleanup.dirs.push(stage_dir.clone());
        std::fs::create_dir_all(&stage_dir)?;

        eprintln!("Downloading {target} {version} ({platform})...");
        fluxwright::download_to_file(&url, &zip_path, Duration::from_secs(600))
            .await
            .with_context(|| format!("downloading {url}"))?;

        eprintln!("Extracting {}...", zip_path.display());
        extract_zip(&zip_path, &stage_dir)?;

        let staged_exe = stage_dir.join(&exe_rel);
        if !staged_exe.is_file() {
            bail!(
                "extracted archive missing expected executable `{}`",
                staged_exe.display()
            );
        }
        let stage_canon = stage_dir.canonicalize()?;
        let exe_canon = staged_exe.canonicalize()?;
        if !exe_canon.starts_with(&stage_canon) {
            bail!("extracted executable escapes staging directory");
        }

        #[cfg(unix)]
        ensure_executable_permissions(&staged_exe);

        if install_dir.exists() {
            std::fs::remove_dir_all(&install_dir)
                .with_context(|| format!("replacing {}", install_dir.display()))?;
        }
        std::fs::rename(&stage_dir, &install_dir).with_context(|| {
            format!(
                "moving {} -> {}",
                stage_dir.display(),
                install_dir.display()
            )
        })?;
        drop(cleanup);

        records.push(InstallRecord {
            browser: target.to_string(),
            channel: args.channel.clone(),
            version,
            platform: platform.to_string(),
            url,
            install_dir: install_dir.display().to_string(),
            executable: exe_path.display().to_string(),
            already_installed: false,
            dry_run: false,
        });
    }

    if records.len() == 1 {
        println!("{}", serde_json::to_string_pretty(&records[0])?);
    } else {
        println!("{}", serde_json::to_string_pretty(&records)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_platforms_and_validates_targets_and_versions() {
        assert_eq!(cft_platform("linux", "x86_64").unwrap(), "linux64");
        assert_eq!(cft_platform("macos", "aarch64").unwrap(), "mac-arm64");
        assert_eq!(cft_platform("macos", "x86_64").unwrap(), "mac-x64");
        assert_eq!(cft_platform("windows", "x86_64").unwrap(), "win64");
        assert_eq!(cft_platform("windows", "x86").unwrap(), "win32");
        assert!(cft_platform("linux", "aarch64").is_err());

        assert_eq!(
            normalize_targets("chrome-headless-shell").unwrap(),
            vec!["chrome-headless-shell"]
        );
        assert_eq!(normalize_targets("chromium").unwrap(), vec!["chrome"]);
        assert_eq!(
            normalize_targets("all").unwrap(),
            vec!["chrome-headless-shell", "chrome"]
        );
        assert!(normalize_targets("firefox").is_err());

        assert_eq!(sanitize_version("131.0.6778.85").unwrap(), "131.0.6778.85");
        assert!(sanitize_version("../evil").is_err());
        assert!(sanitize_version("131/0").is_err());
        assert!(sanitize_version("   ").is_err());
    }

    #[test]
    fn expected_relative_executable_matches_cache_discovery_layout() {
        assert_eq!(
            expected_exe_rel("chrome-headless-shell", "linux64"),
            PathBuf::from("chrome-headless-shell-linux64/chrome-headless-shell")
        );
        assert_eq!(
            expected_exe_rel("chrome-headless-shell", "win64"),
            PathBuf::from("chrome-headless-shell-win64/chrome-headless-shell.exe")
        );
        assert_eq!(
            expected_exe_rel("chrome", "mac-arm64"),
            PathBuf::from(
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
            )
        );
        assert_eq!(
            expected_exe_rel("chrome", "win64"),
            PathBuf::from("chrome-win64/chrome.exe")
        );
    }

    #[test]
    fn resolves_cft_channel_and_pinned_version_downloads() {
        let channels_json = br#"{
          "channels": {
            "Stable": {
              "channel": "Stable",
              "version": "131.0.6778.85",
              "downloads": {
                "chrome-headless-shell": [
                  { "platform": "linux64", "url": "https://storage.googleapis.com/cft/linux64/shell.zip" },
                  { "platform": "win64", "url": "https://storage.googleapis.com/cft/win64/shell.zip" }
                ]
              }
            },
            "Beta": {
              "channel": "Beta",
              "version": "132.0.0.1",
              "downloads": {
                "chrome": [
                  { "platform": "linux64", "url": "https://storage.googleapis.com/cft/linux64/chrome-beta.zip" }
                ]
              }
            }
          }
        }"#;

        let (v, u) = resolve_cft_download(
            channels_json,
            "chrome-headless-shell",
            "stable",
            None,
            "linux64",
        )
        .unwrap();
        assert_eq!(v, "131.0.6778.85");
        assert!(u.ends_with("/linux64/shell.zip"));

        let (bv, bu) =
            resolve_cft_download(channels_json, "chrome", "BETA", None, "linux64").unwrap();
        assert_eq!(bv, "132.0.0.1");
        assert!(bu.ends_with("/chrome-beta.zip"));

        assert!(resolve_cft_download(
            channels_json,
            "chrome-headless-shell",
            "stable",
            None,
            "mac-arm64"
        )
        .is_err());

        let pinned_json = br#"{
          "versions": [
            {
              "version": "130.0.0.5",
              "downloads": {
                "chrome-headless-shell": [
                  { "platform": "linux64", "url": "https://storage.googleapis.com/cft/130/shell.zip" }
                ]
              }
            },
            {
              "version": "130.0.0.6",
              "downloads": {
                "chrome-headless-shell": [
                  { "platform": "linux64", "url": "http://insecure.example/shell.zip" }
                ]
              }
            }
          ]
        }"#;
        let (pv, pu) = resolve_cft_download(
            pinned_json,
            "chrome-headless-shell",
            "stable",
            Some("130.0.0.5"),
            "linux64",
        )
        .unwrap();
        assert_eq!(pv, "130.0.0.5");
        assert!(pu.starts_with("https://"));

        // Non-HTTPS URLs and unknown versions are rejected cleanly.
        assert!(resolve_cft_download(
            pinned_json,
            "chrome-headless-shell",
            "stable",
            Some("130.0.0.6"),
            "linux64"
        )
        .is_err());
        assert!(resolve_cft_download(
            pinned_json,
            "chrome-headless-shell",
            "stable",
            Some("999.0.0.0"),
            "linux64"
        )
        .is_err());
    }
}
