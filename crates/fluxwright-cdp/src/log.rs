//! Console messages and uncaught errors from a page, its popups and its iframes, kept so a job
//! can check them (Playwright's `page.consoleMessages()` and `page.pageErrors()`).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::connection::CdpEvent;

/// Per kind; older entries are dropped first.
const KEEP: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConsoleMessage {
    /// `log`, `error`, `warning`, `info`, `debug`, ..., as Playwright's `type()` reports them.
    pub kind: String,
    pub text: String,
}

/// An exception nothing caught.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageError {
    /// `TypeError`, `Error`, ...; empty when something other than an Error was thrown.
    pub name: String,
    pub message: String,
    pub stack: String,
}

#[derive(Debug, Default)]
pub struct PageLog {
    pub console: VecDeque<ConsoleMessage>,
    pub errors: VecDeque<PageError>,
}

fn keep<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(v);
}

/// How a console argument reads: strings as they are, other values as Chrome describes them.
fn arg_text(o: &Value) -> String {
    match o.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => o["unserializableValue"]
            .as_str()
            .or_else(|| o["description"].as_str())
            .or_else(|| o["type"].as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn page_error(details: &Value) -> PageError {
    let ex = &details["exception"];
    let stack = ex["description"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| ex.get("value").map(|_| arg_text(ex)))
        .unwrap_or_else(|| details["text"].as_str().unwrap_or("Uncaught").to_string());
    if ex["subtype"] != "error" {
        return PageError { name: String::new(), message: stack.clone(), stack };
    }
    // "TypeError: x is not a function\n    at ...": the first line is "name: message".
    let first = stack.lines().next().unwrap_or("").to_string();
    let (name, message) = match first.split_once(": ") {
        Some((n, m)) => (n.to_string(), m.to_string()),
        None => (first, String::new()),
    };
    PageError { name, message, stack }
}

/// Runs until aborted: records console calls and uncaught exceptions from `first_session` and
/// every session that later attaches in `context` (popups, iframes in other processes).
pub(crate) async fn collect(
    mut events: UnboundedReceiver<CdpEvent>,
    first_session: String,
    context: String,
    log: Arc<Mutex<PageLog>>,
) {
    let mut sessions = HashSet::from([first_session]);
    while let Some(e) = events.recv().await {
        let ours = e.session_id.as_ref().is_some_and(|s| sessions.contains(s));
        match e.method.as_str() {
            "Target.attachedToTarget" if e.params["targetInfo"]["browserContextId"] == context.as_str() => {
                if let Some(s) = e.params["sessionId"].as_str() {
                    sessions.insert(s.to_string());
                }
            }
            "Runtime.consoleAPICalled" if ours => {
                let text = e.params["args"].as_array().map(|a| a.iter().map(arg_text).collect::<Vec<_>>().join(" "));
                let msg = ConsoleMessage {
                    kind: e.params["type"].as_str().unwrap_or("log").to_string(),
                    text: text.unwrap_or_default(),
                };
                keep(&mut log.lock().unwrap().console, msg);
            }
            "Runtime.exceptionThrown" if ours => {
                keep(&mut log.lock().unwrap().errors, page_error(&e.params["exceptionDetails"]));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn console_arguments_read_like_the_devtools_console() {
        let args = [
            json!({ "type": "string", "value": "count" }),
            json!({ "type": "number", "value": 3 }),
            json!({ "type": "object", "subtype": "null", "value": null }),
            json!({ "type": "undefined" }),
            json!({ "type": "object", "description": "Object" }),
        ];
        let text: Vec<String> = args.iter().map(arg_text).collect();
        assert_eq!(text.join(" "), "count 3 null undefined Object");
    }

    #[test]
    fn errors_split_into_name_and_message() {
        let e = page_error(&json!({ "exception": { "subtype": "error",
            "description": "TypeError: x is not a function\n    at <anonymous>:1:1" } }));
        assert_eq!((e.name.as_str(), e.message.as_str()), ("TypeError", "x is not a function"));
        let thrown = page_error(&json!({ "exception": { "type": "string", "value": "boom" } }));
        assert_eq!((thrown.name.as_str(), thrown.message.as_str()), ("", "boom"));
    }
}
