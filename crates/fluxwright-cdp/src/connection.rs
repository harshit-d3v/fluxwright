use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message};
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
        let (ws, _) = connect_async(ws_url)
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outgoing>();
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(16_384);
        let watchers: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<CdpEvent>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let dead = Arc::new(AtomicBool::new(false));

        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            tx: out_tx,
            pending: pending.clone(),
            events: events.clone(),
            watchers: watchers.clone(),
            dead: dead.clone(),
            dead_notify: tokio::sync::Notify::new(),
        });

        tokio::spawn(async move {
            while let Some(Outgoing::Json(s)) = out_rx.recv().await {
                if sink.send(Message::Text(s.into())).await.is_err() {
                    break;
                }
            }
        });

        let pending_r = pending;
        let dead_r = dead;
        let events_r = events;
        let watchers_r = watchers;
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Err(e) =
                            dispatch(&text, &pending_r, &events_r, &watchers_r).await
                        {
                            warn!(error = %e, "cdp dispatch");
                        }
                    }
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Frame(_)) => {}
                }
            }
            dead_r.store(true, Ordering::SeqCst);
            fail_all(&pending_r, Error::WebSocket("connection closed".into())).await;
            let ev = CdpEvent {
                method: "Fluxwright.disconnected".into(),
                params: json!({}),
                session_id: None,
            };
            let _ = events_r.send(ev.clone());
            fanout(&watchers_r, ev);
        });

        Ok(conn)
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
