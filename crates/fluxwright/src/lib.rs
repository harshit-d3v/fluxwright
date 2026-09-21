//! Fluxwright: lease-based Chromium fleet for high-concurrency automation.
//!
//! This crate does not claim to be faster or lighter than Playwright, Puppeteer,
//! or any other tool. See `docs/COMPETITIVE_ANALYSIS.md`.

pub use fluxwright_core::{
    BrowserInfo, Engine as BrowserEngine, EngineBuilder, EngineConfig, Error, JobOptions, Locator,
    MetricsSnapshot, PageLease, Priority, QueueFullMode, Result,
};

pub use fluxwright_cdp::{find_chrome, LaunchOptions, ResourceType};
