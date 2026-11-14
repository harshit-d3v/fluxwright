//! Requests handed to the job to fulfill, change or abort, as Playwright's `page.route` does.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::connection::Connection;
use crate::error::{Error, Result};

/// A paused request. Answer it with [`fulfill`](Self::fulfill), [`continue_with`](Self::continue_with)
/// or [`abort`](Self::abort); one that is dropped unanswered continues unchanged.
pub struct InterceptedRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub post_data: Option<String>,
    /// Playwright's names: `document`, `script`, `stylesheet`, `image`, `xhr`, `fetch`, ...
    pub resource_type: String,
    reply: Reply,
}

/// A response made up by the job.
#[derive(Debug, Clone, Default)]
pub struct Fulfill {
    /// Default 200.
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Changes to make before the request goes on. `headers` replaces all of them.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Option<Vec<(String, String)>>,
    pub post_data: Option<Vec<u8>>,
}

struct Reply {
    conn: Arc<Connection>,
    session: String,
    request_id: String,
    runtime: tokio::runtime::Handle,
    done: bool,
}

impl Reply {
    async fn send(&mut self, method: &str, mut params: Value) -> Result<()> {
        params["requestId"] = json!(self.request_id);
        self.conn.call(method, params, Some(&self.session), Duration::from_secs(10)).await?;
        // Only once Chrome took the answer: a rejected one (a bad header, say) leaves Drop to
        // continue the request, which would otherwise stay paused for good.
        self.done = true;
        Ok(())
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let (conn, session, id) = (self.conn.clone(), self.session.clone(), self.request_id.clone());
        self.runtime.spawn(async move {
            let _ = conn.call("Fetch.continueRequest", json!({ "requestId": id }), Some(&session), Duration::from_secs(10)).await;
        });
    }
}

impl fmt::Debug for InterceptedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ({})", self.method, self.url, self.resource_type)
    }
}

fn header_list(headers: &[(String, String)]) -> Value {
    headers.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect()
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn parse_paused_fields(params: &Value) -> Option<(String, String, Vec<(String, String)>, Option<String>, String, String)> {
    let request = &params["request"];
    let url = request["url"].as_str()?.to_string();
    let request_id = params["requestId"].as_str()?.to_string();
    let method = request["method"].as_str().unwrap_or("GET").to_string();
    let headers = request["headers"]
        .as_object()
        // Lower-cased, as Playwright and our network events give them: Chrome sends
        // `User-Agent`, and `{...headers, 'user-agent': x}` then carried both.
        .map(|h| h.iter().map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    let post_data = request["postData"].as_str().map(str::to_owned);
    let resource_type = params["resourceType"].as_str().unwrap_or("Other").to_lowercase();
    Some((url, method, headers, post_data, resource_type, request_id))
}

impl InterceptedRequest {
    /// From a `Fetch.requestPaused` event on `session`.
    pub(crate) fn from_paused(params: &Value, conn: Arc<Connection>, session: String) -> Option<Self> {
        let (url, method, headers, post_data, resource_type, request_id) =
            parse_paused_fields(params)?;
        Some(InterceptedRequest {
            url,
            method,
            headers,
            post_data,
            resource_type,
            reply: Reply {
                conn,
                session,
                request_id,
                runtime: tokio::runtime::Handle::try_current().ok()?,
                done: false,
            },
        })
    }

    pub async fn fulfill(mut self, f: Fulfill) -> Result<()> {
        let params = json!({
            "responseCode": f.status.unwrap_or(200),
            "responseHeaders": header_list(&f.headers),
            "body": base64(&f.body),
        });
        self.reply.send("Fetch.fulfillRequest", params).await
    }

    pub async fn continue_with(mut self, o: Overrides) -> Result<()> {
        let mut params = json!({});
        if let Some(url) = o.url {
            params["url"] = json!(url);
        }
        if let Some(method) = o.method {
            params["method"] = json!(method);
        }
        if let Some(headers) = o.headers {
            params["headers"] = header_list(&headers);
        }
        if let Some(body) = o.post_data {
            params["postData"] = json!(base64(&body));
        }
        self.reply.send("Fetch.continueRequest", params).await
    }

    /// Fails the request with one of Playwright's error codes: `aborted`, `failed`,
    /// `blockedbyclient`, `connectionrefused`, `timedout`, ...
    pub async fn abort(mut self, error_code: &str) -> Result<()> {
        let reason = error_reason(error_code)?;
        self.reply.send("Fetch.failRequest", json!({ "errorReason": reason })).await
    }
}

fn error_reason(code: &str) -> Result<&'static str> {
    Ok(match code {
        "aborted" => "Aborted",
        "accessdenied" => "AccessDenied",
        "addressunreachable" => "AddressUnreachable",
        "blockedbyclient" => "BlockedByClient",
        "blockedbyresponse" => "BlockedByResponse",
        "connectionaborted" => "ConnectionAborted",
        "connectionclosed" => "ConnectionClosed",
        "connectionfailed" => "ConnectionFailed",
        "connectionrefused" => "ConnectionRefused",
        "connectionreset" => "ConnectionReset",
        "internetdisconnected" => "InternetDisconnected",
        "namenotresolved" => "NameNotResolved",
        "timedout" => "TimedOut",
        "failed" => "Failed",
        _ => return Err(Error::Other(format!("unknown abort error code `{code}`"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paused_fields_lowercases_headers_and_defaults_method() {
        let params = json!({
            "requestId": "req-1",
            "resourceType": "Script",
            "request": {
                "url": "https://example.com/app.js",
                "headers": {
                    "User-Agent": "Fluxwright/1.0",
                    "X-Custom": "value"
                },
                "postData": "{\"ok\":true}"
            }
        });
        let (url, method, mut headers, post_data, resource_type, request_id) =
            parse_paused_fields(&params).expect("parsed");
        headers.sort();
        assert_eq!(url, "https://example.com/app.js");
        assert_eq!(method, "GET");
        assert_eq!(
            headers,
            vec![
                ("user-agent".into(), "Fluxwright/1.0".into()),
                ("x-custom".into(), "value".into()),
            ]
        );
        assert_eq!(post_data.as_deref(), Some("{\"ok\":true}"));
        assert_eq!(resource_type, "script");
        assert_eq!(request_id, "req-1");
    }

    #[test]
    fn parse_paused_fields_requires_url_and_request_id() {
        assert!(parse_paused_fields(&json!({ "request": {} })).is_none());
        assert!(parse_paused_fields(&json!({ "requestId": "r1", "request": {} })).is_none());
        assert!(
            parse_paused_fields(&json!({ "request": { "url": "https://a.test" } })).is_none()
        );
    }

    #[test]
    fn header_list_and_base64_encode_cdp_shapes() {
        let encoded = header_list(&[("x-test".into(), "42".into())]);
        assert_eq!(encoded, json!([{ "name": "x-test", "value": "42" }]));
        assert_eq!(base64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn error_reason_maps_known_codes_and_rejects_unknown() {
        for (code, expected) in [
            ("aborted", "Aborted"),
            ("accessdenied", "AccessDenied"),
            ("addressunreachable", "AddressUnreachable"),
            ("blockedbyclient", "BlockedByClient"),
            ("blockedbyresponse", "BlockedByResponse"),
            ("connectionaborted", "ConnectionAborted"),
            ("connectionclosed", "ConnectionClosed"),
            ("connectionfailed", "ConnectionFailed"),
            ("connectionrefused", "ConnectionRefused"),
            ("connectionreset", "ConnectionReset"),
            ("internetdisconnected", "InternetDisconnected"),
            ("namenotresolved", "NameNotResolved"),
            ("timedout", "TimedOut"),
            ("failed", "Failed"),
        ] {
            assert_eq!(error_reason(code).unwrap(), expected);
        }
        assert!(error_reason("bogus").is_err());
    }
}
