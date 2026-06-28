//! Content-addressed version history for markdown files.
//!
//! Layout under the space root:
//! ```text
//! .history/<path>.versions/
//!   head                       # current HEAD hash (single hex line)
//!   commits.jsonl              # append-only commit log, one JSON Commit per line
//!   objects/<aa>/<aabbcc…>     # full content snapshots, sha256-keyed, 2-char fan-out
//! ```
//!
//! Direct port of the Go server's `history.go`. All I/O is synchronous; callers
//! must wrap calls in `tokio::task::spawn_blocking` when on an async runtime.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Single version in the commit chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commit {
    pub hash: String,
    pub parent: String,
    pub timestamp: i64,
    pub source: String, // "client" or "disk"
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("object not found: {0}")]
    ObjectNotFound(String),
    #[error("no common ancestor found for {0} and {1}")]
    NoCommonAncestor(String, String),
}

/// Compute sha256 of `data` as a lower-hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Per-file version-history store under a single space root.
pub struct HistoryStore {
    root_path: PathBuf,
    min_commit_interval: Duration,
    // Serializes all mutating ops; read-only ops take the read side. All work
    // is on the OS thread (spawn_blocking) so `std::sync::RwLock` is correct.
    lock: RwLock<()>,
}

impl HistoryStore {
    pub fn new(root_path: impl AsRef<Path>, min_commit_interval: Duration) -> Self {
        Self {
            root_path: root_path.as_ref().to_path_buf(),
            min_commit_interval,
            lock: RwLock::new(()),
        }
    }

    /// `<root>/.history/<path>.versions` — the per-file history directory.
    fn history_dir(&self, path: &str) -> PathBuf {
        self.root_path
            .join(".history")
            .join(format!("{path}.versions"))
    }

    /// Object path with 2-char fan-out: `objects/<aa>/<aabbcc…>`. Hashes
    /// shorter than 2 chars are stored flat under `objects/`.
    fn object_path(&self, path: &str, hash: &str) -> PathBuf {
        let dir = self.history_dir(path);
        if hash.len() < 2 {
            return dir.join("objects").join(hash);
        }
        dir.join("objects").join(&hash[..2]).join(hash)
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Return the current HEAD hash, or `""` when no history exists.
    pub fn get_head(&self, path: &str) -> Result<String, HistoryError> {
        let _g = self.lock.read().unwrap();
        self.read_head(path)
    }

    fn read_head(&self, path: &str) -> Result<String, HistoryError> {
        let head_file = self.history_dir(path).join("head");
        match fs::read_to_string(&head_file) {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(HistoryError::Io(e)),
        }
    }

    /// Read the full commit log for `path`. Returns an empty Vec when no log
    /// exists.
    pub fn get_commits(&self, path: &str) -> Result<Vec<Commit>, HistoryError> {
        let _g = self.lock.read().unwrap();
        self.read_commits(path)
    }

    fn read_commits(&self, path: &str) -> Result<Vec<Commit>, HistoryError> {
        let log_file = self.history_dir(path).join("commits.jsonl");
        let data = match fs::read_to_string(&log_file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let mut commits = Vec::new();
        for line in data.trim().split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            commits.push(serde_json::from_str::<Commit>(line)?);
        }
        Ok(commits)
    }

    /// Read the content blob for a given hash.
    pub fn get_content(&self, path: &str, hash: &str) -> Result<Vec<u8>, HistoryError> {
        let _g = self.lock.read().unwrap();
        self.read_content(path, hash)
    }

    fn read_content(&self, path: &str, hash: &str) -> Result<Vec<u8>, HistoryError> {
        let obj = self.object_path(path, hash);
        match fs::read(&obj) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(HistoryError::ObjectNotFound(hash.to_string()))
            }
            Err(e) => Err(HistoryError::Io(e)),
        }
    }

    /// Timestamp (ms) of the latest commit, or `0` when no history.
    pub fn get_last_commit_time(&self, path: &str) -> Result<i64, HistoryError> {
        let _g = self.lock.read().unwrap();
        let commits = self.read_commits(path)?;
        Ok(commits.last().map(|c| c.timestamp).unwrap_or(0))
    }

    /// Record a new commit. Returns the commit and whether the commit was
    /// throttled (the caller should still write the file to disk in that case).
    ///
    /// - If the content hash matches HEAD, dedups (no-op, not throttled).
    /// - For client fast-forwards within `min_commit_interval`, throttles.
    pub fn record_commit(
        &self,
        path: &str,
        content: &[u8],
        parent_hash: &str,
        source: &str,
    ) -> Result<(Commit, bool), HistoryError> {
        let _g = self.lock.write().unwrap();
        let content_hash = sha256_hex(content);
        let current_head = self.read_head(path)?;

        // Content matches HEAD: dedup.
        if content_hash == current_head {
            return Ok((
                Commit {
                    hash: content_hash,
                    parent: parent_hash.to_string(),
                    timestamp: now_millis(),
                    source: source.to_string(),
                },
                false,
            ));
        }

        // Throttle window for client fast-forwards.
        if source == "client" && parent_hash == current_head {
            let commits = self.read_commits(path).unwrap_or_default();
            if let Some(last) = commits.last() {
                let elapsed_ms = now_millis() - last.timestamp;
                if elapsed_ms >= 0 && (elapsed_ms as u128) < self.min_commit_interval.as_millis() {
                    return Ok((
                        Commit {
                            hash: content_hash,
                            parent: parent_hash.to_string(),
                            timestamp: now_millis(),
                            source: source.to_string(),
                        },
                        true,
                    ));
                }
            }
        }

        // Write the content object (content-addressable dedup).
        let obj_path = self.object_path(path, &content_hash);
        if let Some(parent) = obj_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !obj_path.exists() {
            fs::write(&obj_path, content)?;
        }

        // Append to commit log.
        let now = now_millis();
        let commit = Commit {
            hash: content_hash,
            parent: parent_hash.to_string(),
            timestamp: now,
            source: source.to_string(),
        };

        let log_file = self.history_dir(path).join("commits.jsonl");
        if let Some(parent) = log_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_file)?;
        let mut line = serde_json::to_vec(&commit)?;
        line.push(b'\n');
        f.write_all(&line)?;
        drop(f);

        // Update HEAD.
        let head_file = self.history_dir(path).join("head");
        fs::write(&head_file, format!("{}\n", commit.hash))?;

        Ok((commit, false))
    }

    /// Create the initial commit for a file when no history exists yet.
    /// Idempotent: returns Ok(()) when a HEAD already exists.
    pub fn ensure_initialized(&self, path: &str, content: &[u8]) -> Result<(), HistoryError> {
        let _g = self.lock.write().unwrap();
        let head_file = self.history_dir(path).join("head");
        if head_file.exists() {
            return Ok(());
        }
        let dir = self.history_dir(path);
        fs::create_dir_all(&dir)?;

        let content_hash = sha256_hex(content);
        let obj_path = self.object_path(path, &content_hash);
        if let Some(parent) = obj_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&obj_path, content)?;

        let commit = Commit {
            hash: content_hash.clone(),
            parent: String::new(),
            timestamp: now_millis(),
            source: "disk".to_string(),
        };
        let mut buf = serde_json::to_vec(&commit)?;
        buf.push(b'\n');
        fs::write(dir.join("commits.jsonl"), buf)?;
        fs::write(dir.join("head"), format!("{content_hash}\n"))?;
        Ok(())
    }

    /// Find the last common ancestor of two commits by walking both chains.
    pub fn find_lca(&self, path: &str, hash1: &str, hash2: &str) -> Result<String, HistoryError> {
        let _g = self.lock.read().unwrap();
        let commits = self.read_commits(path)?;
        let mut parent_map = std::collections::HashMap::with_capacity(commits.len());
        for c in &commits {
            parent_map.insert(c.hash.clone(), c.parent.clone());
        }

        let mut ancestors = std::collections::HashSet::new();
        let mut current = hash1.to_string();
        while !current.is_empty() {
            ancestors.insert(current.clone());
            match parent_map.get(&current) {
                Some(p) => current = p.clone(),
                None => break,
            }
        }

        let mut current = hash2.to_string();
        while !current.is_empty() {
            if ancestors.contains(&current) {
                return Ok(current);
            }
            match parent_map.get(&current) {
                Some(p) => current = p.clone(),
                None => break,
            }
        }
        Err(HistoryError::NoCommonAncestor(
            hash1.to_string(),
            hash2.to_string(),
        ))
    }

    /// Check whether the on-disk content matches HEAD; if not, auto-commit as
    /// `"disk"` source so external edits are captured. Initializes history if
    /// missing.
    pub fn reconcile_disk_state(
        &self,
        path: &str,
        disk_content: &[u8],
    ) -> Result<(), HistoryError> {
        let disk_hash = sha256_hex(disk_content);
        let head = self.get_head(path)?;
        if head.is_empty() {
            return self.ensure_initialized(path, disk_content);
        }
        if disk_hash != head {
            self.record_commit(path, disk_content, &head, "disk")?;
        }
        Ok(())
    }

    /// Remove all history for a file. Tolerant of missing directories.
    pub fn delete_history(&self, path: &str) -> Result<(), HistoryError> {
        let _g = self.lock.write().unwrap();
        let dir = self.history_dir(path);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(HistoryError::Io(e)),
        }
    }

    /// Test-only setter so the throttle-interval tests can change the window
    /// without re-constructing the store.
    #[cfg(test)]
    fn set_min_commit_interval(&mut self, d: Duration) {
        self.min_commit_interval = d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (HistoryStore, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let store = HistoryStore::new(dir.path(), Duration::from_secs(60));
        (store, dir)
    }

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn ensure_initialized_creates_head_and_commit() {
        let (h, _td) = setup();
        let content = b"# Hello\n\nThis is a test page.";
        h.ensure_initialized("test.md", content).unwrap();

        let head = h.get_head("test.md").unwrap();
        assert_eq!(head, sha256_hex(content));

        let stored = h.get_content("test.md", &head).unwrap();
        assert_eq!(stored, content);

        let commits = h.get_commits("test.md").unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, head);
        assert_eq!(commits[0].parent, "");
        assert_eq!(commits[0].source, "disk");

        // Re-init is a no-op even with different content.
        h.ensure_initialized("test.md", b"different content")
            .unwrap();
        let head2 = h.get_head("test.md").unwrap();
        assert_eq!(head, head2);
    }

    #[test]
    fn record_commit_fast_forward() {
        let (mut h, _td) = setup();
        h.ensure_initialized("page.md", b"initial").unwrap();
        let initial_hash = h.get_head("page.md").unwrap();
        h.set_min_commit_interval(Duration::ZERO);

        let (commit, throttled) = h
            .record_commit("page.md", b"updated", &initial_hash, "client")
            .unwrap();
        assert!(!throttled);
        assert_eq!(commit.hash, sha256_hex(b"updated"));
        assert_eq!(commit.parent, initial_hash);

        let head = h.get_head("page.md").unwrap();
        assert_eq!(head, commit.hash);

        let commits = h.get_commits("page.md").unwrap();
        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn record_commit_hash_dedup() {
        let (mut h, _td) = setup();
        h.set_min_commit_interval(Duration::ZERO);
        h.ensure_initialized("page.md", b"same content").unwrap();

        let head = h.get_head("page.md").unwrap();
        let (commit, throttled) = h
            .record_commit("page.md", b"same content", &head, "client")
            .unwrap();
        assert!(!throttled);
        assert_eq!(commit.hash, head);

        let commits = h.get_commits("page.md").unwrap();
        assert_eq!(commits.len(), 1);
    }

    #[test]
    fn record_commit_throttling() {
        let (mut h, _td) = setup();
        h.set_min_commit_interval(Duration::from_secs(3600));

        h.ensure_initialized("page.md", b"initial").unwrap();
        let head = h.get_head("page.md").unwrap();

        let (_c, throttled) = h
            .record_commit("page.md", b"updated", &head, "client")
            .unwrap();
        assert!(throttled);

        let head2 = h.get_head("page.md").unwrap();
        assert_eq!(head, head2, "HEAD should not change on throttle");

        let (_c, throttled) = h
            .record_commit("page.md", b"disk edit", &head, "disk")
            .unwrap();
        assert!(!throttled, "disk source must not be throttled");

        let head3 = h.get_head("page.md").unwrap();
        assert_eq!(head3, sha256_hex(b"disk edit"));
    }

    #[test]
    fn find_lca_with_branched_history() {
        let (mut h, _td) = setup();
        h.set_min_commit_interval(Duration::ZERO);

        h.ensure_initialized("page.md", b"A").unwrap();
        let hash_a = h.get_head("page.md").unwrap();

        h.record_commit("page.md", b"B", &hash_a, "client").unwrap();
        let hash_b = h.get_head("page.md").unwrap();

        h.record_commit("page.md", b"C", &hash_b, "client").unwrap();
        let hash_c = h.get_head("page.md").unwrap();

        // Side-branch D from B — still gets appended to the log, parent=B.
        h.record_commit("page.md", b"D", &hash_b, "disk").unwrap();
        let hash_d = h.get_head("page.md").unwrap();

        let lca = h.find_lca("page.md", &hash_c, &hash_d).unwrap();
        assert_eq!(lca, hash_b);

        let lca = h.find_lca("page.md", &hash_a, &hash_c).unwrap();
        assert_eq!(lca, hash_a);
    }

    #[test]
    fn reconcile_disk_state_initializes_then_tracks_external_edits() {
        let (mut h, _td) = setup();
        h.set_min_commit_interval(Duration::ZERO);

        h.reconcile_disk_state("page.md", b"initial").unwrap();
        let head = h.get_head("page.md").unwrap();
        assert_eq!(head, sha256_hex(b"initial"));

        h.reconcile_disk_state("page.md", b"initial").unwrap();
        let commits = h.get_commits("page.md").unwrap();
        assert_eq!(commits.len(), 1);

        h.reconcile_disk_state("page.md", b"modified externally")
            .unwrap();
        let commits = h.get_commits("page.md").unwrap();
        assert_eq!(commits.len(), 2);
        let head = h.get_head("page.md").unwrap();
        assert_eq!(head, sha256_hex(b"modified externally"));
    }

    #[test]
    fn delete_history_clears_head() {
        let (h, _td) = setup();
        h.ensure_initialized("page.md", b"content").unwrap();
        h.delete_history("page.md").unwrap();
        let head = h.get_head("page.md").unwrap();
        assert_eq!(head, "");
    }

    #[test]
    fn nested_paths_build_correct_directory_layout() {
        let (h, _td) = setup();
        h.ensure_initialized("journal/2024/01/01.md", b"journal entry")
            .unwrap();

        let head = h.get_head("journal/2024/01/01.md").unwrap();
        assert!(!head.is_empty());
        let content = h.get_content("journal/2024/01/01.md", &head).unwrap();
        assert_eq!(content, b"journal entry");

        let dir = h.history_dir("journal/2024/01/01.md");
        assert!(dir.is_absolute());
        assert!(dir.exists());
    }
}
