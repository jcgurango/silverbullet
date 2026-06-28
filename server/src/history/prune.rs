//! Background pruning of old commits and unreferenced objects.
//!
//! Ported from `history_prune.go`. The Go goroutine maps to a tokio task that
//! sleeps with `tokio::time::interval` and runs each prune pass on the blocking
//! pool so it never starves async workers.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use walkdir::WalkDir;

use super::store::HistoryStore;

impl HistoryStore {
    /// Spawn a background task that calls [`Self::prune_all`] every `interval`
    /// after an initial 30-second delay (mirrors Go's `StartPruner`). The task
    /// is detached; dropping the handle does not stop it.
    pub fn start_pruner(self: Arc<Self>, max_age: Duration, interval: Duration) {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate-fire tick semantics of `interval`; the
            // 30-second sleep above already served that role.
            ticker.tick().await;
            loop {
                let store = self.clone();
                let _ = tokio::task::spawn_blocking(move || store.prune_all(max_age)).await;
                ticker.tick().await;
            }
        });
    }

    /// Walk the `.history` tree and prune each file's history.
    pub fn prune_all(&self, max_age: Duration) {
        let history_root = self.root_path().join(".history");
        if !history_root.exists() {
            return;
        }
        for entry in WalkDir::new(&history_root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_name() != "commits.jsonl" {
                continue;
            }
            let Some(dir) = entry.path().parent() else {
                continue;
            };
            let Ok(rel) = dir.strip_prefix(&history_root) else {
                continue;
            };
            let rel = rel.to_string_lossy();
            // The history dir is `<path>.versions`; recover `<path>`.
            let Some(file_path) = rel.strip_suffix(".versions") else {
                continue;
            };
            if let Err(e) = self.prune(file_path, max_age) {
                tracing::warn!("failed to prune history for {file_path}: {e}");
            }
        }
    }

    /// Remove commits older than `max_age` for a single file, plus any orphaned
    /// content objects. Always keeps at least HEAD.
    pub fn prune(&self, path: &str, max_age: Duration) -> Result<(), super::store::HistoryError> {
        // Re-grab data we need under the read lock, then take the write lock to
        // rewrite. We can't easily expose the lock from outside the impl, so we
        // route through the public methods.
        let commits = self.get_commits(path)?;
        if commits.len() <= 1 {
            return Ok(());
        }

        let cutoff_ms = chrono::Utc::now().timestamp_millis() - max_age.as_millis() as i64;
        let head_commit = commits.last().unwrap().clone();
        let mut referenced: HashSet<String> = HashSet::new();
        referenced.insert(head_commit.hash.clone());

        let mut kept: Vec<super::store::Commit> = Vec::new();
        for c in &commits {
            if c.timestamp >= cutoff_ms || c.hash == head_commit.hash {
                kept.push(c.clone());
                referenced.insert(c.hash.clone());
                if !c.parent.is_empty() {
                    referenced.insert(c.parent.clone());
                }
            }
        }

        if kept.len() == commits.len() {
            return Ok(());
        }

        // Fix dangling parent references on kept commits whose parent was
        // pruned (mirrors Go behavior: clear `.parent` to "").
        let kept_hashes: HashSet<String> = kept.iter().map(|c| c.hash.clone()).collect();
        for c in kept.iter_mut() {
            if !c.parent.is_empty() && !kept_hashes.contains(&c.parent) {
                c.parent.clear();
            }
        }

        // Rewrite commits.jsonl.
        let history_dir = self
            .root_path()
            .join(".history")
            .join(format!("{path}.versions"));
        let log_file = history_dir.join("commits.jsonl");
        let mut buf: Vec<u8> = Vec::new();
        for c in &kept {
            let mut line = serde_json::to_vec(c)?;
            line.push(b'\n');
            buf.write_all(&line)?;
        }
        fs::write(&log_file, &buf)?;

        // GC unreferenced object files.
        let objects_dir = history_dir.join("objects");
        if objects_dir.exists() {
            for entry in WalkDir::new(&objects_dir)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_file() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !referenced.contains(&name) {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            // Remove empty fan-out directories.
            remove_empty_subdirs(&objects_dir);
        }

        tracing::info!(
            "pruned history for {path}: {} -> {} commits",
            commits.len(),
            kept.len()
        );
        Ok(())
    }
}

fn remove_empty_subdirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(mut inner) = fs::read_dir(&path) {
            if inner.next().is_none() {
                let _ = fs::remove_dir(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;
    use tempfile::TempDir;

    fn store_with_zero_throttle(dir: &Path) -> HistoryStore {
        // Throttle off so we can lay down many commits quickly.
        HistoryStore::new(dir, Duration::from_secs(0))
    }

    #[test]
    fn prune_keeps_recent_commits_and_head() {
        let td = TempDir::new().unwrap();
        let store = store_with_zero_throttle(td.path());

        store.ensure_initialized("p.md", b"v1").unwrap();
        let h1 = store.get_head("p.md").unwrap();
        let (_c, _) = store.record_commit("p.md", b"v2", &h1, "client").unwrap();
        let h2 = store.get_head("p.md").unwrap();
        let (_c, _) = store.record_commit("p.md", b"v3", &h2, "client").unwrap();

        // Backdate v1 + v2 to the past so they fall outside the prune window.
        backdate_commits(&store, "p.md", &[(0, -10_000), (1, -10_000)]);

        store.prune("p.md", Duration::from_secs(1)).unwrap();
        let commits = store.get_commits("p.md").unwrap();
        // v3 must survive (it's HEAD); v2 may survive if it's still referenced
        // by HEAD's parent. The contract is "HEAD always kept".
        assert!(commits
            .iter()
            .any(|c| c.hash == store.get_head("p.md").unwrap()));
        assert!(commits.len() < 3);
    }

    #[test]
    fn prune_is_noop_for_single_commit_files() {
        let td = TempDir::new().unwrap();
        let store = HistoryStore::new(td.path(), Duration::from_secs(0));
        store.ensure_initialized("p.md", b"v1").unwrap();
        store.prune("p.md", Duration::from_secs(0)).unwrap();
        assert_eq!(store.get_commits("p.md").unwrap().len(), 1);
    }

    #[test]
    fn prune_all_walks_every_file() {
        let td = TempDir::new().unwrap();
        let store = HistoryStore::new(td.path(), Duration::from_secs(0));
        store.ensure_initialized("a.md", b"a").unwrap();
        store.ensure_initialized("nested/b.md", b"b").unwrap();
        store.prune_all(Duration::from_secs(3600));
        // Single commits are no-ops; this just confirms walking doesn't error.
        assert_eq!(store.get_commits("a.md").unwrap().len(), 1);
        assert_eq!(store.get_commits("nested/b.md").unwrap().len(), 1);
    }

    /// Rewrite `commits.jsonl` shifting selected commits' timestamps by
    /// `delta_ms` (negative = into the past).
    fn backdate_commits(store: &HistoryStore, path: &str, edits: &[(usize, i64)]) {
        let mut commits = store.get_commits(path).unwrap();
        for (idx, delta) in edits {
            commits[*idx].timestamp += delta;
        }
        let mut buf: Vec<u8> = Vec::new();
        for c in &commits {
            buf.extend(serde_json::to_vec(c).unwrap());
            buf.push(b'\n');
        }
        let log = store
            .root_path()
            .join(".history")
            .join(format!("{path}.versions"))
            .join("commits.jsonl");
        fs::write(log, buf).unwrap();
    }
}
