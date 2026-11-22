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

pub use adaptive::AdaptiveConfig;
pub use fluxwright_cdp::{
    filter_cookies, find_channel, managed_browsers_dir, ApiRequestOptions, ApiResponse, AriaRef,
    AriaSnapshot, CdpEvent, CdpSession, CdpSessionEvent, Connection, CookieFilter, Har,
    HarBrowser, HarContent, HarContentPolicy, HarCreator, HarEntry, HarHeader, HarLog, HarMode,
    HarNotFound, HarPostData, HarQueryParam, HarRecorder, HarRequest, HarResponse, HarRouteOptions,
    HarRouter, HarTimings, IgnoreDefaultArgs, IndexedDbDatabase, IndexedDbIndex, IndexedDbRecord,
    IndexedDbStore, MarkdownOptions, RecordHarOptions, ResourceType, RouteFetchHandle,
    RouteFetchOptions, SnapshotOptions, WebSocketEvent, Worker, WorkerEvent,
};
pub use config::{EngineConfig, JobOptions, Priority, QueueFullMode, ResourcePreset};
pub use error::{Error, Result};
pub use expect::{expect_locator, expect_page, LocatorExpect, PageExpect};
pub use metrics::MetricsSnapshot;
pub use pool::{
    with_timeout, ApiRequestContext, BrowserInfo, ContextLease, Downloads, Engine, EngineBuilder,
    Frame, FrameLocator, Locator, PageLease,
};
pub use profiles::{validate_profile_name, ProfileInfo, ProfileStore};
pub use proxies::{parse_proxy_url, ProxyLease, ProxyPool, ProxyRotation, ProxyStatus};
pub use source::{BrowserSource, LocalChromium, RemoteCdp};
pub use tracing_rec::{Tracing, TracingChunkOptions, TracingStartOptions};
