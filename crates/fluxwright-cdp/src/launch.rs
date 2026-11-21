#[cfg(windows)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
#[cfg(windows)]
use tokio::time::sleep;
use tracing::{info, warn};

use crate::error::{Error, IgnoreDefaultArgs, LaunchOptions, Result};

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn local_app_data_dir() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join("AppData/Local")))
}

/// Directory where Fluxwright stores managed Chromium binaries downloaded by `fluxwright install`.
/// Respects `FLUXWRIGHT_BROWSERS_PATH` when set and non-empty.
pub fn managed_browsers_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXWRIGHT_BROWSERS_PATH").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    default_managed_browsers_dir()
}

fn default_managed_browsers_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return local_app_data_dir().map(|d| d.join("fluxwright").join("browsers"));
    }
    if cfg!(target_os = "macos") {
        return home_dir().map(|h| h.join("Library/Caches/fluxwright/browsers"));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
        .map(|d| d.join("fluxwright").join("browsers"))
}

fn chrome_for_testing_rel() -> &'static Path {
    if cfg!(windows) {
        Path::new("chrome.exe")
    } else if cfg!(target_os = "macos") {
        Path::new("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
    } else {
        Path::new("chrome")
    }
}

fn from_env() -> Option<PathBuf> {
    ["FLUXWRIGHT_CHROMIUM", "CHROME", "CHROMIUM", "GOOGLE_CHROME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .find(|p| p.exists())
}

/// The browser a launch uses when none is given. FLUXWRIGHT_CHROMIUM / CHROME / CHROMIUM win.
/// Then, for headless launches, chrome-headless-shell if one is installed: Chrome's lighter
/// headless-only build, and Playwright's default. Pages were 5-10x faster to open with it
/// than with Chrome's new headless mode. Otherwise Chrome itself.
pub fn find_browser(headless: bool) -> Result<PathBuf> {
    if let Some(p) = from_env() {
        return Ok(p);
    }
    if headless {
        if let Some(p) = find_headless_shell() {
            return Ok(p);
        }
    }
    find_chrome(None)
}

/// chrome-headless-shell on explicit FLUXWRIGHT_BROWSERS_PATH, PATH, Fluxwright cache,
/// else newest in Puppeteer's or Playwright's cache.
fn find_headless_shell() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "chrome-headless-shell.exe"
    } else {
        "chrome-headless-shell"
    };
    if let Some(explicit_dir) = std::env::var_os("FLUXWRIGHT_BROWSERS_PATH").filter(|v| !v.is_empty()) {
        if let Some(p) = newest_shell(&PathBuf::from(explicit_dir).join("chrome-headless-shell"), "", exe) {
            return Some(p);
        }
    }
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(exe))
            .find(|p| p.is_file())
    });
    if on_path.is_some() {
        return on_path;
    }
    if let Some(managed) = default_managed_browsers_dir() {
        if let Some(p) = newest_shell(&managed.join("chrome-headless-shell"), "", exe) {
            return Some(p);
        }
    }
    let home = home_dir()?;
    let playwright = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                local_app_data_dir()
                    .unwrap_or_else(|| home.join("AppData/Local"))
                    .join("ms-playwright")
            } else if cfg!(target_os = "macos") {
                home.join("Library/Caches/ms-playwright")
            } else {
                home.join(".cache/ms-playwright")
            }
        });
    newest_shell(
        &home.join(".cache/puppeteer/chrome-headless-shell"),
        "",
        exe,
    )
    .or_else(|| newest_shell(&playwright, "chromium_headless_shell-", exe))
}

fn newest_bin(root: &Path, prefix: &str, rel: &Path) -> Option<PathBuf> {
    let mut found: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter_map(|dir| {
            let name = dir.file_name().to_string_lossy().into_owned();
            let version = name.strip_prefix(prefix)?;
            let key: Vec<u64> = version
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|n| n.parse().ok())
                .collect();
            let bin = std::fs::read_dir(dir.path())
                .ok()?
                .flatten()
                .map(|d| d.path().join(rel))
                .find(|p| p.is_file())?;
            Some((key, bin))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, bin)| bin)
}

/// Layout `<root>/<prefix><version>/chrome-headless-shell-<platform>/<exe>`; highest version
/// wins ("1243" in Playwright's cache, "win64-131.0.6778.85" in Puppeteer's).
fn newest_shell(root: &Path, prefix: &str, exe: &str) -> Option<PathBuf> {
    newest_bin(root, prefix, Path::new(exe))
}

fn newest_chrome_bin(root: &Path) -> Option<PathBuf> {
    newest_bin(root, "", chrome_for_testing_rel())
}

pub fn find_chrome(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = from_env() {
        return Ok(p);
    }
    if let Some(p) = managed_browsers_dir().and_then(|d| newest_chrome_bin(&d.join("chrome"))) {
        return Ok(p);
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

/// Resolves a named browser channel (`chrome`, `chrome-beta`, `chrome-dev`, `chrome-canary`,
/// `msedge`, `msedge-beta`, `msedge-dev`, `msedge-canary`, or `chromium`).
pub fn find_channel(channel: &str) -> Result<PathBuf> {
    let norm = channel.trim().to_ascii_lowercase();
    let local = local_app_data_dir();
    let mut paths: Vec<PathBuf> = Vec::new();
    match norm.as_str() {
        "chromium" => {
            for p in [
                "/usr/bin/chromium",
                "/usr/bin/chromium-browser",
                "/snap/bin/chromium",
                r"C:\Program Files\Chromium\Application\chrome.exe",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
            ] {
                let pb = PathBuf::from(p);
                if pb.exists() {
                    return Ok(pb);
                }
            }
            if let Some(p) = managed_browsers_dir().and_then(|d| newest_chrome_bin(&d.join("chrome"))) {
                return Ok(p);
            }
            return find_chrome(None).map_err(|_| {
                Error::Launch(format!("browser channel `{channel}` is not installed"))
            });
        }
        "chrome" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
                PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
                PathBuf::from("/usr/bin/google-chrome"),
                PathBuf::from("/usr/bin/google-chrome-stable"),
                PathBuf::from("/opt/google/chrome/chrome"),
                PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
        "chrome-beta" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files\Google\Chrome Beta\Application\chrome.exe"),
                PathBuf::from(r"C:\Program Files (x86)\Google\Chrome Beta\Application\chrome.exe"),
                PathBuf::from("/usr/bin/google-chrome-beta"),
                PathBuf::from("/opt/google/chrome-beta/chrome"),
                PathBuf::from("/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Google\Chrome Beta\Application\chrome.exe"));
            }
        }
        "chrome-dev" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files\Google\Chrome Dev\Application\chrome.exe"),
                PathBuf::from(r"C:\Program Files (x86)\Google\Chrome Dev\Application\chrome.exe"),
                PathBuf::from("/usr/bin/google-chrome-unstable"),
                PathBuf::from("/opt/google/chrome-unstable/chrome"),
                PathBuf::from("/Applications/Google Chrome Dev.app/Contents/MacOS/Google Chrome Dev"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Google\Chrome Dev\Application\chrome.exe"));
            }
        }
        "chrome-canary" => {
            paths.extend([
                PathBuf::from("/usr/bin/google-chrome-canary"),
                PathBuf::from("/opt/google/chrome-canary/chrome"),
                PathBuf::from("/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Google\Chrome SxS\Application\chrome.exe"));
            }
        }
        "msedge" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
                PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
                PathBuf::from("/usr/bin/microsoft-edge"),
                PathBuf::from("/usr/bin/microsoft-edge-stable"),
                PathBuf::from("/opt/microsoft/msedge/msedge"),
                PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Microsoft\Edge\Application\msedge.exe"));
            }
        }
        "msedge-beta" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge Beta\Application\msedge.exe"),
                PathBuf::from(r"C:\Program Files\Microsoft\Edge Beta\Application\msedge.exe"),
                PathBuf::from("/usr/bin/microsoft-edge-beta"),
                PathBuf::from("/opt/microsoft/msedge-beta/msedge"),
                PathBuf::from("/Applications/Microsoft Edge Beta.app/Contents/MacOS/Microsoft Edge Beta"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Microsoft\Edge Beta\Application\msedge.exe"));
            }
        }
        "msedge-dev" => {
            paths.extend([
                PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge Dev\Application\msedge.exe"),
                PathBuf::from(r"C:\Program Files\Microsoft\Edge Dev\Application\msedge.exe"),
                PathBuf::from("/usr/bin/microsoft-edge-dev"),
                PathBuf::from("/opt/microsoft/msedge-dev/msedge"),
                PathBuf::from("/Applications/Microsoft Edge Dev.app/Contents/MacOS/Microsoft Edge Dev"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Microsoft\Edge Dev\Application\msedge.exe"));
            }
        }
        "msedge-canary" => {
            paths.extend([
                PathBuf::from("/opt/microsoft/msedge-canary/msedge"),
                PathBuf::from("/Applications/Microsoft Edge Canary.app/Contents/MacOS/Microsoft Edge Canary"),
            ]);
            if let Some(l) = local {
                paths.push(l.join(r"Microsoft\Edge SxS\Application\msedge.exe"));
            }
        }
        _ => {
            return Err(Error::Launch(format!(
                "unsupported browser channel `{channel}`; expected chrome, chrome-beta, chrome-dev, chrome-canary, msedge, msedge-beta, msedge-dev, msedge-canary, or chromium"
            )));
        }
    }
    paths
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| Error::Launch(format!("browser channel `{channel}` is not installed")))
}

pub(crate) fn build_chrome_args(opts: &LaunchOptions, user_data_dir: &Path) -> Vec<String> {
    let mut default_args = vec![
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
        // Each launch gets a fresh profile, so without these every browser spends its first
        // minutes on component updates, field trials and model downloads. Playwright's set.
        "--disable-component-update".to_string(),
        "--disable-field-trial-config".to_string(),
        "--disable-extensions".to_string(),
        "--disable-component-extensions-with-background-pages".to_string(),
        "--disable-client-side-phishing-detection".to_string(),
        "--disable-backgrounding-occluded-windows".to_string(),
        "--disable-search-engine-choice-screen".to_string(),
        "--no-service-autorun".to_string(),
        "--disable-features=Translate,OptimizationHints,MediaRouter,DialMediaRouteProvider,GlobalMediaControls,LensOverlay,PaintHolding".to_string(),
        "--disable-dev-shm-usage".to_string(),
        format!(
            "--crash-dumps-dir={}",
            user_data_dir.join("Crashpad").display()
        ),
    ];
    if opts.headless {
        default_args.push("--headless=new".to_string());
        default_args.push("--hide-scrollbars".to_string());
        default_args.push("--mute-audio".to_string());
    }
    match &opts.ignore_default_args {
        IgnoreDefaultArgs::None => {}
        IgnoreDefaultArgs::All => default_args.clear(),
        IgnoreDefaultArgs::Some(ignored) => {
            default_args.retain(|a| {
                !ignored
                    .iter()
                    .any(|ig| a == ig || a.split_once('=').is_some_and(|(k, _)| k == ig))
            });
        }
    }

    let mut args = Vec::new();
    if !opts
        .extra_args
        .iter()
        .any(|a| a.starts_with("--user-data-dir="))
    {
        args.push(format!("--user-data-dir={}", user_data_dir.display()));
    }
    args.extend(default_args);
    if opts.no_sandbox {
        warn!("launching chromium with --no-sandbox (explicit opt-in)");
        if !opts.extra_args.iter().any(|a| a == "--no-sandbox") {
            args.push("--no-sandbox".to_string());
        }
        if !opts.extra_args.iter().any(|a| a == "--disable-setuid-sandbox") {
            args.push("--disable-setuid-sandbox".to_string());
        }
    }
    if cfg!(unix) {
        if !opts
            .extra_args
            .iter()
            .any(|a| a == "--remote-debugging-pipe")
        {
            args.push("--remote-debugging-pipe".to_string());
        }
    } else if !opts
        .extra_args
        .iter()
        .any(|a| a.starts_with("--remote-debugging-port"))
    {
        args.push("--remote-debugging-port=0".to_string());
        // No --remote-allow-origins: it lets any web page in any job open a DevTools socket to
        // another job's page. tungstenite sends no Origin header, which Chrome accepts.
    }
    args.extend(opts.extra_args.iter().cloned());
    args
}

pub struct Launched {
    pub child: Child,
    pub pid: u32,
    pub transport: Transport,
    pub user_data_dir: tempfile::TempDir,
}

/// How the engine talks to a browser it launched.
pub enum Transport {
    /// DevTools WebSocket on a localhost port (Windows; the job object ties Chrome to us).
    #[cfg(windows)]
    WebSocket(String),
    /// `--remote-debugging-pipe`: commands on Chrome's fd 3, replies on fd 4, NUL-separated.
    /// Private to this process (a debugging port is open to every local user), and Chrome
    /// exits when the pipe closes, so it cannot outlive a crashed engine. That is the only
    /// tether on macOS, which has no parent-death signal.
    #[cfg(unix)]
    Pipe {
        to_chrome: OwnedFd,
        from_chrome: OwnedFd,
    },
}

pub async fn launch_chrome(opts: &LaunchOptions) -> Result<Launched> {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    SWEEP.call_once(|| {
        std::thread::spawn(sweep_stale_profiles);
    });
    let exe = match (opts.executable.as_deref(), opts.channel.as_deref()) {
        (Some(p), _) => p.to_path_buf(),
        (None, Some(ch)) => find_channel(ch)?,
        (None, None) => find_browser(opts.headless)?,
    };
    // The pid in the name lets sweep_stale_profiles tell a dead engine's profile from a live one.
    let user_data_dir = tempfile::Builder::new()
        .prefix(&format!("fluxwright-{}-", std::process::id()))
        .tempdir()?;
    let args = build_chrome_args(opts, user_data_dir.path());

    info!(executable = %exe.display(), "launching chromium");
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Windows reads the DevTools URL from stderr; the pipe needs nothing from it.
        .stderr(if cfg!(unix) {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        // Our own launches must not fork between pipe() and FD_CLOEXEC (not atomic on
        // macOS): a sibling Chrome inheriting our write end would keep this pipe open.
        static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let (child, to_chrome, from_chrome) = {
            let _one_at_a_time = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            let (chrome_in, to_chrome) = cloexec_pipe()?;
            let (from_chrome, chrome_out) = cloexec_pipe()?;
            let (cin, cout) = (chrome_in.as_raw_fd(), chrome_out.as_raw_fd());
            #[cfg(target_os = "linux")]
            let engine = std::process::id() as libc::pid_t;
            // Async-signal-safe calls only: this runs between fork and exec.
            unsafe {
                cmd.pre_exec(move || {
                    // kill_on_drop only runs on a clean exit; the kernel covers a crash too.
                    // ponytail: the signal fires when the *thread* that spawned Chrome exits;
                    // tokio workers live as long as the runtime, so launch from async code.
                    #[cfg(target_os = "linux")]
                    {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        // The engine may have died between fork and prctl: then no signal
                        // comes, so do not start Chrome at all.
                        if libc::getppid() != engine {
                            return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                        }
                    }
                    // Move both ends above 4 first, so neither dup2 clobbers the other.
                    let a = libc::fcntl(cin, libc::F_DUPFD_CLOEXEC, 5);
                    let b = libc::fcntl(cout, libc::F_DUPFD_CLOEXEC, 5);
                    if a < 0 || b < 0 || libc::dup2(a, 3) < 0 || libc::dup2(b, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| Error::Launch(e.to_string()))?;
            // chrome_in / chrome_out drop here: only Chrome holds its ends now.
            (child, to_chrome, from_chrome)
        };
        let pid = child.id().ok_or_else(|| Error::Launch("no pid".into()))?;
        Ok(Launched {
            child,
            pid,
            transport: Transport::Pipe {
                to_chrome,
                from_chrome,
            },
            user_data_dir,
        })
    }

    #[cfg(windows)]
    {
        let mut child = cmd.spawn().map_err(|e| Error::Launch(e.to_string()))?;
        tie_to_this_process(&child);
        let pid = child.id().ok_or_else(|| Error::Launch("no pid".into()))?;
        let ws_url = match wait_devtools_url(
            &mut child,
            user_data_dir.path(),
            Duration::from_secs(20),
        )
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
            transport: Transport::WebSocket(ws_url),
            user_data_dir,
        })
    }
}

/// A pipe whose ends are both close-on-exec, so no child inherits them by accident.
#[cfg(unix)]
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok((read, write))
}

/// Puts Chrome in a job object that Windows kills when this process exits for any reason,
/// crash and hard kill included. Chrome's own children join the job, so the tree goes too.
#[cfg(windows)]
fn tie_to_this_process(child: &Child) {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::*;

    // One job per process, never closed by us: its only handle dies with the process. It is
    // not inheritable, so Chrome cannot keep it open.
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of_val(&info) as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return 0;
        }
        job as usize
    });
    let tied = match child.raw_handle() {
        Some(h) if job != 0 => unsafe { AssignProcessToJobObject(job as HANDLE, h as HANDLE) != 0 },
        _ => false,
    };
    if !tied {
        warn!("could not tie chromium to this process; it may outlive a crash");
    }
}

fn stale_profile_pid(name: &str) -> Option<u32> {
    name.strip_prefix("fluxwright-")?
        .split('-')
        .next()?
        .parse::<u32>()
        .ok()
}

/// Deletes profile dirs of engines that died without cleaning up (a crash or hard kill skips
/// TempDir's Drop). Only `fluxwright-<pid>-*` dirs whose pid is no longer running.
/// Runs once, in the background, on the first launch in a process.
pub fn sweep_stale_profiles() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, false);
    for e in entries.flatten() {
        let name = e.file_name();
        if let Some(pid) = name.to_str().and_then(stale_profile_pid) {
            if sys.process(sysinfo::Pid::from_u32(pid)).is_none() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

#[cfg(windows)]
fn ws_url_from_line(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("DevTools listening on ") {
        return Some(rest.trim().to_string());
    }
    line.find("ws://")
        .map(|i| {
            line[i..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|s| s.starts_with("ws://"))
}

#[cfg(windows)]
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

#[cfg(windows)]
fn push_tail(tail: &mut std::collections::VecDeque<String>, line: String) {
    if tail.len() == 16 {
        tail.pop_front();
    }
    tail.push_back(line);
}

#[cfg(windows)]
async fn wait_devtools_url(
    child: &mut Child,
    user_data_dir: &Path,
    timeout: Duration,
) -> Result<String> {
    let port_file = user_data_dir.join("DevToolsActivePort");
    enum StderrMsg {
        Url(String),
        Line(String),
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StderrMsg>();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = ws_url_from_line(&line) {
                    let _ = tx.send(StderrMsg::Url(url));
                } else if !line.trim().is_empty() {
                    let _ = tx.send(StderrMsg::Line(line));
                }
            }
        });
    }
    let mut stderr_tail = std::collections::VecDeque::with_capacity(16);
    let mut stderr_open = true;
    let start = tokio::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    StderrMsg::Url(url) => push_tail(&mut stderr_tail, url),
                    StderrMsg::Line(line) => push_tail(&mut stderr_tail, line),
                }
            }
            let tail = Vec::from(stderr_tail).join("\n");
            return Err(Error::Launch(if tail.is_empty() {
                format!("chrome exited immediately with {status}")
            } else {
                format!("chrome exited with {status}: {tail}")
            }));
        }
        if start.elapsed() > timeout {
            return Err(Error::Launch(
                "timed out waiting for DevToolsActivePort".into(),
            ));
        }
        tokio::select! {
            biased;
            msg = rx.recv(), if stderr_open => match msg {
                Some(StderrMsg::Url(url)) => return Ok(url),
                Some(StderrMsg::Line(line)) => push_tail(&mut stderr_tail, line),
                None => stderr_open = false,
            },
            _ = sleep(Duration::from_millis(30)) => {
                if let Some(url) = try_read_port_file(&port_file).await? {
                    return Ok(url);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_headless_shell_wins() {
        let root = tempfile::tempdir().unwrap();
        for version in [
            "chromium_headless_shell-1200",
            "chromium_headless_shell-1243",
            "chromium-1243",
        ] {
            let dir = root
                .path()
                .join(version)
                .join("chrome-headless-shell-linux64");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("shell"), "").unwrap();
        }
        let found = newest_shell(root.path(), "chromium_headless_shell-", "shell").unwrap();
        assert!(
            found.starts_with(root.path().join("chromium_headless_shell-1243")),
            "{found:?}"
        );

        let puppeteer = tempfile::tempdir().unwrap();
        for version in [
            "win64-131.0.6778.85",
            "win64-140.0.7339.80",
            "win64-99.0.1.1",
        ] {
            let dir = puppeteer
                .path()
                .join(version)
                .join("chrome-headless-shell-win64");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("shell"), "").unwrap();
        }
        let found = newest_shell(puppeteer.path(), "", "shell").unwrap();
        assert!(
            found.starts_with(puppeteer.path().join("win64-140.0.7339.80")),
            "{found:?}"
        );
    }

    #[test]
    fn stale_profile_pid_parses_fluxwright_prefix() {
        assert_eq!(stale_profile_pid("fluxwright-1234-ab"), Some(1234));
        assert_eq!(stale_profile_pid("fluxwright-42-dl-ctx"), Some(42));
        assert_eq!(stale_profile_pid("other-1234-ab"), None);
        assert_eq!(stale_profile_pid("fluxwright-bad-ab"), None);
    }

    #[test]
    fn newest_chrome_bin_picks_highest_version() {
        let root = tempfile::tempdir().unwrap();
        let rel = chrome_for_testing_rel();
        for version in ["130.0.6723.58", "134.0.6998.165", "132.0.6834.83"] {
            let bin = root
                .path()
                .join(version)
                .join("chrome-platform")
                .join(rel);
            std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
            std::fs::write(&bin, "").unwrap();
        }
        let found = newest_chrome_bin(root.path()).unwrap();
        assert!(
            found.starts_with(root.path().join("134.0.6998.165")),
            "{found:?}"
        );
    }

    #[test]
    fn find_channel_rejects_unknown_channel() {
        let err = find_channel("bogus-browser").unwrap_err();
        assert!(
            err.to_string().contains("unsupported browser channel"),
            "{err}"
        );
    }

    #[test]
    fn build_chrome_args_respects_ignore_default_args_and_extra_args() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = if cfg!(unix) {
            "--remote-debugging-pipe"
        } else {
            "--remote-debugging-port=0"
        };

        let def = build_chrome_args(&LaunchOptions::default(), tmp.path());
        assert!(def.iter().any(|a| a == "--mute-audio"));
        assert!(def.iter().any(|a| a.starts_with("--disable-features=")));
        assert!(def.iter().any(|a| a == transport));

        let all_ignored = build_chrome_args(
            &LaunchOptions {
                ignore_default_args: IgnoreDefaultArgs::All,
                extra_args: vec!["--custom-flag".into()],
                ..Default::default()
            },
            tmp.path(),
        );
        assert!(!all_ignored.iter().any(|a| a == "--mute-audio"));
        assert!(!all_ignored.iter().any(|a| a == "--no-first-run"));
        assert!(all_ignored.iter().any(|a| a == transport));
        assert!(all_ignored.iter().any(|a| a.starts_with("--user-data-dir=")));
        assert!(all_ignored.iter().any(|a| a == "--custom-flag"));

        let some_ignored = build_chrome_args(
            &LaunchOptions {
                ignore_default_args: IgnoreDefaultArgs::Some(vec![
                    "--mute-audio".into(),
                    "--disable-features".into(),
                ]),
                extra_args: vec!["--user-data-dir=/custom/profile".into()],
                ..Default::default()
            },
            tmp.path(),
        );
        assert!(!some_ignored.iter().any(|a| a == "--mute-audio"));
        assert!(!some_ignored.iter().any(|a| a.starts_with("--disable-features=")));
        assert!(some_ignored.iter().any(|a| a == "--no-first-run"));
        assert_eq!(
            some_ignored
                .iter()
                .filter(|a| a.starts_with("--user-data-dir="))
                .count(),
            1
        );
        assert!(some_ignored
            .iter()
            .any(|a| a == "--user-data-dir=/custom/profile"));
    }

    #[cfg(windows)]
    #[test]
    fn ws_url_from_line_parses_banner_and_inline_urls() {
        assert_eq!(
            ws_url_from_line("  DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc  "),
            Some("ws://127.0.0.1:9222/devtools/browser/abc".into())
        );
        assert_eq!(
            ws_url_from_line("[123] ready ws://127.0.0.1:5000/devtools/browser/xyz extra"),
            Some("ws://127.0.0.1:5000/devtools/browser/xyz".into())
        );
        assert_eq!(ws_url_from_line("nothing to see here"), None);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn try_read_port_file_parses_formats_and_incomplete_files() {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("DevToolsActivePort");
        assert_eq!(try_read_port_file(&port_file).await.unwrap(), None);

        tokio::fs::write(&port_file, "9222\n").await.unwrap();
        assert_eq!(try_read_port_file(&port_file).await.unwrap(), None);

        tokio::fs::write(&port_file, "9222\n/devtools/browser/id1\n")
            .await
            .unwrap();
        assert_eq!(
            try_read_port_file(&port_file).await.unwrap(),
            Some("ws://127.0.0.1:9222/devtools/browser/id1".into())
        );

        tokio::fs::write(&port_file, "9222\nws://127.0.0.1:9222/custom\n")
            .await
            .unwrap();
        assert_eq!(
            try_read_port_file(&port_file).await.unwrap(),
            Some("ws://127.0.0.1:9222/custom".into())
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn wait_devtools_url_fails_fast_when_child_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("cmd")
            .args(["/C", "echo mock crash 1>&2 & exit /B 7"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let err = wait_devtools_url(&mut child, dir.path(), Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stalled: {:?}",
            started.elapsed()
        );
        let msg = err.to_string();
        assert!(
            msg.contains("exited") && msg.contains("mock crash"),
            "{msg}"
        );
    }
}
