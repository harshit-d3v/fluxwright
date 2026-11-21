//! Fluxwright: lease-based Chromium fleet for high-concurrency automation.
//!
//! This crate does not claim to be faster or lighter than Playwright, Puppeteer,
//! or any other tool. See `docs/COMPETITIVE_ANALYSIS.md`.

pub use fluxwright_core::netguard;
pub use fluxwright_core::{
    with_timeout, ApiRequestContext, BrowserInfo, BrowserSource, ContextLease, Downloads,
    Engine as BrowserEngine, EngineBuilder, EngineConfig, Error, Frame, FrameLocator, JobOptions,
    LocalChromium, Locator, MetricsSnapshot, PageLease, Priority, QueueFullMode, RemoteCdp,
    ResourcePreset, Result,
};

pub use fluxwright_cdp::{
    download_to_file, fetch_url_bytes, filter_cookies, find_browser, find_channel, find_chrome,
    managed_browsers_dir, parse_modifiers, sweep_stale_profiles, url_glob_matches,
    ApiRequestOptions, ApiResponse, BindingCall, BoundingBox, CdpSession, CdpSessionEvent,
    CheckOptions, ClickOptions, Clip, ColorScheme, ConsoleMessage, Cookie, CookieFilter, Dialog,
    Download, Emulation, FileChooser, FileInput, FilePayload, FrameInfo, Fulfill, Geolocation, Har,
    HarBrowser, HarContent, HarContentPolicy, HarCreator, HarEntry, HarHeader, HarLog, HarMode,
    HarNotFound, HarPostData, HarQueryParam, HarRecorder, HarRequest, HarResponse, HarRouteOptions,
    HarRouter, HarTimings, HoverOptions, IgnoreDefaultArgs, ImageFormat, IndexedDbDatabase,
    IndexedDbIndex, IndexedDbRecord, IndexedDbStore, InterceptedRequest, LaunchOptions, LogEntry,
    MouseButton, NetEvent, NetRequest, NetResponse, OriginStorage, Overrides, PageError, PdfMargin,
    PdfOptions, Proxy, RecordHarOptions, ResourceType, RoleOptions, RouteFetchHandle,
    RouteFetchOptions, ScreenshotOptions, ScriptTagOptions, SelectEntry, Selector, StorageItem,
    StorageState, StyleTagOptions, WaitPolling, WaitState, WaitUntil, WebSocketEvent, Worker,
    WorkerEvent, FRAME_ID_PREFIX,
};
