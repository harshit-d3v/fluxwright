//! What Playwright passes to `newContext`: emulation, permissions, and saved storage
//! (cookies plus localStorage) in Playwright's `storageState` format.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// How a page presents itself. Unset fields keep Chrome's defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Emulation {
    pub user_agent: Option<String>,
    /// BCP 47 tag such as `de-DE`: `navigator.language`, `Accept-Language` and `Intl`.
    pub locale: Option<String>,
    /// IANA name such as `Asia/Tokyo`.
    pub timezone_id: Option<String>,
    /// Needs the `geolocation` permission, as in Playwright.
    pub geolocation: Option<Geolocation>,
    /// CSS pixels.
    pub viewport: Option<(u32, u32)>,
    pub device_scale_factor: Option<f64>,
    pub color_scheme: Option<ColorScheme>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geolocation {
    pub latitude: f64,
    pub longitude: f64,
    /// Meters.
    pub accuracy: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
    NoPreference,
}

impl ColorScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
            ColorScheme::NoPreference => "no-preference",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "light" => Some(ColorScheme::Light),
            "dark" => Some(ColorScheme::Dark),
            "no-preference" => Some(ColorScheme::NoPreference),
            _ => None,
        }
    }
}

impl Emulation {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// CDP calls for one page or iframe session. Viewport and scale only apply to pages.
    /// A locale without a user agent needs the agent filled in first: Chrome sets
    /// `Accept-Language` through the same call.
    pub(crate) fn calls(&self, page: bool) -> Vec<(&'static str, Value)> {
        let mut calls = Vec::new();
        if let Some(ua) = &self.user_agent {
            let mut params = json!({ "userAgent": ua });
            if let Some(locale) = &self.locale {
                params["acceptLanguage"] = json!(locale);
            }
            calls.push(("Emulation.setUserAgentOverride", params));
        }
        if let Some(locale) = &self.locale {
            calls.push(("Emulation.setLocaleOverride", json!({ "locale": locale })));
        }
        if let Some(tz) = &self.timezone_id {
            calls.push(("Emulation.setTimezoneOverride", json!({ "timezoneId": tz })));
        }
        if let Some(g) = self.geolocation {
            calls.push((
                "Emulation.setGeolocationOverride",
                json!({ "latitude": g.latitude, "longitude": g.longitude, "accuracy": g.accuracy }),
            ));
        }
        if let Some(scheme) = self.color_scheme {
            calls.push((
                "Emulation.setEmulatedMedia",
                json!({ "features": [{ "name": "prefers-color-scheme", "value": scheme.as_str() }] }),
            ));
        }
        if page && (self.viewport.is_some() || self.device_scale_factor.is_some()) {
            // 0 keeps the window's size, or the screen's scale.
            let (width, height) = self.viewport.unwrap_or((0, 0));
            calls.push((
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": self.device_scale_factor.unwrap_or(0.0),
                    "mobile": false
                }),
            ));
        }
        calls
    }
}

/// Playwright's permission names, as Chrome's `Browser.grantPermissions` spells them.
pub(crate) fn permission_types(names: &[String]) -> Result<Vec<&'static str>> {
    names
        .iter()
        .map(|name| {
            Ok(match name.as_str() {
                "geolocation" => "geolocation",
                "notifications" => "notifications",
                "camera" => "videoCapture",
                "microphone" => "audioCapture",
                "clipboard-read" => "clipboardReadWrite",
                "clipboard-write" => "clipboardSanitizedWrite",
                "midi" => "midi",
                "midi-sysex" => "midiSysex",
                "background-sync" => "backgroundSync",
                "accelerometer" | "gyroscope" | "magnetometer" | "ambient-light-sensor" => "sensors",
                "payment-handler" => "paymentHandler",
                "storage-access" => "storageAccess",
                "local-fonts" => "localFonts",
                "idle-detection" => "idleDetection",
                _ => return Err(Error::Other(format!("unknown permission `{name}`"))),
            })
        })
        .collect()
}

/// Cookies and localStorage, in Playwright's `storageState` JSON format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    #[serde(default)]
    pub origins: Vec<OriginStorage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; -1 for a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    /// `Strict`, `Lax` or `None`.
    pub same_site: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginStorage {
    pub origin: String,
    pub local_storage: Vec<StorageItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageItem {
    pub name: String,
    pub value: String,
}

impl Cookie {
    pub(crate) fn from_cdp(c: &Value) -> Option<Self> {
        Some(Cookie {
            name: c["name"].as_str()?.into(),
            value: c["value"].as_str()?.into(),
            domain: c["domain"].as_str()?.into(),
            path: c["path"].as_str()?.into(),
            expires: if c["session"].as_bool() == Some(true) { -1.0 } else { c["expires"].as_f64().unwrap_or(-1.0) },
            http_only: c["httpOnly"].as_bool().unwrap_or(false),
            secure: c["secure"].as_bool().unwrap_or(false),
            // Chrome leaves it out when the site did not set it; Lax is how Chrome treats that.
            same_site: c["sameSite"].as_str().unwrap_or("Lax").into(),
        })
    }

    pub(crate) fn to_cdp(&self) -> Value {
        let mut c = json!({
            "name": self.name,
            "value": self.value,
            "domain": self.domain,
            "path": self.path,
            "httpOnly": self.http_only,
            "secure": self.secure,
            "sameSite": self.same_site,
        });
        if self.expires >= 0.0 {
            c["expires"] = json!(self.expires);
        }
        c
    }
}

impl StorageState {
    /// Fills localStorage the first time each saved origin loads in a page. A sessionStorage
    /// marker stops later loads from undoing the page's own changes (a logout, for example).
    pub(crate) fn restore_script(&self) -> Option<String> {
        let saved: serde_json::Map<String, Value> = self
            .origins
            .iter()
            .filter(|o| !o.local_storage.is_empty())
            .map(|o| {
                let items: Vec<Value> = o.local_storage.iter().map(|i| json!([i.name, i.value])).collect();
                (o.origin.clone(), Value::Array(items))
            })
            .collect();
        if saved.is_empty() {
            return None;
        }
        Some(format!(
            "(() => {{ const items = {}[location.origin]; if (!items) return; try {{ \
             if (sessionStorage.getItem('__fluxwright_restored')) return; \
             for (const [k, v] of items) localStorage.setItem(k, v); \
             sessionStorage.setItem('__fluxwright_restored', '1'); }} catch {{}} }})()",
            Value::Object(saved)
        ))
    }
}

/// localStorage of the page's own origin, or `null` where there is none (about:blank, data:).
pub(crate) const LOCAL_STORAGE_JS: &str = "(() => { try { \
    const localStorage_ = Object.keys(localStorage).map(name => ({ name, value: localStorage.getItem(name) })); \
    return { origin: location.origin, localStorage: localStorage_ }; } catch { return null; } })()";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_round_trip_through_cdp() {
        let cdp = json!({ "name": "sid", "value": "1", "domain": "a.test", "path": "/",
            "expires": -1, "session": true, "httpOnly": true, "secure": false });
        let c = Cookie::from_cdp(&cdp).unwrap();
        assert_eq!((c.expires, c.same_site.as_str(), c.http_only), (-1.0, "Lax", true));
        assert!(c.to_cdp().get("expires").is_none(), "a session cookie must not get an expiry");
    }

    #[test]
    fn restore_script_only_for_origins_with_items() {
        let mut state = StorageState::default();
        assert!(state.restore_script().is_none());
        state.origins.push(OriginStorage { origin: "http://a.test".into(), local_storage: vec![] });
        assert!(state.restore_script().is_none());
        state.origins[0].local_storage.push(StorageItem { name: "k".into(), value: "it's".into() });
        let js = state.restore_script().unwrap();
        assert!(js.contains(r#""http://a.test":[["k","it's"]]"#), "{js}");
    }

    #[test]
    fn locale_alone_sets_no_agent() {
        let e = Emulation { locale: Some("de-DE".into()), ..Default::default() };
        let methods: Vec<&str> = e.calls(true).iter().map(|(m, _)| *m).collect();
        assert_eq!(methods, ["Emulation.setLocaleOverride"]);
        assert!(permission_types(&["bogus".into()]).is_err());
    }
}
