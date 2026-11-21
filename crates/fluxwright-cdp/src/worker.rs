//! Dedicated Web Worker tracking and evaluation for a page.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::log::ConsoleMessage;
use crate::page::js_error;

struct WorkerInner {
    target_id: String,
    session_id: String,
    url: String,
    conn: Arc<Connection>,
    browser_pid: u32,
    closed: Arc<AtomicBool>,
    close_tx: watch::Sender<bool>,
}

/// A dedicated Web Worker attached to a page.
#[derive(Clone)]
pub struct Worker(Arc<WorkerInner>);

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("target_id", &self.0.target_id)
            .field("url", &self.0.url)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Worker {
    pub(crate) fn new(
        target_id: String,
        session_id: String,
        url: String,
        conn: Arc<Connection>,
        browser_pid: u32,
    ) -> Self {
        let (close_tx, _) = watch::channel(false);
        Self(Arc::new(WorkerInner {
            target_id,
            session_id,
            url,
            conn,
            browser_pid,
            closed: Arc::new(AtomicBool::new(false)),
            close_tx,
        }))
    }

    pub fn target_id(&self) -> &str {
        &self.0.target_id
    }

    pub fn url(&self) -> &str {
        &self.0.url
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.0.session_id
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::SeqCst) || self.0.conn.is_dead()
    }

    pub fn subscribe_close(&self) -> watch::Receiver<bool> {
        self.0.close_tx.subscribe()
    }

    pub(crate) fn mark_closed(&self) {
        if !self.0.closed.swap(true, Ordering::SeqCst) {
            let _ = self.0.close_tx.send(true);
        }
    }

    /// Evaluates JavaScript in the worker execution context and returns its JSON value.
    pub async fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value> {
        if self.0.closed.load(Ordering::SeqCst) {
            return Err(Error::Other("worker is closed".into()));
        }
        if self.0.conn.is_dead() {
            return Err(Error::BrowserDead {
                pid: self.0.browser_pid,
            });
        }
        let res = self
            .0
            .conn
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
                Some(&self.0.session_id),
                timeout,
            )
            .await?;
        match js_error(&res) {
            Some(e) => Err(e),
            None => Ok(res["result"]["value"].clone()),
        }
    }
}

/// Worker lifecycle or console event emitted for a page.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Created(Worker),
    Closed {
        target_id: String,
        url: String,
    },
    Console {
        target_id: String,
        message: ConsoleMessage,
    },
}

#[derive(Default)]
pub(crate) struct WorkerRegistry {
    live: Vec<Worker>,
    subscribers: Vec<mpsc::UnboundedSender<WorkerEvent>>,
}

impl WorkerRegistry {
    pub(crate) fn list(&self) -> Vec<Worker> {
        self.live.clone()
    }

    pub(crate) fn get(&self, target_id: &str) -> Option<Worker> {
        self.live.iter().find(|w| w.target_id() == target_id).cloned()
    }

    pub(crate) fn subscribe(&mut self) -> mpsc::UnboundedReceiver<WorkerEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.push(tx);
        rx
    }

    pub(crate) fn has_subscribers(&self) -> bool {
        !self.subscribers.is_empty()
    }

    fn emit(&mut self, event: WorkerEvent) {
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }

    pub(crate) fn add(&mut self, worker: Worker) {
        if let Some(idx) = self.live.iter().position(|w| w.target_id() == worker.target_id()) {
            let old = self.live.remove(idx);
            old.mark_closed();
        }
        self.live.push(worker.clone());
        self.emit(WorkerEvent::Created(worker));
    }

    pub(crate) fn remove_by_target(&mut self, target_id: &str) {
        if let Some(idx) = self.live.iter().position(|w| w.target_id() == target_id) {
            let w = self.live.remove(idx);
            let tid = w.target_id().to_string();
            let url = w.url().to_string();
            w.mark_closed();
            self.emit(WorkerEvent::Closed { target_id: tid, url });
        }
    }

    pub(crate) fn remove_by_session(&mut self, session_id: &str) {
        if let Some(idx) = self.live.iter().position(|w| w.session_id() == session_id) {
            let w = self.live.remove(idx);
            let tid = w.target_id().to_string();
            let url = w.url().to_string();
            w.mark_closed();
            self.emit(WorkerEvent::Closed { target_id: tid, url });
        }
    }

    pub(crate) fn emit_console(&mut self, target_id: String, message: ConsoleMessage) {
        self.emit(WorkerEvent::Console { target_id, message });
    }

    pub(crate) fn close_all(&mut self) {
        let drained: Vec<_> = self.live.drain(..).collect();
        for w in drained {
            let tid = w.target_id().to_string();
            let url = w.url().to_string();
            w.mark_closed();
            self.emit(WorkerEvent::Closed { target_id: tid, url });
        }
    }
}
