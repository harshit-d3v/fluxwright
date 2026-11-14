use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fluxwright_cdp::{CdpBrowser, LaunchOptions, Result as CdpResult};

/// How the engine obtains browsers. Local Chromium is the v0 implementation;
/// a remote CDP endpoint can be a second impl later.
pub trait BrowserSource: Send + Sync {
    fn launch(
        &self,
        opts: &LaunchOptions,
    ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>>;
}

pub struct LocalChromium;

impl BrowserSource for LocalChromium {
    fn launch(
        &self,
        opts: &LaunchOptions,
    ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
        let opts = opts.clone();
        Box::pin(async move { CdpBrowser::launch(opts).await })
    }
}

/// Obtains browsers by connecting to an external CDP endpoint (e.g. `ws://...` or `http://...`).
pub struct RemoteCdp {
    endpoint_url: String,
}

impl RemoteCdp {
    pub fn new(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
        }
    }
}

impl BrowserSource for RemoteCdp {
    fn launch(
        &self,
        _opts: &LaunchOptions,
    ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
        Box::pin(async move { CdpBrowser::connect(&self.endpoint_url).await })
    }
}
