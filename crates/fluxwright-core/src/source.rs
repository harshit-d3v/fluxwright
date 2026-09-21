use std::sync::Arc;

use async_trait::async_trait;
use fluxwright_cdp::{CdpBrowser, LaunchOptions, Result as CdpResult};

/// How the engine obtains browsers. Local Chromium is the v0 implementation;
/// a remote CDP endpoint can be a second impl later.
#[async_trait]
pub trait BrowserSource: Send + Sync {
    async fn launch(&self, opts: &LaunchOptions) -> CdpResult<Arc<CdpBrowser>>;
}

pub struct LocalChromium;

#[async_trait]
impl BrowserSource for LocalChromium {
    async fn launch(&self, opts: &LaunchOptions) -> CdpResult<Arc<CdpBrowser>> {
        CdpBrowser::launch(opts.clone()).await
    }
}
