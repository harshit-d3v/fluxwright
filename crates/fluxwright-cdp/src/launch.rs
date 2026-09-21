use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::error::{Error, LaunchOptions, Result};

pub fn find_chrome(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    for key in ["FLUXWRIGHT_CHROMIUM", "CHROME", "CHROMIUM", "GOOGLE_CHROME"] {
        if let Ok(v) = std::env::var(key) {
            let p = PathBuf::from(v);
            if p.exists() {
                return Ok(p);
            }
        }
    }
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Chromium\Application\chrome.exe",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(Error::ChromeNotFound)
}

pub struct Launched {
    pub child: Child,
    pub pid: u32,
    pub ws_url: String,
    pub user_data_dir: tempfile::TempDir,
}

pub async fn launch_chrome(opts: &LaunchOptions) -> Result<Launched> {
    let exe = find_chrome(opts.executable.as_deref())?;
    let user_data_dir = tempfile::TempDir::new()?;
    let mut args = vec![
        format!("--user-data-dir={}", user_data_dir.path().display()),
        "--remote-debugging-port=0".to_string(),
        "--remote-allow-origins=*".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-popup-blocking".to_string(),
        "--disable-sync".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-renderer-backgrounding".to_string(),
        "--disable-hang-monitor".to_string(),
        "--disable-ipc-flooding-protection".to_string(),
        "--metrics-recording-only".to_string(),
        "--password-store=basic".to_string(),
        "--use-mock-keychain".to_string(),
        "--window-size=1280,720".to_string(),
        "--disable-crash-reporter".to_string(),
        "--disable-breakpad".to_string(),
        "--disable-dev-shm-usage".to_string(),
        format!(
            "--crash-dumps-dir={}",
            user_data_dir.path().join("Crashpad").display()
        ),
    ];
    if opts.headless {
        args.push("--headless=new".to_string());
        args.push("--hide-scrollbars".to_string());
        args.push("--mute-audio".to_string());
    }
    if opts.no_sandbox {
        warn!("launching chromium with --no-sandbox (explicit opt-in)");
        args.push("--no-sandbox".to_string());
        args.push("--disable-setuid-sandbox".to_string());
    }
    args.extend(opts.extra_args.iter().cloned());

    info!(executable = %exe.display(), "launching chromium");
    let mut child = Command::new(&exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Error::Launch(e.to_string()))?;

    let pid = child.id().ok_or_else(|| Error::Launch("no pid".into()))?;

    let ws_url = match wait_devtools_url(&mut child, user_data_dir.path(), Duration::from_secs(20))
        .await
    {
        Ok(u) => u,
        Err(e) => {
            let _ = child.kill().await;
            return Err(e);
        }
    };

    Ok(Launched {
        child,
        pid,
        ws_url,
        user_data_dir,
    })
}

fn ws_url_from_line(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("DevTools listening on ") {
        return Some(rest.trim().to_string());
    }
    line.find("ws://")
        .map(|i| line[i..].split_whitespace().next().unwrap_or("").to_string())
        .filter(|s| s.starts_with("ws://"))
}

async fn try_read_port_file(port_file: &Path) -> Result<Option<String>> {
    let text = match tokio::fs::read_to_string(port_file).await {
        Ok(t) => t,
        Err(e)
            if e.kind() == ErrorKind::NotFound
                || e.kind() == ErrorKind::PermissionDenied
                || e.raw_os_error() == Some(32) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mut lines = text.lines();
    let Some(port) = lines.next() else {
        return Ok(None);
    };
    let Some(path) = lines.next() else {
        return Ok(None);
    };
    let path = path.trim();
    if path.is_empty() {
        return Ok(None);
    }
    let url = if path.starts_with("ws://") {
        path.to_string()
    } else {
        format!("ws://127.0.0.1:{port}{path}")
    };
    Ok(Some(url))
}

async fn wait_devtools_url(
    child: &mut Child,
    user_data_dir: &Path,
    timeout: Duration,
) -> Result<String> {
    let port_file = user_data_dir.join("DevToolsActivePort");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = ws_url_from_line(&line) {
                    let _ = tx.send(url);
                }
            }
        });
    }
    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(Error::Launch(
                "timed out waiting for DevToolsActivePort".into(),
            ));
        }
        tokio::select! {
            biased;
            Some(url) = rx.recv() => return Ok(url),
            _ = sleep(Duration::from_millis(30)) => {
                if let Some(url) = try_read_port_file(&port_file).await? {
                    return Ok(url);
                }
            }
        }
    }
}
