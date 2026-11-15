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
    find_browser, find_chrome, sweep_stale_profiles, BoundingBox, Clip, ColorScheme,
    ConsoleMessage, Cookie, Dialog, Download, Emulation, Fulfill, Geolocation, ImageFormat,
    InterceptedRequest, LaunchOptions, LogEntry, NetEvent, NetRequest, NetResponse, OriginStorage,
    Overrides, PageError, PdfMargin, PdfOptions, Proxy, ResourceType, ScreenshotOptions, Selector,
    StorageItem, StorageState, WaitState, WaitUntil,
};
