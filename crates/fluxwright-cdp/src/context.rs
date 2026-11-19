//! What Playwright passes to `newContext`: emulation, permissions, and saved storage
//! (cookies plus localStorage) in Playwright's `storageState` format.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::http_client::ParsedUrl;

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
    /// The mobile viewport: the meta viewport tag counts and touch scrollbars show.
    pub is_mobile: bool,
    /// Touch events, `ontouchstart` and `navigator.maxTouchPoints`.
    pub has_touch: bool,
    /// Sent with every request the page makes.
    pub extra_http_headers: Vec<(String, String)>,
    /// Load pages whose certificate Chrome would refuse (self-signed, expired, wrong host).
    pub ignore_https_errors: bool,
    /// `Some(false)` stops the page's own scripts; `evaluate` still runs.
    pub javascript_enabled: Option<bool>,
    /// No network: requests fail and `navigator.onLine` is false.
    pub offline: bool,
    /// Bypass the page's Content-Security-Policy (`Page.setBypassCSP`).
    pub bypass_csp: bool,
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
        if self.has_touch {
            calls.push(("Emulation.setTouchEmulationEnabled", json!({ "enabled": true })));
        }
        if self.javascript_enabled == Some(false) {
            calls.push(("Emulation.setScriptExecutionDisabled", json!({ "value": true })));
        }
        if !self.extra_http_headers.is_empty() {
            let headers: serde_json::Map<String, Value> =
                self.extra_http_headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            calls.push(("Network.setExtraHTTPHeaders", json!({ "headers": headers })));
        }
        if self.ignore_https_errors {
            calls.push(("Security.setIgnoreCertificateErrors", json!({ "ignore": true })));
        }
        if self.offline {
            calls.push((
                "Network.emulateNetworkConditions",
                json!({ "offline": true, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1 }),
            ));
        }
        if page && self.bypass_csp {
            calls.push(("Page.setBypassCSP", json!({ "enabled": true })));
        }
        if page && (self.viewport.is_some() || self.device_scale_factor.is_some() || self.is_mobile) {
            // 0 keeps the window's size, or the screen's scale.
            let (width, height) = self.viewport.unwrap_or((0, 0));
            calls.push((
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": self.device_scale_factor.unwrap_or(0.0),
                    "mobile": self.is_mobile
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

/// Cookies, localStorage, and IndexedDB in Playwright's `storageState` JSON format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StorageState {
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    #[serde(default)]
    pub origins: Vec<OriginStorage>,
}

/// Optional exact-match filters for `context.clear_cookies_filtered`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieFilter {
    pub name: Option<String>,
    pub domain: Option<String>,
    pub path: Option<String>,
}

impl CookieFilter {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.domain.is_none() && self.path.is_none()
    }

    pub fn matches(&self, c: &Cookie) -> bool {
        if let Some(name) = &self.name {
            if c.name != *name {
                return false;
            }
        }
        if let Some(domain) = &self.domain {
            let d = domain.to_ascii_lowercase();
            let cd = c.domain.to_ascii_lowercase();
            let domain_ok = if d.starts_with('.') {
                cd == d
            } else {
                cd == d || cd == format!(".{d}")
            };
            if !domain_ok {
                return false;
            }
        }
        if let Some(path) = &self.path {
            if c.path != *path {
                return false;
            }
        }
        true
    }
}

/// Returns cookies that apply to at least one of `urls` (or all cookies when `urls` is empty).
pub fn filter_cookies(cookies: &[Cookie], urls: &[String]) -> Result<Vec<Cookie>> {
    if urls.is_empty() {
        return Ok(cookies.to_vec());
    }
    let parsed: Vec<ParsedUrl> = urls
        .iter()
        .map(|u| ParsedUrl::parse(u))
        .collect::<Result<Vec<_>>>()?;
    Ok(cookies
        .iter()
        .filter(|c| parsed.iter().any(|u| cookie_matches_url(c, u)))
        .cloned()
        .collect())
}

fn cookie_matches_url(c: &Cookie, u: &ParsedUrl) -> bool {
    if c.secure && u.scheme != "https" && u.host != "localhost" {
        return false;
    }
    let cd = c.domain.to_ascii_lowercase();
    let domain_ok = if let Some(bare) = cd.strip_prefix('.') {
        u.host == bare || u.host.ends_with(&cd)
    } else {
        u.host == cd
    };
    if !domain_ok {
        return false;
    }
    let cp = if c.path.is_empty() { "/" } else { c.path.as_str() };
    u.path == cp
        || (u.path.starts_with(cp)
            && (cp.ends_with('/') || u.path.as_bytes().get(cp.len()) == Some(&b'/')))
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
    /// Top-level site of a partitioned (CHIPS) cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<String>,
    /// The rest of Chrome's partition key, under Playwright's name for it.
    #[serde(rename = "_crHasCrossSiteAncestor", default, skip_serializing_if = "Option::is_none")]
    pub cross_site_ancestor: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginStorage {
    pub origin: String,
    #[serde(default)]
    pub local_storage: Vec<StorageItem>,
    #[serde(rename = "indexedDB", default, skip_serializing_if = "Option::is_none")]
    pub indexed_db: Option<Vec<IndexedDbDatabase>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedDbDatabase {
    pub name: String,
    pub version: u64,
    #[serde(default)]
    pub stores: Vec<IndexedDbStore>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedDbStore {
    pub name: String,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path_array: Option<Vec<String>>,
    #[serde(default)]
    pub records: Vec<IndexedDbRecord>,
    #[serde(default)]
    pub indexes: Vec<IndexedDbIndex>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedDbRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_encoded: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_encoded: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedDbIndex {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path_array: Option<Vec<String>>,
    #[serde(default)]
    pub multi_entry: bool,
    #[serde(default)]
    pub unique: bool,
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
            // An object in current Chrome, a plain site in older versions.
            partition_key: c["partitionKey"]["topLevelSite"].as_str().or(c["partitionKey"].as_str()).map(str::to_owned),
            cross_site_ancestor: c["partitionKey"]["hasCrossSiteAncestor"].as_bool(),
        })
    }

    pub(crate) fn to_cdp(&self) -> Value {
        let mut c = json!({
            "name": self.name,
            "value": self.value,
            "httpOnly": self.http_only,
            "secure": self.secure,
            "sameSite": if self.same_site.is_empty() { "Lax" } else { &self.same_site },
        });
        if self.domain.starts_with("http://") || self.domain.starts_with("https://") {
            c["url"] = json!(self.domain);
        } else {
            c["domain"] = json!(self.domain);
            c["path"] = json!(if self.path.is_empty() { "/" } else { &self.path });
        }
        if self.expires >= 0.0 {
            c["expires"] = json!(self.expires);
        }
        if let Some(site) = &self.partition_key {
            c["partitionKey"] = json!({
                "topLevelSite": site,
                "hasCrossSiteAncestor": self.cross_site_ancestor.unwrap_or(false)
            });
        }
        c
    }
}

impl StorageState {
    /// Fills localStorage and IndexedDB the first time each saved origin loads in a page. A
    /// sessionStorage marker stops later loads from undoing the page's own changes. Initiating
    /// `indexedDB.open(..., version)` synchronously holds Chromium's `versionchange` lock so page
    /// scripts opening the DB wait for stores and records to commit.
    pub(crate) fn restore_script(&self) -> Option<String> {
        let has_idb = self
            .origins
            .iter()
            .any(|o| o.indexed_db.as_ref().is_some_and(|dbs| !dbs.is_empty()));
        let saved: serde_json::Map<String, Value> = self
            .origins
            .iter()
            .filter(|o| {
                !o.local_storage.is_empty()
                    || o.indexed_db.as_ref().is_some_and(|dbs| !dbs.is_empty())
            })
            .map(|o| {
                let items: Vec<Value> = o
                    .local_storage
                    .iter()
                    .map(|i| json!([i.name, i.value]))
                    .collect();
                if has_idb {
                    let dbs = o.indexed_db.as_deref().unwrap_or(&[]);
                    (
                        o.origin.clone(),
                        json!({ "localStorage": items, "indexedDB": dbs }),
                    )
                } else {
                    (o.origin.clone(), Value::Array(items))
                }
            })
            .collect();
        if saved.is_empty() {
            return None;
        }
        if !has_idb {
            return Some(format!(
                "(() => {{ const items = {}[location.origin]; if (!items) return; try {{ \
                 if (sessionStorage.getItem('__fluxwright_restored')) return; \
                 for (const [k, v] of items) localStorage.setItem(k, v); \
                 sessionStorage.setItem('__fluxwright_restored', '1'); }} catch {{}} }})()",
                Value::Object(saved)
            ));
        }
        Some(format!(
            "(() => {{ \
const entry = ({})[location.origin]; \
if (!entry) return; \
try {{ \
  if (sessionStorage.getItem('__fluxwright_restored')) return; \
  sessionStorage.setItem('__fluxwright_restored', '1'); \
  for (const [k, v] of (entry.localStorage || [])) localStorage.setItem(k, v); \
  const dbs = entry.indexedDB || []; \
  if (!dbs.length || typeof indexedDB === 'undefined') return; \
  const b64ToBuf = (b64) => {{ \
    const bin = atob(b64); \
    const out = new Uint8Array(bin.length); \
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i); \
    return out.buffer; \
  }}; \
  const taMap = {{ i8: Int8Array, ui8: Uint8Array, ui8c: Uint8ClampedArray, i16: Int16Array, ui16: Uint16Array, i32: Int32Array, ui32: Uint32Array, f32: Float32Array, f64: Float64Array, bi64: BigInt64Array, bui64: BigUint64Array }}; \
  const dec = (v) => {{ \
    if (v === null || typeof v !== 'object') return v; \
    if ('v' in v) {{ \
      if (v.v === 'undefined') return undefined; \
      if (v.v === 'null') return null; \
      if (v.v === 'NaN') return NaN; \
      if (v.v === 'Infinity') return Infinity; \
      if (v.v === '-Infinity') return -Infinity; \
      if (v.v === '-0') return -0; \
      return undefined; \
    }} \
    if ('d' in v) return new Date(v.d); \
    if ('u' in v) return new URL(v.u); \
    if ('bi' in v) return BigInt(v.bi); \
    if ('r' in v) return new RegExp(v.r.p, v.r.f); \
    if ('ab' in v) return b64ToBuf(v.ab); \
    if ('ta' in v && taMap[v.ta.k]) return new taMap[v.ta.k](b64ToBuf(v.ta.b)); \
    if ('m' in v) return new Map(v.m.map(([k, val]) => [dec(k), dec(val)])); \
    if ('s' in v) return new Set(v.s.map(dec)); \
    if ('a' in v) return v.a.map(dec); \
    if ('o' in v) {{ const obj = {{}}; for (const {{ k, v: val }} of v.o) obj[k] = dec(val); return obj; }} \
    return v; \
  }}; \
  for (const dbInfo of dbs) {{ \
    const req = indexedDB.open(dbInfo.name, dbInfo.version || 1); \
    let upgraded = false; \
    req.onupgradeneeded = () => {{ \
      upgraded = true; \
      const db = req.result; \
      for (const s of dbInfo.stores || []) {{ \
        if (db.objectStoreNames.contains(s.name)) db.deleteObjectStore(s.name); \
        const kp = s.keyPathArray !== undefined ? s.keyPathArray : (s.keyPath !== undefined ? s.keyPath : null); \
        const os = db.createObjectStore(s.name, {{ autoIncrement: !!s.autoIncrement, ...(kp !== null ? {{ keyPath: kp }} : {{}}) }}); \
        for (const idx of s.indexes || []) {{ \
          const ikp = idx.keyPathArray !== undefined ? idx.keyPathArray : idx.keyPath; \
          if (ikp !== undefined) os.createIndex(idx.name, ikp, {{ unique: !!idx.unique, multiEntry: !!idx.multiEntry }}); \
        }} \
        const inline = kp !== null; \
        for (const rec of s.records || []) {{ \
          const val = rec.value !== undefined ? rec.value : dec(rec.valueEncoded); \
          const key = rec.key !== undefined ? rec.key : dec(rec.keyEncoded); \
          if (inline || key === undefined) os.put(val); \
          else os.put(val, key); \
        }} \
      }} \
    }}; \
    req.onsuccess = () => {{ \
      const db = req.result; \
      if (upgraded || !(dbInfo.stores || []).length) {{ db.close(); return; }} \
      const names = (dbInfo.stores || []).map(s => s.name).filter(n => db.objectStoreNames.contains(n)); \
      if (!names.length) {{ db.close(); return; }} \
      const tx = db.transaction(names, 'readwrite'); \
      for (const s of dbInfo.stores || []) {{ \
        if (!db.objectStoreNames.contains(s.name)) continue; \
        const os = tx.objectStore(s.name); \
        os.clear(); \
        const inline = s.keyPathArray !== undefined || s.keyPath !== undefined; \
        for (const rec of s.records || []) {{ \
          const val = rec.value !== undefined ? rec.value : dec(rec.valueEncoded); \
          const key = rec.key !== undefined ? rec.key : dec(rec.keyEncoded); \
          if (inline || key === undefined) os.put(val); \
          else os.put(val, key); \
        }} \
      }} \
      tx.oncomplete = () => db.close(); \
      tx.onabort = () => db.close(); \
    }}; \
  }} \
}} catch {{}} }})()",
            Value::Object(saved)
        ))
    }
}

/// localStorage of the page's own origin, or `null` where there is none (about:blank, data:).
pub(crate) const LOCAL_STORAGE_JS: &str = "(() => { try { \
    const localStorage_ = Object.keys(localStorage).map(name => ({ name, value: localStorage.getItem(name) })); \
    return { origin: location.origin, localStorage: localStorage_ }; } catch { return null; } })()";

/// localStorage + IndexedDB of the page's own origin, or `null` where there is none.
pub(crate) const STORAGE_WITH_INDEXED_DB_JS: &str = r#"(async () => {
  try {
    const origin = location.origin;
    if (!origin || origin === 'null') return null;
    const localStorage_ = Object.keys(localStorage).map(name => ({ name, value: localStorage.getItem(name) }));
    const decodeKeyPath = (kp) => {
      if (kp == null) return {};
      if (Array.isArray(kp)) return { keyPathArray: [...kp] };
      return { keyPath: String(kp) };
    };
    const isSimple = (v, seen = new Set()) => {
      if (v === null) return true;
      const t = typeof v;
      if (t === 'boolean' || t === 'string') return true;
      if (t === 'number') return Number.isFinite(v) && !Object.is(v, -0);
      if (t !== 'object') return false;
      if (v instanceof Date || v instanceof RegExp || v instanceof ArrayBuffer || ArrayBuffer.isView(v) || v instanceof Map || v instanceof Set || v instanceof URL || v instanceof Error) return false;
      if (seen.has(v)) return false;
      seen.add(v);
      if (Array.isArray(v)) {
        const ok = v.every(item => item !== undefined && isSimple(item, seen));
        seen.delete(v);
        return ok;
      }
      const proto = Object.getPrototypeOf(v);
      if (proto !== null && proto !== Object.prototype) return false;
      for (const k of Object.keys(v)) {
        if (v[k] === undefined || !isSimple(v[k], seen)) { seen.delete(v); return false; }
      }
      seen.delete(v);
      return true;
    };
    const abToB64 = (buf, offset = 0, len = buf.byteLength) => {
      const bytes = new Uint8Array(buf, offset, len);
      let bin = '';
      for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
      return btoa(bin);
    };
    const encodeVal = (v) => {
      if (v === undefined) return { v: 'undefined' };
      if (v === null) return { v: 'null' };
      if (typeof v === 'number') {
        if (Number.isNaN(v)) return { v: 'NaN' };
        if (v === Infinity) return { v: 'Infinity' };
        if (v === -Infinity) return { v: '-Infinity' };
        if (Object.is(v, -0)) return { v: '-0' };
        return v;
      }
      if (typeof v === 'boolean' || typeof v === 'string') return v;
      if (typeof v === 'bigint') return { bi: v.toString() };
      if (v instanceof Date) return { d: v.toISOString() };
      if (v instanceof URL) return { u: v.toString() };
      if (v instanceof RegExp) return { r: { p: v.source, f: v.flags } };
      if (v instanceof ArrayBuffer) return { ab: abToB64(v) };
      if (ArrayBuffer.isView(v)) {
        const map = [
          [Int8Array, 'i8'], [Uint8Array, 'ui8'], [Uint8ClampedArray, 'ui8c'],
          [Int16Array, 'i16'], [Uint16Array, 'ui16'], [Int32Array, 'i32'],
          [Uint32Array, 'ui32'], [Float32Array, 'f32'], [Float64Array, 'f64'],
          [BigInt64Array, 'bi64'], [BigUint64Array, 'bui64']
        ];
        const found = map.find(([C]) => v instanceof C);
        if (found) return { ta: { b: abToB64(v.buffer, v.byteOffset, v.byteLength), k: found[1] } };
      }
      if (v instanceof Map) return { m: Array.from(v.entries()).map(([k, val]) => [encodeVal(k), encodeVal(val)]) };
      if (v instanceof Set) return { s: Array.from(v.values()).map(encodeVal) };
      if (Array.isArray(v)) return { a: v.map(encodeVal) };
      if (typeof v === 'object') {
        return { o: Object.keys(v).map(k => ({ k, v: encodeVal(v[k]) })) };
      }
      return null;
    };
    const dbs = typeof indexedDB !== 'undefined' && indexedDB.databases ? await indexedDB.databases() : [];
    const indexedDB_ = [];
    for (const dbMeta of dbs) {
      if (!dbMeta.name) continue;
      const db = await new Promise((resolve, reject) => {
        const req = indexedDB.open(dbMeta.name);
        req.onsuccess = () => resolve(req.result);
        req.onerror = () => reject(req.error);
      });
      try {
        const storeNames = Array.from(db.objectStoreNames);
        let stores = [];
        if (storeNames.length > 0) {
          const tx = db.transaction(storeNames, 'readonly');
          stores = await Promise.all(storeNames.map(sName => new Promise((resolve, reject) => {
            const os = tx.objectStore(sName);
            const indexes = Array.from(os.indexNames).map(iName => {
              const idx = os.index(iName);
              return { name: idx.name, ...decodeKeyPath(idx.keyPath), multiEntry: idx.multiEntry, unique: idx.unique };
            });
            const records = [];
            const curReq = os.openCursor();
            curReq.onsuccess = () => {
              const cur = curReq.result;
              if (!cur) {
                resolve({ name: os.name, autoIncrement: os.autoIncrement, ...decodeKeyPath(os.keyPath), records, indexes });
                return;
              }
              const rec = {};
              if (os.keyPath === null) {
                if (isSimple(cur.primaryKey)) rec.key = cur.primaryKey;
                else rec.keyEncoded = encodeVal(cur.primaryKey);
              }
              if (isSimple(cur.value)) rec.value = cur.value;
              else rec.valueEncoded = encodeVal(cur.value);
              records.push(rec);
              cur.continue();
            };
            curReq.onerror = () => reject(curReq.error);
          })));
        }
        indexedDB_.push({ name: db.name, version: db.version, stores });
      } finally {
        db.close();
      }
    }
    return { origin, localStorage: localStorage_, indexedDB: indexedDB_ };
  } catch {
    return null;
  }
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_cookie(name: &str, domain: &str, path: &str, secure: bool) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: path.into(),
            expires: -1.0,
            http_only: false,
            secure,
            same_site: "Lax".into(),
            partition_key: None,
            cross_site_ancestor: None,
        }
    }

    #[test]
    fn filter_cookies_matches_domain_path_and_secure() {
        let cookies = vec![
            mk_cookie("a", "example.com", "/", false),
            mk_cookie("b", ".example.com", "/app", false),
            mk_cookie("c", ".example.com", "/", true),
            mk_cookie("d", "other.test", "/", false),
        ];
        let http_app = filter_cookies(&cookies, &["http://sub.example.com/app/users".into()]).unwrap();
        let names: Vec<&str> = http_app.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["b"]);

        let http_apple = filter_cookies(&cookies, &["http://example.com/apple".into()]).unwrap();
        let names: Vec<&str> = http_apple.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a"]);

        let https_sub = filter_cookies(&cookies, &["https://sub.example.com/".into()]).unwrap();
        let names: Vec<&str> = https_sub.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["c"]);

        assert!(filter_cookies(&cookies, &["not-a-url".into()]).is_err());
    }

    #[test]
    fn cookie_filter_matches_fields_and_dot_domains() {
        let dot = mk_cookie("sid", ".example.com", "/api", false);
        let host = mk_cookie("sid", "example.com", "/", false);
        let f = CookieFilter {
            name: Some("sid".into()),
            domain: Some("example.com".into()),
            path: Some("/api".into()),
        };
        assert!(f.matches(&dot));
        assert!(!f.matches(&host));
        assert!(!f.is_empty());
        assert!(CookieFilter::default().is_empty());
    }

    #[test]
    fn cookies_round_trip_through_cdp() {
        let cdp = json!({ "name": "sid", "value": "1", "domain": "a.test", "path": "/",
            "expires": -1, "session": true, "httpOnly": true, "secure": false });
        let c = Cookie::from_cdp(&cdp).unwrap();
        assert_eq!((c.expires, c.same_site.as_str(), c.http_only), (-1.0, "Lax", true));
        assert!(c.to_cdp().get("expires").is_none(), "a session cookie must not get an expiry");
        assert_eq!(c.partition_key, None);
        assert!(c.to_cdp().get("partitionKey").is_none());
    }

    #[test]
    fn partitioned_cookies_keep_their_partition() {
        let key = json!({ "topLevelSite": "https://shop.test", "hasCrossSiteAncestor": true });
        let cdp = json!({ "name": "p", "value": "1", "domain": "w.test", "path": "/", "expires": 9e9,
            "secure": true, "sameSite": "None", "partitionKey": key });
        let c = Cookie::from_cdp(&cdp).unwrap();
        assert_eq!(c.to_cdp()["partitionKey"], key);
        // Playwright's field names, in and out.
        let file = serde_json::to_value(&c).unwrap();
        assert_eq!((&file["partitionKey"], &file["_crHasCrossSiteAncestor"]), (&json!("https://shop.test"), &json!(true)));
        assert_eq!(serde_json::from_value::<Cookie>(file).unwrap(), c);
    }

    #[test]
    fn restore_script_only_for_origins_with_items() {
        let mut state = StorageState::default();
        assert!(state.restore_script().is_none());
        state.origins.push(OriginStorage { origin: "http://a.test".into(), local_storage: vec![], indexed_db: None });
        assert!(state.restore_script().is_none());
        state.origins[0].local_storage.push(StorageItem { name: "k".into(), value: "it's".into() });
        let js = state.restore_script().unwrap();
        assert!(js.contains(r#""http://a.test":[["k","it's"]]"#), "{js}");
    }

    #[test]
    fn indexed_db_storage_state_serializes_and_restores() {
        let raw = json!({
            "cookies": [],
            "origins": [{
                "origin": "http://127.0.0.1:8080",
                "localStorage": [],
                "indexedDB": [{
                    "name": "authDb",
                    "version": 1,
                    "stores": [{
                        "name": "tokens",
                        "autoIncrement": false,
                        "keyPath": "id",
                        "records": [{ "value": { "id": "u1", "token": "secret" } }],
                        "indexes": [{ "name": "byToken", "keyPath": "token", "multiEntry": false, "unique": true }]
                    }]
                }]
            }]
        });
        let parsed: StorageState = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
        let js = parsed.restore_script().unwrap();
        assert!(js.contains("authDb") && js.contains("onupgradeneeded"), "{js}");
    }

    #[test]
    fn page_options_become_their_cdp_calls() {
        let e = Emulation {
            is_mobile: true,
            has_touch: true,
            extra_http_headers: vec![("X-Job".into(), "7".into())],
            ignore_https_errors: true,
            javascript_enabled: Some(false),
            offline: true,
            bypass_csp: true,
            ..Default::default()
        };
        let calls = e.calls(true);
        let find = |name: &str| calls.iter().find(|(m, _)| *m == name).map(|(_, p)| p.clone());
        assert_eq!(find("Emulation.setTouchEmulationEnabled").unwrap()["enabled"], true);
        assert_eq!(find("Emulation.setScriptExecutionDisabled").unwrap()["value"], true);
        assert_eq!(find("Network.setExtraHTTPHeaders").unwrap()["headers"]["X-Job"], "7");
        assert_eq!(find("Security.setIgnoreCertificateErrors").unwrap()["ignore"], true);
        assert_eq!(find("Network.emulateNetworkConditions").unwrap()["offline"], true);
        assert_eq!(find("Page.setBypassCSP").unwrap()["enabled"], true);
        assert_eq!(find("Emulation.setDeviceMetricsOverride").unwrap()["mobile"], true);
        // Nothing asked for, nothing sent.
        assert!(Emulation::default().calls(true).is_empty());
        assert!(Emulation { javascript_enabled: Some(true), ..Default::default() }.calls(true).is_empty());
    }

    #[test]
    fn locale_alone_sets_no_agent() {
        let e = Emulation { locale: Some("de-DE".into()), ..Default::default() };
        let methods: Vec<&str> = e.calls(true).iter().map(|(m, _)| *m).collect();
        assert_eq!(methods, ["Emulation.setLocaleOverride"]);
        assert!(permission_types(&["bogus".into()]).is_err());
    }
}
