//! Fluxwright: lease-based Chromium fleet for high-concurrency automation.
//!
//! This crate does not claim to be faster or lighter than Playwright, Puppeteer,
//! or any other tool. See `docs/COMPETITIVE_ANALYSIS.md`.

pub use fluxwright_core::netguard;
pub use fluxwright_core::{
    with_timeout, BrowserInfo, BrowserSource, Downloads, Engine as BrowserEngine, EngineBuilder, EngineConfig,
    Error, FrameLocator, JobOptions, LocalChromium, Locator, MetricsSnapshot, PageLease, Priority,
    QueueFullMode, RemoteCdp, Result,
};

pub use fluxwright_cdp::{
    find_browser, find_chrome, parse_modifiers, sweep_stale_profiles, url_glob_matches,
    BoundingBox, CheckOptions, ClickOptions, Clip, ColorScheme, ConsoleMessage, Cookie, Dialog,
    Download, Emulation, FileChooser, FileInput, FilePayload, Fulfill, Geolocation, HoverOptions,
    ImageFormat, InterceptedRequest, LaunchOptions, LogEntry, MouseButton, NetEvent, NetRequest,
    NetResponse, OriginStorage, Overrides, PageError, PdfMargin, PdfOptions, Proxy, ResourceType,
    RoleOptions, ScreenshotOptions, SelectEntry, Selector, StorageItem, StorageState, WaitPolling,
    WaitState, WaitUntil,
};
