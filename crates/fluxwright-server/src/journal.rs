//! Jobs that survive a restart: every accepted job is appended to a file, and a line marks it
//! done when it ends. On start, jobs with no done line are queued again under their old ids.
//! A job that was running when the server stopped runs again from its first step, so its side
//! effects (a click, a form) can happen twice.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) struct Journal {
    file: Mutex<File>,
}

/// Owner-only on Unix: requests hold URLs and typed values. Windows has no mode bits; the file
/// takes its folder's access list.
fn options() -> OpenOptions {
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o
}

/// The requests in `text` with no done line, oldest first. A line cut short by a crash, or
/// anything else unreadable, is skipped.
fn pending(text: &str) -> Vec<(Uuid, Value)> {
    let mut jobs: Vec<(Uuid, Value)> = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = entry["id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
            continue;
        };
        if entry["done"] == true {
            jobs.retain(|(job, _)| *job != id);
        } else if entry["request"].is_object() && jobs.iter().all(|(job, _)| *job != id) {
            jobs.push((id, entry["request"].clone()));
        }
    }
    jobs
}

impl Journal {
    /// Opens (or creates) the journal and returns the jobs to run again. The file is rewritten
    /// with just those, so it does not grow across restarts.
    pub fn open(path: &Path) -> std::io::Result<(Self, Vec<(Uuid, Value)>)> {
        let jobs = match std::fs::read(path) {
            Ok(bytes) => pending(&String::from_utf8_lossy(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        // Written beside it and renamed over it: a crash here leaves the old file whole.
        let tmp = path.with_extension("tmp");
        let mut file = options().write(true).create(true).truncate(true).open(&tmp)?;
        for (id, request) in &jobs {
            writeln!(file, "{}", json!({ "id": id, "request": request }))?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        let file = options().append(true).open(path)?;
        Ok((Self { file: Mutex::new(file) }, jobs))
    }

    pub fn submitted(&self, id: Uuid, request: &Value) {
        self.append(json!({ "id": id, "request": request }));
    }

    pub fn done(&self, id: Uuid) {
        self.append(json!({ "id": id, "done": true }));
    }

    /// shortcut: a blocking write and sync on the caller's thread, about a millisecond a job.
    /// Move it to a writer task if submission rates reach thousands a second.
    fn append(&self, entry: Value) {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        // One write per line, so a crash can only cut the last line short.
        let line = format!("{entry}\n");
        if let Err(e) = file.write_all(line.as_bytes()).and_then(|()| file.sync_data()) {
            // The job still runs; it is only not safe across a restart.
            tracing::warn!(error = %e, "could not write the job journal");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_keeps_unfinished_jobs_in_order_and_skips_broken_lines() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let text = format!(
            "{}\n{}\nnot json\n{}\n{}\n{{\"id\":\"{c}\",\"requ",
            json!({ "id": a, "request": { "n": 1 } }),
            json!({ "id": b, "request": { "n": 2 } }),
            json!({ "id": a, "done": true }),
            json!({ "id": c, "request": { "n": 3 } }),
        );
        let jobs = pending(&text);
        assert_eq!(jobs, vec![(b, json!({ "n": 2 })), (c, json!({ "n": 3 }))]);
    }

    #[test]
    fn reopening_returns_unfinished_jobs_and_compacts_the_file() {
        let path = std::env::temp_dir().join(format!("fluxwright-journal-{}.jsonl", Uuid::new_v4()));
        let (journal, jobs) = Journal::open(&path).unwrap();
        assert!(jobs.is_empty());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        journal.submitted(a, &json!({ "n": 1 }));
        journal.submitted(b, &json!({ "n": 2 }));
        journal.done(a);
        drop(journal);

        let (journal, jobs) = Journal::open(&path).unwrap();
        assert_eq!(jobs, vec![(b, json!({ "n": 2 }))]);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        journal.done(b);
        drop(journal);
        assert!(Journal::open(&path).unwrap().1.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
