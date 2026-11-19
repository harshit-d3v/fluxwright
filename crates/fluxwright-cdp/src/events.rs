//! Network events and dialogs, for Playwright's
//! `page.on('request' | 'response' | 'requestfinished' | 'requestfailed' | 'dialog')`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::input::{canonicalize_local_paths, encode_file_payloads, FileInput};

/// A request the page, one of its frames or popups sent.
#[derive(Debug, Clone)]
pub struct NetRequest {
    /// Chrome's request id: the same for every event of one request (redirect hops included).
    pub id: String,
    pub url: String,
    pub method: String,
    /// Names lower-cased, as Playwright reports them.
    pub headers: Vec<(String, String)>,
    pub post_data: Option<String>,
    /// `document`, `script`, `xhr`, `fetch`, ..., as Playwright's `resourceType()` names them.
    pub resource_type: String,
}

impl NetRequest {
    pub(crate) fn from_sent(params: &Value) -> Self {
        let r = &params["request"];
        NetRequest {
            id: params["requestId"].as_str().unwrap_or("").to_string(),
            url: r["url"].as_str().unwrap_or("").to_string(),
            method: r["method"].as_str().unwrap_or("GET").to_string(),
            headers: headers(&r["headers"]),
            post_data: r["postData"].as_str().map(str::to_owned),
            resource_type: params["type"].as_str().unwrap_or("Other").to_lowercase(),
        }
    }
}

/// The response to a request. Its body can be read once it finished loading.
#[derive(Debug, Clone)]
pub struct NetResponse {
    pub request: NetRequest,
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub(crate) body: BodySource,
}

/// Where a response's body is read from, and whether it finished loading.
#[derive(Clone)]
pub(crate) struct BodySource {
    pub(crate) conn: Arc<Connection>,
    pub(crate) session: String,
    pub(crate) request_id: String,
    /// `None` while loading, then whether it finished or why it failed.
    pub(crate) done: watch::Receiver<Loaded>,
}

pub(crate) type Loaded = Option<std::result::Result<(), String>>;

impl std::fmt::Debug for BodySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodySource")
            .field("request_id", &self.request_id)
            .finish()
    }
}

impl NetResponse {
    pub(crate) fn from_received(request: NetRequest, params: &Value, body: BodySource) -> Self {
        let r = &params["response"];
        NetResponse {
            request,
            url: r["url"].as_str().unwrap_or("").to_string(),
            status: r["status"].as_u64().unwrap_or(0) as u16,
            status_text: r["statusText"].as_str().unwrap_or("").to_string(),
            headers: headers(&r["headers"]),
            body,
        }
    }

    /// The body, once the response finished loading. `timeout` covers the wait and the read.
    pub async fn body(&self, timeout: Duration) -> Result<Vec<u8>> {
        let mut done = self.body.done.clone();
        let outcome = tokio::time::timeout(timeout, done.wait_for(|d| d.is_some()))
            .await
            .map_err(|_| Error::timeout("the response body", timeout.as_millis() as u64))?
            .map_err(|_| {
                Error::Other("the page closed before the response finished loading".into())
            })?
            .clone();
        if let Some(Err(reason)) = outcome {
            return Err(Error::Other(format!(
                "the response failed to load: {reason}"
            )));
        }
        let res = self
            .body
            .conn
            .call(
                "Network.getResponseBody",
                json!({ "requestId": self.body.request_id }),
                Some(&self.body.session),
                timeout,
            )
            .await?;
        let text = res["body"].as_str().unwrap_or("");
        if res["base64Encoded"].as_bool().unwrap_or(false) {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|e| Error::Other(format!("response body: {e}")))
        } else {
            Ok(text.as_bytes().to_vec())
        }
    }
}

/// One network event of a page, in the order Chrome reported it.
#[derive(Debug, Clone)]
pub enum NetEvent {
    Request(NetRequest),
    Response(NetResponse),
    Finished(NetRequest),
    /// The request and why it failed (`net::ERR_FAILED`, ...).
    Failed(NetRequest, String),
}

fn headers(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    (
                        k.to_lowercase(),
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A JavaScript dialog (`alert`, `confirm`, `prompt`, `beforeunload`) waiting for an answer.
/// One dropped unanswered is answered as Fluxwright does without a listener (`beforeunload`
/// accepted, the rest dismissed), so the page never hangs on it.
pub struct Dialog {
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    pub kind: String,
    pub message: String,
    /// The prompt's default text; empty for other dialogs.
    pub default_value: String,
    answer: Option<(Arc<Connection>, String)>,
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for Dialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialog")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish()
    }
}

impl Dialog {
    /// Made on the browser's event loop, so a runtime is at hand for `Drop`.
    pub(crate) fn new(params: &Value, conn: Arc<Connection>, session: String) -> Self {
        Dialog {
            kind: params["type"].as_str().unwrap_or("alert").to_string(),
            message: params["message"].as_str().unwrap_or("").to_string(),
            default_value: params["defaultPrompt"].as_str().unwrap_or("").to_string(),
            answer: Some((conn, session)),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// Accepts it; `prompt_text` is what a `prompt` returns.
    pub async fn accept(mut self, prompt_text: Option<String>) -> Result<()> {
        let (conn, session) = self.answer.take().expect("a dialog is answered once");
        respond(&conn, &session, true, prompt_text).await
    }

    pub async fn dismiss(mut self) -> Result<()> {
        let (conn, session) = self.answer.take().expect("a dialog is answered once");
        respond(&conn, &session, false, None).await
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some((conn, session)) = self.answer.take() {
            let accept = self.kind == "beforeunload";
            self.runtime.spawn(async move {
                let _ = respond(&conn, &session, accept, None).await;
            });
        }
    }
}

pub(crate) async fn respond(
    conn: &Connection,
    session: &str,
    accept: bool,
    prompt_text: Option<String>,
) -> Result<()> {
    let mut params = json!({ "accept": accept });
    if let Some(text) = prompt_text {
        params["promptText"] = json!(text);
    }
    conn.call(
        "Page.handleJavaScriptDialog",
        params,
        Some(session),
        Duration::from_secs(5),
    )
    .await
    .map(|_| ())
}

const FILE_CHOOSER_SET_JS: &str = "function(files) { \
    const dt = new DataTransfer(); \
    for (const f of files) { \
        const bin = atob(f.base64); \
        const bytes = new Uint8Array(bin.length); \
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i); \
        dt.items.add(new File([bytes], f.name, { type: f.mimeType || '' })); \
    } \
    this.files = dt.files; \
    this.dispatchEvent(new Event('input', { bubbles: true })); \
    this.dispatchEvent(new Event('change', { bubbles: true })); \
}";

/// An intercepted file chooser dialog (`page.on('filechooser')`), holding the underlying `<input>` node.
#[derive(Clone)]
pub struct FileChooser {
    pub is_multiple: bool,
    backend_node_id: Option<i64>,
    conn: Arc<Connection>,
    session: String,
    is_remote: bool,
}

impl std::fmt::Debug for FileChooser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChooser")
            .field("is_multiple", &self.is_multiple)
            .finish()
    }
}

impl FileChooser {
    pub(crate) fn new(params: &Value, conn: Arc<Connection>, session: String, is_remote: bool) -> Self {
        Self {
            is_multiple: params["mode"].as_str() == Some("selectMultiple"),
            backend_node_id: params["backendNodeId"].as_i64(),
            conn,
            session,
            is_remote,
        }
    }

    pub async fn set_files(&self, files: &FileInput, timeout: Duration) -> Result<()> {
        let Some(backend_node_id) = self.backend_node_id else {
            return Err(Error::Other("file chooser has no backendNodeId".into()));
        };
        if files.len() > 1 && !self.is_multiple {
            return Err(Error::Other(
                "non-multiple file input can only accept single file".into(),
            ));
        }
        if let FileInput::Paths(paths) = files {
            if !self.is_remote && !paths.is_empty() {
                let strs = canonicalize_local_paths(paths).await?;
                self.conn
                    .call(
                        "DOM.setFileInputFiles",
                        json!({ "files": strs, "backendNodeId": backend_node_id }),
                        Some(&self.session),
                        timeout,
                    )
                    .await?;
                return Ok(());
            }
        }
        let encoded = encode_file_payloads(files).await?;
        let resolved = self
            .conn
            .call(
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
                Some(&self.session),
                timeout,
            )
            .await?;
        let object_id = resolved["object"]["objectId"]
            .as_str()
            .ok_or_else(|| Error::Other("file chooser input element went away".into()))?
            .to_string();
        let res = self
            .conn
            .call(
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": FILE_CHOOSER_SET_JS,
                    "objectId": object_id,
                    "arguments": [{ "value": encoded }],
                    "returnByValue": true
                }),
                Some(&self.session),
                timeout,
            )
            .await;
        let _ = self
            .conn
            .call(
                "Runtime.releaseObject",
                json!({ "objectId": object_id }),
                Some(&self.session),
                timeout,
            )
            .await;
        res.map(|_| ())
    }
}

/// An invocation of a function exposed via `expose_function` from a page or iframe.
#[derive(Clone)]
pub struct BindingCall {
    pub name: String,
    pub seq: u64,
    pub args: Vec<Value>,
    conn: Arc<Connection>,
    session: String,
    context_id: i64,
}

impl std::fmt::Debug for BindingCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindingCall")
            .field("name", &self.name)
            .field("seq", &self.seq)
            .field("args", &self.args)
            .finish()
    }
}

impl BindingCall {
    pub(crate) fn new(
        name: String,
        seq: u64,
        args: Vec<Value>,
        conn: Arc<Connection>,
        session: String,
        context_id: i64,
    ) -> Self {
        Self { name, seq, args, conn, session, context_id }
    }

    pub async fn resolve(&self, value: &Value) -> Result<()> {
        self.finish(true, value, None).await
    }

    pub async fn reject(&self, message: &str) -> Result<()> {
        self.finish(false, &Value::Null, Some(message)).await
    }

    async fn finish(&self, ok: bool, value: &Value, err_msg: Option<&str>) -> Result<()> {
        let expression = format!(
            "globalThis.__fwBindingResolve && globalThis.__fwBindingResolve({}, {}, {}, {})",
            self.seq,
            ok,
            serde_json::to_string(value).unwrap(),
            serde_json::to_string(&err_msg).unwrap(),
        );
        let mut params = json!({ "expression": expression, "returnByValue": true });
        if self.context_id > 0 {
            params["contextId"] = json!(self.context_id);
        }
        match self
            .conn
            .call("Runtime.evaluate", params, Some(&self.session), Duration::from_secs(5))
            .await
        {
            Ok(_) => Ok(()),
            Err(Error::SessionGone(_)) => Ok(()),
            Err(Error::Command { ref message, .. })
                if message.contains("Cannot find context")
                    || message.contains("Execution context was destroyed") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
