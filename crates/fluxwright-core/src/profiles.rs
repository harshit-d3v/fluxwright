//! Named persistent browser profile storage (`storageState` cookies + localStorage + IndexedDB).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fluxwright_cdp::StorageState;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

const MAX_PROFILE_NAME_LEN: usize = 64;

/// Validates a profile identifier (`1..=64` ASCII `[A-Za-z0-9_-]`), rejecting path separators,
/// traversal segments, whitespace, and non-ASCII characters.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_PROFILE_NAME_LEN {
        return Err(Error::Other(format!(
            "profile name must be 1-{MAX_PROFILE_NAME_LEN} ASCII characters ([A-Za-z0-9_-])"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(Error::Other(
            "profile name may only contain ASCII letters, digits, '_' and '-'".into(),
        ));
    }
    Ok(())
}

/// Summary metadata for a stored named profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileInfo {
    pub name: String,
    pub updated_at_ms: u64,
    pub cookies: usize,
    pub origins: usize,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredProfile {
    updated_at_ms: u64,
    storage_state: StorageState,
}

/// Atomic, owner-restricted JSON store for named [`StorageState`] profiles.
pub struct ProfileStore {
    dir: PathBuf,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl ProfileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn lock_for(&self, name: &str) -> Arc<Mutex<()>> {
        let mut guard = self.locks.lock().unwrap();
        if !guard.contains_key(name) && guard.len() >= 4096 {
            guard.retain(|_, v| Arc::strong_count(v) > 1);
        }
        guard
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn profile_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    /// Loads `storageState` for `name`, or `Ok(None)` when no profile exists yet.
    pub fn load(&self, name: &str) -> Result<Option<StorageState>> {
        validate_profile_name(name)?;
        let lock = self.lock_for(name);
        let _guard = lock.lock().unwrap();
        let path = self.profile_path(name);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Other(format!("read profile {name}: {e}"))),
        };
        let stored: StoredProfile = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Other(format!("corrupt profile {name}: {e}")))?;
        Ok(Some(stored.storage_state))
    }

    /// Saves `state` atomically under `name`, creating the store directory if missing.
    pub fn save(&self, name: &str, state: &StorageState) -> Result<ProfileInfo> {
        validate_profile_name(name)?;
        let lock = self.lock_for(name);
        let _guard = lock.lock().unwrap();

        ensure_private_dir(&self.dir)?;

        let updated_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let payload = StoredProfile {
            updated_at_ms,
            storage_state: state.clone(),
        };
        let bytes = serde_json::to_vec(&payload)
            .map_err(|e| Error::Other(format!("encode profile {name}: {e}")))?;

        let dest = self.profile_path(name);
        let tmp = self.dir.join(format!("{name}.json.{}.tmp", Uuid::new_v4().simple()));
        if let Err(e) = write_private_atomic(&tmp, &dest, &bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Other(format!("save profile {name}: {e}")));
        }

        Ok(ProfileInfo {
            name: name.to_string(),
            updated_at_ms,
            cookies: state.cookies.len(),
            origins: state.origins.len(),
            size_bytes: bytes.len() as u64,
        })
    }

    /// Lists all valid profiles on disk, sorted newest first.
    pub fn list(&self) -> Result<Vec<ProfileInfo>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Other(format!("list profiles: {e}"))),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_profile_name(stem).is_err() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(stored) = serde_json::from_slice::<StoredProfile>(&bytes) else {
                continue;
            };
            out.push(ProfileInfo {
                name: stem.to_string(),
                updated_at_ms: stored.updated_at_ms,
                cookies: stored.storage_state.cookies.len(),
                origins: stored.storage_state.origins.len(),
                size_bytes: bytes.len() as u64,
            });
        }
        out.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    /// Removes `name` from disk; returns `false` if it did not exist.
    pub fn delete(&self, name: &str) -> Result<bool> {
        validate_profile_name(name)?;
        let lock = self.lock_for(name);
        let _guard = lock.lock().unwrap();
        let path = self.profile_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Other(format!("delete profile {name}: {e}"))),
        }
    }
}

fn ensure_private_dir(dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| Error::Other(format!("create profiles dir: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Other(format!("create profiles dir: {e}")))?;
    }
    Ok(())
}

fn write_private_atomic(tmp: &std::path::Path, dest: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxwright_cdp::{Cookie, OriginStorage, StorageItem};

    #[test]
    fn validates_profile_names_strictly() {
        for ok in ["user-1", "acct_prod", "A", &"x".repeat(64)] {
            assert!(validate_profile_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../secret", "a/b", "a\\b", "has space", "é", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn save_load_overwrite_list_and_delete_round_trip() {
        let root = std::env::temp_dir().join(format!("fluxwright-prof-test-{}", Uuid::new_v4()));
        let store = ProfileStore::new(root.join("profiles"));

        assert_eq!(store.list().unwrap().len(), 0);
        assert!(store.load("alice").unwrap().is_none());
        assert!(!store.delete("alice").unwrap());

        let s1 = StorageState {
            cookies: vec![Cookie {
                name: "sid".into(),
                value: "v1".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires: -1.0,
                http_only: true,
                secure: true,
                same_site: "Lax".into(),
                partition_key: None,
                cross_site_ancestor: None,
            }],
            origins: vec![OriginStorage {
                origin: "https://example.com".into(),
                local_storage: vec![StorageItem {
                    name: "theme".into(),
                    value: "dark".into(),
                }],
                indexed_db: None,
            }],
        };

        let info1 = store.save("alice", &s1).unwrap();
        assert_eq!(info1.name, "alice");
        assert_eq!(info1.cookies, 1);
        assert_eq!(info1.origins, 1);
        assert!(info1.size_bytes > 0);
        assert_eq!(store.load("alice").unwrap().as_ref(), Some(&s1));

        let mut s2 = s1.clone();
        s2.cookies[0].value = "v2".into();
        store.save("alice", &s2).unwrap();
        assert_eq!(store.load("alice").unwrap().unwrap().cookies[0].value, "v2");

        store.save("bob", &StorageState::default()).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);

        assert!(store.delete("alice").unwrap());
        assert!(store.load("alice").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
