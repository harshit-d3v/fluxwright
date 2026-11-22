//! Fluxwright: lease-based Chromium fleet for high-concurrency automation.
//!
//! This crate does not claim to be faster or lighter than Playwright, Puppeteer,
//! or any other tool. See `docs/COMPETITIVE_ANALYSIS.md`.

pub use fluxwright_core::netguard;
pub use fluxwright_core::{
    expect_locator, expect_page, parse_proxy_url, validate_profile_name, with_timeout,
    AdaptiveConfig, ApiRequestContext, BrowserInfo, BrowserSource, ContextLease, Downloads,
    Engine as BrowserEngine, EngineBuilder, EngineConfig, Error, Frame, FrameLocator, JobOptions,
    LocalChromium, Locator, LocatorExpect, MetricsSnapshot, PageExpect, PageLease, Priority,
    ProfileInfo, ProfileStore, ProxyLease, ProxyPool, ProxyRotation, ProxyStatus, QueueFullMode,
    RemoteCdp, ResourcePreset, Result, Tracing, TracingChunkOptions, TracingStartOptions,
};

pub use fluxwright_cdp::{
    download_to_file, fetch_url_bytes, filter_cookies, find_browser, find_channel, find_chrome,
    managed_browsers_dir, parse_modifiers, sweep_stale_profiles, url_glob_matches,
    ApiRequestOptions, ApiResponse, AriaRef, AriaSnapshot, BindingCall, BoundingBox, CdpEvent,
    CdpSession, CdpSessionEvent, CheckOptions, ClickOptions, Clip, ColorScheme, Connection,
    ConsoleMessage, Cookie, CookieFilter, Dialog, Download, Emulation, FileChooser, FileInput,
    FilePayload, FrameInfo, Fulfill, Geolocation, Har, HarBrowser, HarContent, HarContentPolicy,
    HarCreator, HarEntry, HarHeader, HarLog, HarMode, HarNotFound, HarPostData, HarQueryParam,
    HarRecorder, HarRequest, HarResponse, HarRouteOptions, HarRouter, HarTimings, HoverOptions,
    IgnoreDefaultArgs, ImageFormat, IndexedDbDatabase, IndexedDbIndex, IndexedDbRecord,
    IndexedDbStore, InterceptedRequest, LaunchOptions, LogEntry, MarkdownOptions, MouseButton,
    NetEvent, NetRequest, NetResponse, OriginStorage, Overrides, PageError, PdfMargin, PdfOptions,
    Proxy, RecordHarOptions, ResourceType, RoleOptions, RouteFetchHandle, RouteFetchOptions,
    ScreenshotOptions, ScriptTagOptions, SelectEntry, Selector, SnapshotOptions, StorageItem,
    StorageState, StyleTagOptions, WaitPolling, WaitState, WaitUntil, WebSocketEvent, Worker,
    WorkerEvent, FRAME_ID_PREFIX,
};
