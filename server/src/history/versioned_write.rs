//! Versioned-write entry point for `.md` files: routes between fast-forward,
//! brand-new-file initialization, clean 3-way merge, and 409-conflict outcomes.
//!
//! Ported from the Go `handleVersionedWrite` in `fs.go`. Pure synchronous
//! function — callers wrap it in `spawn_blocking` from the async handler.

use silverbullet_server_common::{FileMeta, SpaceError, SpacePrimitives};

use super::merge::three_way_merge;
use super::store::{sha256_hex, HistoryError, HistoryStore};

/// Result of a versioned write attempt.
pub enum VersionedWriteResult {
    /// Write applied cleanly (or was a no-op). `hash` is the new HEAD.
    Ok { hash: String, meta: FileMeta },
    /// Merge produced conflict markers. The body is the conflict-marked
    /// content; the server side has NOT been written to disk or recorded.
    /// `hash` is the unchanged server HEAD.
    Conflict {
        hash: String,
        meta: FileMeta,
        merged: Vec<u8>,
    },
}

/// Errors a versioned write can surface.
#[derive(Debug, thiserror::Error)]
pub enum VersionedWriteError {
    #[error(transparent)]
    Space(#[from] SpaceError),
    #[error(transparent)]
    History(#[from] HistoryError),
}

/// Process a write to `path` with `content`, given the client's claimed parent
/// hash (empty string = legacy/no parent).
///
/// Branching mirrors the Go reference exactly:
///   1. Reconcile external disk edits before doing anything else.
///   2. No history yet → init + write.
///   3. No parent hash → treat as fast-forward.
///   4. Parent matches HEAD → fast-forward (or no-op when content matches).
///   5. Diverged → walk to LCA, run 3-way merge. Conflict bubbles up as
///      `Conflict`; clean merge writes the result and records a commit.
pub fn handle(
    space: &dyn SpacePrimitives,
    history: &HistoryStore,
    path: &str,
    content: &[u8],
    parent_hash: &str,
) -> Result<VersionedWriteResult, VersionedWriteError> {
    // Reconcile disk state first so external edits become a commit before our
    // write either lands or has to merge.
    let read_result = space.read_file(path);
    let current_data: Option<Vec<u8>> = match &read_result {
        Ok((data, _)) => {
            if let Err(e) = history.reconcile_disk_state(path, data) {
                tracing::warn!("failed to reconcile disk state for {path}: {e}");
            }
            Some(data.clone())
        }
        Err(SpaceError::NotFound) => None,
        Err(e) => {
            tracing::warn!("failed to read {path} before versioned write: {e}");
            None
        }
    };

    let mut current_head = history.get_head(path)?;

    // No history yet.
    if current_head.is_empty() {
        if let Some(existing) = current_data {
            // File exists on disk without history — initialize from disk first
            // so we have a base for the merge below.
            history.ensure_initialized(path, &existing)?;
            current_head = history.get_head(path)?;
        } else {
            // Brand-new file: write through, initialize from the just-written
            // content, return its hash.
            let meta = space.write_file(path, content, None)?;
            history.ensure_initialized(path, content)?;
            let head = history.get_head(path)?;
            return Ok(VersionedWriteResult::Ok { hash: head, meta });
        }
    }

    // Legacy client (no parent hash) — treat as fast-forward over HEAD.
    if parent_hash.is_empty() {
        let meta = space.write_file(path, content, None)?;
        let (commit, _throttled) = history.record_commit(path, content, &current_head, "client")?;
        return Ok(VersionedWriteResult::Ok {
            hash: commit.hash,
            meta,
        });
    }

    // Fast-forward path: parent matches HEAD.
    if parent_hash == current_head {
        let content_hash = sha256_hex(content);
        if content_hash == current_head {
            // No-op: content already on disk + HEAD.
            let meta = space.get_file_meta(path)?;
            return Ok(VersionedWriteResult::Ok {
                hash: current_head,
                meta,
            });
        }
        let meta = space.write_file(path, content, None)?;
        let (commit, _throttled) = history.record_commit(path, content, &current_head, "client")?;
        return Ok(VersionedWriteResult::Ok {
            hash: commit.hash,
            meta,
        });
    }

    // Diverged: 3-way merge.
    let lca = history
        .find_lca(path, parent_hash, &current_head)
        .unwrap_or_else(|_| parent_hash.to_string());

    let base_content = history.get_content(path, &lca)?;
    let head_content = history.get_content(path, &current_head)?;
    let (merged, has_conflict) = three_way_merge(&base_content, &head_content, content);

    if has_conflict {
        let meta = space.get_file_meta(path)?;
        return Ok(VersionedWriteResult::Conflict {
            hash: current_head,
            meta,
            merged,
        });
    }

    let meta = space.write_file(path, &merged, None)?;
    let (commit, _throttled) = history.record_commit(path, &merged, &current_head, "client")?;
    Ok(VersionedWriteResult::Ok {
        hash: commit.hash,
        meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryStore;
    use silverbullet_server_common::space::MemorySpacePrimitives;
    use std::time::Duration;
    use tempfile::TempDir;

    fn setup() -> (MemorySpacePrimitives, HistoryStore, TempDir) {
        let td = TempDir::new().unwrap();
        let store = HistoryStore::new(td.path(), Duration::from_secs(0));
        (MemorySpacePrimitives::new(), store, td)
    }

    #[test]
    fn brand_new_file_initializes_and_writes() {
        let (space, store, _td) = setup();
        let r = handle(&space, &store, "new.md", b"hello", "").unwrap();
        match r {
            VersionedWriteResult::Ok { hash, .. } => {
                assert_eq!(hash, sha256_hex(b"hello"));
            }
            _ => panic!("expected Ok"),
        }
        let (data, _) = space.read_file("new.md").unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn fast_forward_with_matching_parent_records_commit() {
        let (space, store, _td) = setup();
        handle(&space, &store, "p.md", b"v1", "").unwrap();
        let head_after_v1 = store.get_head("p.md").unwrap();

        let r = handle(&space, &store, "p.md", b"v2", &head_after_v1).unwrap();
        match r {
            VersionedWriteResult::Ok { hash, .. } => {
                assert_eq!(hash, sha256_hex(b"v2"));
            }
            _ => panic!("expected Ok"),
        }
        let commits = store.get_commits("p.md").unwrap();
        assert_eq!(commits.len(), 2);
    }

    #[test]
    fn no_op_when_content_matches_head() {
        let (space, store, _td) = setup();
        handle(&space, &store, "p.md", b"same", "").unwrap();
        let head = store.get_head("p.md").unwrap();
        let r = handle(&space, &store, "p.md", b"same", &head).unwrap();
        match r {
            VersionedWriteResult::Ok { hash, .. } => assert_eq!(hash, head),
            _ => panic!("expected Ok"),
        }
        assert_eq!(store.get_commits("p.md").unwrap().len(), 1);
    }

    #[test]
    fn diverged_clean_merge_produces_ok() {
        let (space, store, _td) = setup();
        // Base
        handle(&space, &store, "p.md", b"a\nb\nc\n", "").unwrap();
        let base_head = store.get_head("p.md").unwrap();

        // Server advances HEAD: change line 1.
        handle(&space, &store, "p.md", b"A\nb\nc\n", &base_head).unwrap();

        // Client commits against the old base, changing line 3 — non-overlapping.
        let r = handle(&space, &store, "p.md", b"a\nb\nC\n", &base_head).unwrap();
        match r {
            VersionedWriteResult::Ok { .. } => {}
            _ => panic!("expected Ok (clean merge)"),
        }
        let (data, _) = space.read_file("p.md").unwrap();
        assert_eq!(data, b"A\nb\nC\n");
    }

    #[test]
    fn diverged_conflict_returns_conflict_without_writing_disk() {
        let (space, store, _td) = setup();
        handle(&space, &store, "p.md", b"line1\nline2\nline3\n", "").unwrap();
        let base_head = store.get_head("p.md").unwrap();

        // Server change to line 2.
        handle(
            &space,
            &store,
            "p.md",
            b"line1\nSERVER\nline3\n",
            &base_head,
        )
        .unwrap();
        let server_head = store.get_head("p.md").unwrap();
        let (server_disk_before, _) = space.read_file("p.md").unwrap();

        // Client also changes line 2 — true conflict.
        let r = handle(
            &space,
            &store,
            "p.md",
            b"line1\nCLIENT\nline3\n",
            &base_head,
        )
        .unwrap();
        match r {
            VersionedWriteResult::Conflict { hash, merged, .. } => {
                assert_eq!(hash, server_head);
                let s = String::from_utf8(merged).unwrap();
                assert!(s.contains("<<<<<<< server"));
                assert!(s.contains("SERVER"));
                assert!(s.contains("CLIENT"));
                assert!(s.contains(">>>>>>> client"));
            }
            _ => panic!("expected Conflict"),
        }

        // Disk must not have changed past server head.
        let (server_disk_after, _) = space.read_file("p.md").unwrap();
        assert_eq!(server_disk_before, server_disk_after);
        // HEAD unchanged.
        assert_eq!(store.get_head("p.md").unwrap(), server_head);
    }

    #[test]
    fn legacy_client_without_parent_hash_fast_forwards() {
        let (space, store, _td) = setup();
        handle(&space, &store, "p.md", b"v1", "").unwrap();
        // Second call also has empty parent — must NOT 409.
        let r = handle(&space, &store, "p.md", b"v2", "").unwrap();
        match r {
            VersionedWriteResult::Ok { hash, .. } => {
                assert_eq!(hash, sha256_hex(b"v2"));
            }
            _ => panic!("expected Ok"),
        }
    }
}
