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

type LeaseSlot = Arc<Mutex<Option<fluxwright::PageLease>>>;

#[napi]
pub struct Page {
    lease: LeaseSlot,
}

/// Runs `f` against the page on the engine's runtime; fails with "page closed" after close().
async fn with_page<T, F>(lease: LeaseSlot, f: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(
            &'a fluxwright::PageLease,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = fluxwright::Result<T>> + Send + 'a>,
        > + Send
        + 'static,
{
    RT.spawn(async move {
        let g = lease.lock().await;
        match g.as_ref() {
            Some(p) => f(p).await.map_err(|e| e.to_string()),
            None => Err("page closed".to_string()),
        }
    })
    .await
    .map_err(|e| Error::from_reason(e.to_string()))?
    .map_err(Error::from_reason)
}

#[napi(object)]
pub struct GetByTextOptions {
    /// Default false: case-insensitive substring.
    pub exact: Option<bool>,
}

#[napi(object)]
pub struct GetByRoleOptions {
    /// Accessible name. Default: case-insensitive substring.
    pub name: Option<String>,
    pub exact: Option<bool>,
}

/// Lazy, like Playwright's: every action runs the query again.
#[napi]
pub struct Locator {
    lease: LeaseSlot,
    selector: fluxwright::Selector,
}

#[napi]
impl Locator {
    #[napi]
    pub async fn click(&self) -> Result<()> {
        let sel = self.selector.clone();
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.click(&sel).await })).await
    }

    #[napi]
    pub async fn fill(&self, value: String) -> Result<()> {
        let sel = self.selector.clone();
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.fill(&sel, &value).await })).await
    }

    /// Waits until visible.
    #[napi]
    pub async fn wait_for(&self) -> Result<()> {
        let sel = self.selector.clone();
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.wait_for_selector(&sel).await }))
            .await
    }

    #[napi]
    pub async fn text_content(&self) -> Result<Option<String>> {
        let sel = self.selector.clone();
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.text_content(&sel).await })).await
    }
}

/// An iframe, same- or cross-origin, to find elements in.
#[napi]
pub struct FrameLocator {
    lease: LeaseSlot,
    frames: Vec<String>,
}

impl FrameLocator {
    fn find(&self, query: String) -> Locator {
        Locator {
            lease: self.lease.clone(),
            selector: fluxwright::Selector { frames: self.frames.clone(), query },
        }
    }
}

#[napi]
impl FrameLocator {
    #[napi]
    pub fn locator(&self, selector: String) -> Locator {
        self.find(selector)
    }

    #[napi]
    pub fn get_by_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        let exact = options.and_then(|o| o.exact).unwrap_or(false);
        self.find(fluxwright::Selector::text(&text, exact))
    }

    #[napi]
    pub fn get_by_role(&self, role: String, options: Option<GetByRoleOptions>) -> Locator {
        let (name, exact) = options.map(|o| (o.name, o.exact.unwrap_or(false))).unwrap_or_default();
        self.find(fluxwright::Selector::role(&role, name.as_deref(), exact))
    }

    /// A nested iframe inside this one.
    #[napi]
    pub fn frame_locator(&self, selector: String) -> FrameLocator {
        let mut frames = self.frames.clone();
        frames.push(selector);
        FrameLocator { lease: self.lease.clone(), frames }
    }
}

#[napi(object)]
pub struct LaunchOptions {
    pub max_browsers: Option<u32>,
    /// Browser binary. Default: chrome-headless-shell when headless and installed, else Chrome.
    pub executable_path: Option<String>,
    /// Default true.
    pub headless: Option<bool>,
}

#[napi(object)]
pub struct ProxyOptions {
    /// `http://host:port`, `socks5://host:port`, ...
    pub server: String,
    /// Comma-separated hosts that skip the proxy.
    pub bypass: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Playwright's `newContext` options that Fluxwright supports.
#[napi(object)]
pub struct NewPageOptions {
    pub proxy: Option<ProxyOptions>,
    pub user_agent: Option<String>,
    /// BCP 47, e.g. `de-DE`: `navigator.language`, `Accept-Language` and `Intl`.
    pub locale: Option<String>,
    /// IANA, e.g. `Asia/Tokyo`.
    pub timezone_id: Option<String>,
    /// Also needs `permissions: ['geolocation']`, as in Playwright.
    pub geolocation: Option<Geolocation>,
    /// `geolocation`, `notifications`, `clipboard-read`, ... granted to every origin.
    pub permissions: Option<Vec<String>>,
    pub viewport: Option<ViewportSize>,
    pub device_scale_factor: Option<f64>,
    #[napi(ts_type = "'light' | 'dark' | 'no-preference'")]
    pub color_scheme: Option<String>,
    /// Cookies and localStorage to start from: what `page.storageState()` returned, or the
    /// path of the JSON file it saved.
    #[napi(ts_type = "StorageState | string")]
    pub storage_state: Option<StorageState>,
}

#[napi(object)]
pub struct Geolocation {
    pub latitude: f64,
    pub longitude: f64,
    /// Meters. Default 0.
    pub accuracy: Option<f64>,
}

/// Cookies and localStorage, in Playwright's `storageState` format: files work in both.
#[napi(object)]
pub struct StorageState {
    pub cookies: Vec<Cookie>,
    pub origins: Vec<OriginStorage>,
}

#[napi(object)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; -1 for a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    #[napi(ts_type = "'Strict' | 'Lax' | 'None'")]
    pub same_site: String,
}

#[napi(object)]
pub struct OriginStorage {
    pub origin: String,
    pub local_storage: Vec<StorageItem>,
}

#[napi(object)]
pub struct StorageItem {
    pub name: String,
    pub value: String,
}

impl From<StorageState> for fluxwright::StorageState {
    fn from(s: StorageState) -> Self {
        fluxwright::StorageState {
            cookies: s
                .cookies
                .into_iter()
                .map(|c| fluxwright::Cookie {
                    name: c.name,
                    value: c.value,
                    domain: c.domain,
                    path: c.path,
                    expires: c.expires,
                    http_only: c.http_only,
                    secure: c.secure,
                    same_site: c.same_site,
                })
                .collect(),
            origins: s
                .origins
                .into_iter()
                .map(|o| fluxwright::OriginStorage {
                    origin: o.origin,
                    local_storage: o
                        .local_storage
                        .into_iter()
                        .map(|i| fluxwright::StorageItem { name: i.name, value: i.value })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<fluxwright::StorageState> for StorageState {
    fn from(s: fluxwright::StorageState) -> Self {
        StorageState {
            cookies: s
                .cookies
                .into_iter()
                .map(|c| Cookie {
                    name: c.name,
                    value: c.value,
                    domain: c.domain,
                    path: c.path,
                    expires: c.expires,
                    http_only: c.http_only,
                    secure: c.secure,
                    same_site: c.same_site,
                })
                .collect(),
            origins: s
                .origins
                .into_iter()
                .map(|o| OriginStorage {
                    origin: o.origin,
                    local_storage: o.local_storage.into_iter().map(|i| StorageItem { name: i.name, value: i.value }).collect(),
                })
                .collect(),
        }
    }
}

fn job_options(o: NewPageOptions) -> Result<fluxwright::JobOptions> {
    let color_scheme = match o.color_scheme.as_deref() {
        None => None,
        Some(s) => Some(fluxwright::ColorScheme::parse(s).ok_or_else(|| {
            Error::from_reason(format!("colorScheme must be light, dark or no-preference, not `{s}`"))
        })?),
    };
    Ok(fluxwright::JobOptions {
        proxy: o.proxy.map(|p| fluxwright::Proxy {
            server: p.server,
            bypass: p.bypass,
            username: p.username,
            password: p.password,
        }),
        emulation: fluxwright::Emulation {
            user_agent: o.user_agent,
            locale: o.locale,
            timezone_id: o.timezone_id,
            geolocation: o.geolocation.map(|g| fluxwright::Geolocation {
                latitude: g.latitude,
                longitude: g.longitude,
                accuracy: g.accuracy.unwrap_or(0.0),
            }),
            viewport: o.viewport.map(|v| (v.width, v.height)),
            device_scale_factor: o.device_scale_factor,
            color_scheme,
        },
        permissions: o.permissions.unwrap_or_default(),
        storage_state: o.storage_state.map(Into::into),
        ..Default::default()
    })
}

#[napi(object)]
pub struct GotoOptions {
    /// Default "load".
    #[napi(ts_type = "'load' | 'domcontentloaded' | 'networkidle' | 'commit'")]
    pub wait_until: Option<String>,
}

#[napi(object)]
pub struct ScreenshotOptions {
    pub full_page: Option<bool>,
}

#[napi(object)]
pub struct ViewportSize {
    pub width: u32,
    pub height: u32,
}

#[napi]
pub struct Chromium {}

#[napi]
impl Chromium {
    #[napi]
    pub async fn launch(options: Option<LaunchOptions>) -> Result<Browser> {
        let (max, exe, headless) = match options {
            Some(o) => (o.max_browsers, o.executable_path, o.headless),
            None => (None, None, None),
        };
        let engine = RT
            .spawn(async move {
                let mut b = fluxwright::BrowserEngine::builder()
                    .max_browsers(max.unwrap_or(4) as usize)
                    .max_contexts_per_browser(8);
                if let Some(exe) = exe {
                    b = b.chrome(exe);
                }
                if let Some(h) = headless {
                    b = b.headless(h);
                }
                b.build().await
            })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Browser { inner: engine })
    }
}

#[napi]
impl Browser {
    /// A fresh browser context. Every option applies to this page only.
    #[napi]
    pub async fn new_page(&self, options: Option<NewPageOptions>) -> Result<Page> {
        let opts = match options {
            Some(o) => job_options(o)?,
            None => fluxwright::JobOptions::default(),
        };
        let eng = self.inner.clone();
        let lease = RT
            .spawn(async move { eng.acquire_job(&opts).await })
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
    /// Every cookie in this page's context, plus localStorage of the current origin. Pass it
    /// (or a file it was saved to) as `newPage({ storageState })` to skip logging in again.
    #[napi]
    pub async fn storage_state(&self) -> Result<StorageState> {
        with_page(self.lease.clone(), |p| Box::pin(async move { p.storage_state().await }))
            .await
            .map(Into::into)
    }

    #[napi]
    pub async fn goto(&self, url: String, options: Option<GotoOptions>) -> Result<()> {
        let wait_until = match options.and_then(|o| o.wait_until).as_deref() {
            None | Some("load") => fluxwright::WaitUntil::Load,
            Some("domcontentloaded") => fluxwright::WaitUntil::DomContentLoaded,
            Some("networkidle") => fluxwright::WaitUntil::NetworkIdle,
            Some("commit") => fluxwright::WaitUntil::Commit,
            Some(other) => {
                return Err(Error::from_reason(format!(
                    "waitUntil must be load, domcontentloaded, networkidle or commit, got {other}"
                )))
            }
        };
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.goto_with(&url, wait_until).await.map_err(|e| e.to_string())
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
    pub async fn screenshot(&self, options: Option<ScreenshotOptions>) -> Result<Buffer> {
        let full_page = options.and_then(|o| o.full_page).unwrap_or(false);
        let lease = self.lease.clone();
        let bytes = RT
            .spawn(async move {
                let g = lease.lock().await;
                if let Some(p) = g.as_ref() {
                    if full_page { p.screenshot_full_page().await } else { p.screenshot().await }
                        .map_err(|e| e.to_string())
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
    pub async fn set_viewport_size(&self, size: ViewportSize) -> Result<()> {
        let lease = self.lease.clone();
        RT.spawn(async move {
            let g = lease.lock().await;
            if let Some(p) = g.as_ref() {
                p.set_viewport_size(size.width, size.height)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                Err("page closed".into())
            }
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?
        .map_err(Error::from_reason)
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

    /// CSS, `text=`, or `role=` selector.
    #[napi]
    pub fn locator(&self, selector: String) -> Locator {
        self.top().locator(selector)
    }

    #[napi]
    pub fn get_by_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_text(text, options)
    }

    #[napi]
    pub fn get_by_role(&self, role: String, options: Option<GetByRoleOptions>) -> Locator {
        self.top().get_by_role(role, options)
    }

    #[napi]
    pub fn frame_locator(&self, selector: String) -> FrameLocator {
        self.top().frame_locator(selector)
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

impl Page {
    fn top(&self) -> FrameLocator {
        FrameLocator { lease: self.lease.clone(), frames: Vec::new() }
    }
}

#[napi]
pub fn chromium() -> Chromium {
    Chromium {}
}
