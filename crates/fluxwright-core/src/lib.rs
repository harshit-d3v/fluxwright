mod adaptive;
mod config;
mod error;
mod expect;
mod hosts;
mod metrics;
pub mod netguard;
mod pool;
mod process;
mod profiles;
mod proxies;
mod source;
mod tracing_rec;
mod video;
mod visual;

pub use adaptive::AdaptiveConfig;
pub use fluxwright_cdp::{
    filter_cookies, find_channel, managed_browsers_dir, normalize_headers, ApiRequestOptions,
    ApiResponse, AriaRef, AriaSnapshot, CdpEvent, CdpSession, CdpSessionEvent,
    ClockInstallOptions, ClockTicks, ClockTime, Connection, CookieFilter, Fulfill, Har, HarBrowser,
    HarContent, HarContentPolicy, HarCreator, HarEntry, HarHeader, HarLog, HarMode, HarNotFound,
    HarPostData, HarQueryParam, HarRecorder, HarRequest, HarResponse, HarRouteOptions, HarRouter,
    HarTimings, IgnoreDefaultArgs, IndexedDbDatabase, IndexedDbIndex, IndexedDbRecord,
    IndexedDbStore, MarkdownOptions, Overrides, RecordHarOptions, ResourceType, RouteFetchHandle,
    RouteFetchOptions, RouteOptions, ScreenshotAnimations, ScreenshotCaret, SnapshotOptions,
    WebSocketEvent, Worker, WorkerEvent,
};
pub use config::{
    EngineConfig, JobOptions, Priority, QueueFullMode, ResourcePreset, COOKIE_BANNER_URL_PATTERNS,
};
pub use error::{Error, Result};
pub use expect::{expect_locator, expect_page, LocatorExpect, PageExpect};
pub use metrics::MetricsSnapshot;
pub use pool::{
    with_timeout, ApiRequestContext, BrowserInfo, Clock, ContextLease, Downloads, Engine,
    EngineBuilder, Frame, FrameLocator, Locator, PageLease, Route, RouteRequest, UrlPattern,
};
pub use profiles::{validate_profile_name, ProfileInfo, ProfileStore};
pub use proxies::{parse_proxy_url, ProxyLease, ProxyPool, ProxyRotation, ProxyStatus};
pub use source::{BrowserSource, LocalChromium, RemoteCdp};
pub use tracing_rec::{Tracing, TracingChunkOptions, TracingStartOptions};
pub use video::{encode_mjpeg_avi, RecordVideoOptions, Video, VideoRecorder};
pub use visual::{
    compare_images, decode_png, encode_png, Animations, RgbaImage, ToHaveScreenshotOptions,
    VisualDiff,
};
