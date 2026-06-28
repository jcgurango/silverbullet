//! Version history for markdown files: content-addressed object store,
//! commit log, 3-way merge. Ported from the pre-Rust Go server's
//! `history.go` / `merge.go` / `history_prune.go`.

pub mod merge;
pub mod prune;
pub mod store;
pub mod versioned_write;

pub use store::{sha256_hex, Commit, HistoryError, HistoryStore};
pub use versioned_write::{VersionedWriteError, VersionedWriteResult};
