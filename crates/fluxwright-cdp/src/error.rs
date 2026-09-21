use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("chrome executable not found; set FLUXWRIGHT_CHROMIUM, CHROME, or CHROMIUM")]
    ChromeNotFound,
    #[error("failed to launch chrome: {0}")]
    Launch(String),
    #[error("devtools websocket: {0}")]
    WebSocket(String),
    #[error("cdp command {method} failed: {message}")]
    Command { method: String, message: String },
    #[error("timed out waiting for {what} after {timeout_ms}ms")]
    Timeout { what: String, timeout_ms: u64 },
    #[error("browser process exited (pid {pid})")]
    BrowserDead { pid: u32 },
    #[error("target crashed: {status}")]
    TargetCrashed { status: String },
    #[error("session {0} is gone")]
    SessionGone(String),
    #[error("no main frame")]
    NoMainFrame,
    #[error("element {selector} not actionable: {reason}")]
    NotActionable { selector: String, reason: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::BrowserDead { .. }
                | Error::TargetCrashed { .. }
                | Error::WebSocket(_)
                | Error::SessionGone(_)
        )
    }

    pub fn timeout(what: impl Into<String>, timeout_ms: u64) -> Self {
        Error::Timeout {
            what: what.into(),
            timeout_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub executable: Option<PathBuf>,
    pub headless: bool,
    pub no_sandbox: bool,
    pub extra_args: Vec<String>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            headless: true,
            no_sandbox: false,
            extra_args: Vec::new(),
        }
    }
}
