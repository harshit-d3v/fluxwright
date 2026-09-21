use napi::bindgen_prelude::*;
use napi_derive::napi;
use once_cell::sync::Lazy;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

static RT: Lazy<Runtime> = Lazy::new(|| Runtime::new().expect("tokio"));

#[napi]
pub struct Browser {
    inner: fluxwright::BrowserEngine,
}

#[napi]
pub struct Page {
    lease: Arc<Mutex<Option<fluxwright::PageLease>>>,
}

#[napi(object)]
pub struct LaunchOptions {
    pub max_browsers: Option<u32>,
}

#[napi]
pub struct Chromium {}

#[napi]
impl Chromium {
    #[napi]
    pub async fn launch(options: Option<LaunchOptions>) -> Result<Browser> {
        let max = options.and_then(|o| o.max_browsers).unwrap_or(4) as usize;
        let engine = RT
            .spawn(async move {
                fluxwright::BrowserEngine::builder()
                    .max_browsers(max)
                    .max_contexts_per_browser(8)
                    .build()
                    .await
            })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Browser { inner: engine })
    }
}

#[napi]
impl Browser {
    #[napi]
    pub async fn new_page(&self) -> Result<Page> {
        let eng = self.inner.clone();
        let lease = RT
            .spawn(async move { eng.acquire().await })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Page {
            lease: Arc::new(Mutex::new(Some(lease))),
        })
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        let eng = self.inner.clone();
        RT.spawn(async move {
            let _ = eng.shutdown(std::time::Duration::from_secs(5)).await;
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    }
}

#[napi]
impl Page {
    #[napi]
    pub async fn goto(&self, url: String) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.goto(&url).await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn title(&self) -> Result<String> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.title().await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn content(&self) -> Result<String> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.content().await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn click(&self, selector: String) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.click(&selector).await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn fill(&self, selector: String, value: String) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.fill(&selector, &value).await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn evaluate(&self, expression: String) -> Result<serde_json::Value> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.evaluate(&expression).await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn screenshot(&self) -> Result<Buffer> {
        let lease = self.lease.clone();
        let bytes = RT
            .spawn(async move {
                let g = lease.lock().await;
                if let Some(p) = g.as_ref() {
                    p.screenshot().await.map_err(|e| e.to_string())
                } else {
                    Err("page closed".into())
                }
            })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)?;
        Ok(bytes.into())
    }

    #[napi]
    pub async fn wait_for_selector(&self, selector: String) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.wait_for_selector(&selector).await.map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let mut g = lease.lock().await;
            *g = None;
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    }
}

#[napi]
pub fn chromium() -> Chromium {
    Chromium {}
}
