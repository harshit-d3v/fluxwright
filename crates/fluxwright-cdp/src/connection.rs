use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};
use tracing::{debug, warn};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

struct Pending {
    method: String,
    tx: oneshot::Sender<Result<Value>>,
}

pub struct Connection {
    next_id: AtomicU64,
    tx: mpsc::UnboundedSender<Outgoing>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: broadcast::Sender<CdpEvent>,
    watchers: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<CdpEvent>>>>,
    dead: Arc<AtomicBool>,
    dead_notify: tokio::sync::Notify,
}

enum Outgoing {
    Json(String),
}

impl Connection {
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        // Chrome sends every CDP message as a single frame, and a full-page screenshot can
        // pass tungstenite's 16 MiB default, which closes the socket and every job on it.
        // 256 MB is Playwright's cap. Nagle off: CDP is many small request/response pairs.
        let limit = Some(256 << 20);
        let config = WebSocketConfig::default()
            .max_message_size(limit)
            .max_frame_size(limit);
        let (ws, _) = connect_async_with_config(ws_url, Some(config), true)
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        let (mut sink, mut stream) = ws.split();
        let (conn, mut out_rx, inbound) = Self::open();

        tokio::spawn(async move {
            while let Some(Outgoing::Json(s)) = out_rx.recv().await {
                if sink.send(Message::Text(s.into())).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => inbound.handle(&text).await,
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Frame(_)) => {}
                }
            }
            inbound.closed().await;
        });

        Ok(conn)
    }

    /// `--remote-debugging-pipe`: NUL-separated JSON over two pipes.
    #[cfg(unix)]
    pub fn from_pipes(to_chrome: std::os::fd::OwnedFd, from_chrome: std::os::fd::OwnedFd) -> Result<Arc<Self>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::unix::pipe;

        let mut writer = pipe::Sender::from_owned_fd(to_chrome)?;
        let reader = pipe::Receiver::from_owned_fd(from_chrome)?;
        let (conn, mut out_rx, inbound) = Self::open();

        tokio::spawn(async move {
            while let Some(Outgoing::Json(s)) = out_rx.recv().await {
                let mut msg = s.into_bytes();
                msg.push(0);
                if writer.write_all(&msg).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(0, &mut buf).await {
                    Ok(0) | Err(_) => break, // Chrome exited
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        match std::str::from_utf8(&buf) {
                            Ok(text) => inbound.handle(text).await,
                            Err(e) => warn!(error = %e, "cdp pipe: invalid utf-8"),
                        }
                    }
                }
            }
            inbound.closed().await;
        });

        Ok(conn)
    }

    /// A connection with no transport yet: the caller spawns a writer draining the
    /// receiver and a reader feeding `Inbound`.
    fn open() -> (Arc<Self>, mpsc::UnboundedReceiver<Outgoing>, Inbound) {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Outgoing>();
        let inbound = Inbound {
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16_384).0,
            watchers: Arc::new(std::sync::Mutex::new(Vec::new())),
            dead: Arc::new(AtomicBool::new(false)),
        };
        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            tx: out_tx,
            pending: inbound.pending.clone(),
            events: inbound.events.clone(),
            watchers: inbound.watchers.clone(),
            dead: inbound.dead.clone(),
            dead_notify: tokio::sync::Notify::new(),
        });
        (conn, out_rx, inbound)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// Never-lagging event stream for the browser session state machine.
    pub fn subscribe_unbounded(&self) -> mpsc::UnboundedReceiver<CdpEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.watchers.lock().unwrap().push(tx);
        rx
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_dead() {
            return Err(Error::WebSocket("connection closed".into()));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(
            id,
            Pending {
                method: method.to_string(),
                tx,
            },
        );
        let mut body = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            body["sessionId"] = json!(sid);
        }
        self.tx
            .send(Outgoing::Json(body.to_string()))
            .map_err(|_| Error::WebSocket("writer gone".into()))?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(Error::WebSocket("cancelled".into())),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(Error::timeout(method, timeout.as_millis() as u64))
            }
        }
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
        self.dead_notify.notify_waiters();
    }
}

/// The reader's half of a connection, whatever the transport.
struct Inbound {
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    events: broadcast::Sender<CdpEvent>,
    watchers: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<CdpEvent>>>>,
    dead: Arc<AtomicBool>,
}

impl Inbound {
    async fn handle(&self, text: &str) {
        if let Err(e) = dispatch(text, &self.pending, &self.events, &self.watchers).await {
            warn!(error = %e, "cdp dispatch");
        }
    }

    /// The browser end went away: fail every pending call and tell subscribers.
    async fn closed(&self) {
        self.dead.store(true, Ordering::SeqCst);
        fail_all(&self.pending, Error::WebSocket("connection closed".into())).await;
        let ev = CdpEvent {
            method: "Fluxwright.disconnected".into(),
            params: json!({}),
            session_id: None,
        };
        let _ = self.events.send(ev.clone());
        fanout(&self.watchers, ev);
    }
}

fn fanout(
    watchers: &std::sync::Mutex<Vec<mpsc::UnboundedSender<CdpEvent>>>,
    ev: CdpEvent,
) {
    watchers
        .lock()
        .unwrap()
        .retain(|tx| tx.send(ev.clone()).is_ok());
}

async fn dispatch(
    text: &str,
    pending: &Mutex<HashMap<u64, Pending>>,
    events: &broadcast::Sender<CdpEvent>,
    watchers: &std::sync::Mutex<Vec<mpsc::UnboundedSender<CdpEvent>>>,
) -> Result<()> {
    let msg: Incoming = serde_json::from_str(text)?;
    if let Some(id) = msg.id {
        if let Some(p) = pending.lock().await.remove(&id) {
            let res = if let Some(err) = msg.error {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("cdp error")
                    .to_string();
                Err(Error::Command {
                    method: p.method,
                    message,
                })
            } else {
                Ok(msg.result.unwrap_or(Value::Null))
            };
            let _ = p.tx.send(res);
        }
        return Ok(());
    }
    if let Some(method) = msg.method {
        debug!(%method, session = ?msg.session_id, "cdp event");
        let ev = CdpEvent {
            method,
            params: msg.params.unwrap_or(Value::Null),
            session_id: msg.session_id,
        };
        let _ = events.send(ev.clone());
        fanout(watchers, ev);
    }
    Ok(())
}

async fn fail_all(pending: &Mutex<HashMap<u64, Pending>>, err: Error) {
    let mut map = pending.lock().await;
    for (_, p) in map.drain() {
        let _ = p.tx.send(Err(Error::WebSocket(err.to_string())));
    }
}
