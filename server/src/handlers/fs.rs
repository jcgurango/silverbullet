use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use silverbullet_server_common::FileMeta;

use crate::handlers::{http_date, space_error_response};
use crate::history::{sha256_hex, versioned_write, VersionedWriteError, VersionedWriteResult};
use crate::router::run_blocking;
use crate::state::ServerState;

/// Run `f` on the blocking pool. Mirrors `run_blocking` but with a custom error.
async fn run_blocking_versioned<F>(f: F) -> Result<VersionedWriteResult, VersionedWriteError>
where
    F: FnOnce() -> Result<VersionedWriteResult, VersionedWriteError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(join_err) => {
            tracing::error!("versioned write join error: {join_err}");
            Err(VersionedWriteError::Space(
                silverbullet_server_common::SpaceError::Io(std::io::Error::other(format!(
                    "blocking task join error: {join_err}"
                ))),
            ))
        }
    }
}

pub async fn handle_fs_list(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let state_inner = state.clone();
    match run_blocking(move || state_inner.space.fetch_file_list()).await {
        Ok(files) => {
            let json = serde_json::to_string(&files).unwrap_or_else(|e| {
                tracing::error!("failed to serialize file list: {e}");
                "[]".to_string()
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .header("X-Space-Path", &state.space_folder_path)
                .header("Cache-Control", "no-cache")
                .body(Body::from(json))
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(e.to_string()))
            .unwrap(),
    }
}

pub async fn handle_fs_get(
    State(state): State<Arc<ServerState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Metadata-only probe.
    if headers.get("X-Get-Meta").is_some() {
        let is_versioned_md = is_md(&path) && state.history.is_some();
        let state_inner = state.clone();
        let path_inner = path.clone();
        return match run_blocking(move || state_inner.space.get_file_meta(&path_inner)).await {
            Ok(meta) => {
                let mut builder =
                    set_file_meta_headers(Response::builder().status(StatusCode::OK), &meta)
                        .header("Cache-Control", "no-store");
                if is_versioned_md {
                    // Reconcile + emit content hash so the sync engine can use it as parent hash.
                    if let Some(hash) = reconcile_and_hash(&state, &path).await {
                        builder = builder.header("X-Content-Hash", hash);
                    }
                }
                builder.body(Body::empty()).unwrap()
            }
            Err(e) => space_error_response(e),
        };
    }

    // Conditional request: `/.fs` serves mutable files with `Cache-Control:
    // no-cache`, so the browser revalidates on every load. We emit a standard
    // `Last-Modified` validator (see `set_file_meta_headers`) which the browser
    // echoes back verbatim in `If-Modified-Since`; a string match means the file
    // is unchanged and we can answer 304 without reading the (potentially large)
    // body off disk — we only need a metadata-only `get_file_meta` probe here.
    let if_modified_since = headers
        .get(axum::http::header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(ims) = if_modified_since {
        let state_inner = state.clone();
        let path_inner = path.clone();
        if let Ok(meta) = run_blocking(move || state_inner.space.get_file_meta(&path_inner)).await {
            let last_modified = http_date(meta.last_modified);
            if !last_modified.is_empty() && ims == last_modified {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(axum::http::header::LAST_MODIFIED, &last_modified)
                    .header(axum::http::header::VARY, "Accept")
                    .body(Body::empty())
                    .unwrap();
            }
        }
    }

    let force_octet_stream = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/octet-stream"))
        .unwrap_or(false);

    let is_versioned_md = is_md(&path) && state.history.is_some();
    let state_inner = state.clone();
    let path_inner = path.clone();
    match run_blocking(move || state_inner.space.read_file(&path_inner)).await {
        Ok((data, mut meta)) => {
            let real_content_type = meta.content_type.clone();
            if force_octet_stream {
                meta.content_type = "application/octet-stream".to_string();
            }
            let mut builder =
                set_file_meta_headers(Response::builder().status(StatusCode::OK), &meta)
                    .header("X-Content-Type", &real_content_type)
                    // Mutable file: revalidate every load. The `Last-Modified`
                    // validator lets the browser get a 304 (handled above)
                    // instead of refetching the full body.
                    .header("Cache-Control", "no-cache")
                    // The body's `Content-Type` depends on the request's `Accept`
                    // (octet-stream vs the real type), so cache must key on it.
                    .header(axum::http::header::VARY, "Accept");
            let last_modified = http_date(meta.last_modified);
            if !last_modified.is_empty() {
                builder = builder.header(axum::http::header::LAST_MODIFIED, &last_modified);
            }
            if is_versioned_md {
                let hash = sha256_hex(&data);
                // Run reconcile off the hot path: we already have the body bytes here.
                let history = state.history.clone().unwrap();
                let path_for_reconcile = path.clone();
                let data_for_reconcile = data.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        history.reconcile_disk_state(&path_for_reconcile, &data_for_reconcile)
                    {
                        tracing::warn!("reconcile failed for {path_for_reconcile}: {e}");
                    }
                })
                .await;
                builder = builder.header("X-Content-Hash", hash);
            }
            builder.body(Body::from(data)).unwrap()
        }
        Err(e) => space_error_response(e),
    }
}

fn is_md(path: &str) -> bool {
    path.ends_with(".md")
}

/// Read the file fresh, reconcile its disk state into history, and return its
/// content hash. Returns `None` on any error (the caller falls back to omitting
/// the header).
async fn reconcile_and_hash(state: &Arc<ServerState>, path: &str) -> Option<String> {
    let history = state.history.clone()?;
    let state_inner = state.clone();
    let path_inner = path.to_string();
    let result = run_blocking(move || state_inner.space.read_file(&path_inner)).await;
    let (data, _) = result.ok()?;
    let path_for_reconcile = path.to_string();
    let data_for_reconcile = data.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = history.reconcile_disk_state(&path_for_reconcile, &data_for_reconcile) {
            tracing::warn!("reconcile failed for {path_for_reconcile}: {e}");
        }
    })
    .await;
    Some(sha256_hex(&data))
}

/// Set the `X-*` file-metadata headers the client reads off `/.fs` responses.
pub(crate) fn set_file_meta_headers(
    builder: axum::http::response::Builder,
    meta: &FileMeta,
) -> axum::http::response::Builder {
    builder
        .header("Content-Type", &meta.content_type)
        .header("X-Created", meta.created.to_string())
        .header("X-Last-Modified", meta.last_modified.to_string())
        .header("X-Content-Length", meta.size.to_string())
        .header("X-Permission", &meta.perm)
}

pub async fn handle_fs_put(
    State(state): State<Arc<ServerState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Versioned path: `.md` files when history is enabled.
    if is_md(&path) && state.history.is_some() {
        return handle_versioned_put(state, path, headers, body).await;
    }

    let meta = file_meta_from_headers(&headers, &path);
    let state_inner = state.clone();
    let path_inner = path.clone();
    match run_blocking(move || {
        state_inner
            .space
            .write_file(&path_inner, &body, Some(&meta))
    })
    .await
    {
        Ok(result_meta) => {
            set_file_meta_headers(Response::builder().status(StatusCode::OK), &result_meta)
                .header("Cache-Control", "no-store")
                .body(Body::from("OK"))
                .unwrap()
        }
        Err(e) => {
            tracing::error!("write failed: {e}");
            space_error_response(e)
        }
    }
}

/// Versioned write for `.md` files. Reads X-Parent-Hash, routes through the
/// merge logic in `history::versioned_write`, and emits X-Content-Hash on every
/// successful path. Returns 409 with conflict-marked body on merge conflict.
async fn handle_versioned_put(
    state: Arc<ServerState>,
    path: String,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parent_hash = headers
        .get("X-Parent-Hash")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // We need a Sync handle to the space inside the blocking closure. `state` is
    // `Arc<ServerState>` and `ServerState.space: Box<dyn SpacePrimitives>` is
    // already `Send + Sync` per the trait, so cloning the Arc is enough.
    let state_for_blocking = state.clone();
    let path_for_blocking = path.clone();
    let body_vec = body.to_vec();
    let parent_for_blocking = parent_hash.clone();
    let history = state.history.as_ref().expect("history present").clone();

    let result = run_blocking_versioned(move || {
        versioned_write::handle(
            state_for_blocking.space.as_ref(),
            &history,
            &path_for_blocking,
            &body_vec,
            &parent_for_blocking,
        )
    })
    .await;

    match result {
        Ok(VersionedWriteResult::Ok { hash, meta }) => {
            set_file_meta_headers(Response::builder().status(StatusCode::OK), &meta)
                .header("Cache-Control", "no-store")
                .header("X-Content-Hash", hash)
                .body(Body::from("OK"))
                .unwrap()
        }
        Ok(VersionedWriteResult::Conflict { hash, meta, merged }) => {
            set_file_meta_headers(Response::builder().status(StatusCode::CONFLICT), &meta)
                .header("Cache-Control", "no-store")
                .header("X-Content-Hash", hash)
                .body(Body::from(merged))
                .unwrap()
        }
        Err(VersionedWriteError::Space(e)) => space_error_response(e),
        Err(VersionedWriteError::History(e)) => {
            tracing::error!("history error during PUT {path}: {e}");
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from(e.to_string()))
                .unwrap()
        }
    }
}

pub async fn handle_fs_delete(
    State(state): State<Arc<ServerState>>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let state_inner = state.clone();
    let path_inner = path.clone();
    match run_blocking(move || state_inner.space.delete_file(&path_inner)).await {
        Ok(()) => {
            if is_md(&path) {
                if let Some(history) = state.history.clone() {
                    let path_inner = path.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = history.delete_history(&path_inner) {
                            tracing::warn!("failed to delete history for {path_inner}: {e}");
                        }
                    })
                    .await;
                }
            }
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from("OK"))
                .unwrap()
        }
        Err(e) => space_error_response(e),
    }
}

/// Parse the client's `X-*` write headers into a `FileMeta`.
fn file_meta_from_headers(headers: &HeaderMap, path: &str) -> FileMeta {
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let header_i64 = |name: &str| {
        header_str(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0i64)
    };
    FileMeta {
        name: path.to_string(),
        created: header_i64("X-Created"),
        last_modified: header_i64("X-Last-Modified"),
        content_type: header_str("Content-Type").unwrap_or("").to_string(),
        size: header_str("X-Content-Length")
            .or_else(|| header_str("Content-Length"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0i64),
        perm: header_str("X-Permission").unwrap_or("ro").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::test_support::test_state;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn list_returns_written_files() {
        let state = test_state();
        state.space.write_file("a.md", b"hello", None).unwrap();
        let resp = crate::build_router(Arc::new(state))
            .oneshot(Request::builder().uri("/.fs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let files: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(files
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "a.md"));
    }

    #[tokio::test]
    async fn get_returns_bytes_and_headers() {
        let state = test_state();
        state.space.write_file("a.md", b"hello", None).unwrap();
        let resp = crate::build_router(Arc::new(state))
            .oneshot(
                Request::builder()
                    .uri("/.fs/a.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Full metadata-header contract the client reads off a GET.
        assert!(resp.headers().get("X-Created").is_some());
        assert!(resp.headers().get("X-Last-Modified").is_some());
        assert_eq!(resp.headers().get("X-Content-Length").unwrap(), "5");
        assert_eq!(resp.headers().get("X-Permission").unwrap(), "rw");
        let content_type = resp
            .headers()
            .get("Content-Type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let x_content_type = resp
            .headers()
            .get("X-Content-Type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(!content_type.is_empty());
        // On a normal GET the served Content-Type equals the real one.
        assert_eq!(content_type, x_content_type);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn fs_get_supports_conditional_304() {
        let state = Arc::new(test_state());
        state.space.write_file("big.js", b"payload", None).unwrap();
        // First request: 200 with a standard `Last-Modified` header.
        let r1 = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/.fs/big.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let last_modified = r1
            .headers()
            .get("last-modified")
            .expect("Last-Modified present")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!last_modified.is_empty());

        // Re-request echoing that value back: 304 Not Modified, empty body, but
        // the `Last-Modified` validator is still present.
        let r2 = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/.fs/big.js")
                    .header("if-modified-since", &last_modified)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r2.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            r2.headers().get("last-modified").unwrap().to_str().unwrap(),
            last_modified
        );
        let body = axum::body::to_bytes(r2.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());

        // A stale `If-Modified-Since` must still serve the full body with 200.
        let r3 = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/.fs/big.js")
                    .header("if-modified-since", "Tue, 01 Jan 1980 00:00:00 GMT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r3.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"payload");
    }

    #[tokio::test]
    async fn meta_probe_is_no_store_content_is_revalidatable() {
        // Regression guard: the empty-body X-Get-Meta probe and the full-body
        // content GET share the same URL. If the probe were cacheable, a browser
        // would revalidate a later content read against the cached EMPTY body and
        // serve nothing. So the probe MUST be `no-store`; the content GET stays a
        // revalidatable `no-cache` + `Last-Modified`.
        let state = Arc::new(test_state());
        state.space.write_file("x.md", b"hello", None).unwrap();

        // Metadata probe: no-store, empty body, but X-* metadata present.
        let meta = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/.fs/x.md")
                    .header("X-Get-Meta", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(meta.status(), StatusCode::OK);
        assert_eq!(
            meta.headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-store"
        );
        assert!(meta.headers().get("X-Last-Modified").is_some());
        let meta_body = axum::body::to_bytes(meta.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(meta_body.is_empty());

        // Content GET: revalidatable, full body.
        let content = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/.fs/x.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(content.status(), StatusCode::OK);
        assert_eq!(
            content
                .headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "no-cache"
        );
        assert!(content.headers().get("last-modified").is_some());
        // Content varies by `Accept` (octet-stream vs real type), so the cache
        // must key on it.
        assert_eq!(
            content.headers().get("vary").unwrap().to_str().unwrap(),
            "Accept"
        );
        let content_body = axum::body::to_bytes(content.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&content_body[..], b"hello");
    }

    #[tokio::test]
    async fn get_with_octet_stream_accept_overrides_content_type() {
        // The subtlest part of the /.fs contract: an `accept: application/octet-stream`
        // request forces the body Content-Type to octet-stream while the real type
        // is preserved in X-Content-Type.
        let state = test_state();
        state.space.write_file("a.md", b"hello", None).unwrap();
        let resp = crate::build_router(Arc::new(state))
            .oneshot(
                Request::builder()
                    .uri("/.fs/a.md")
                    .header("accept", "application/octet-stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "application/octet-stream"
        );
        let real = resp
            .headers()
            .get("X-Content-Type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_ne!(real, "application/octet-stream");
        assert!(real.contains("markdown") || real.starts_with("text/"));
    }

    #[tokio::test]
    async fn get_missing_is_404() {
        let resp = crate::build_router(Arc::new(test_state()))
            .oneshot(
                Request::builder()
                    .uri("/.fs/nope.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let app = crate::build_router(Arc::new(test_state()));
        let put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/.fs/note.md")
                    .body(Body::from("content"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK);

        let get = app
            .oneshot(
                Request::builder()
                    .uri("/.fs/note.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(get.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"content");
    }

    #[tokio::test]
    async fn delete_then_get_is_404() {
        let state = test_state();
        state.space.write_file("gone.md", b"x", None).unwrap();
        let app = crate::build_router(Arc::new(state));
        let del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/.fs/gone.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del.status(), StatusCode::OK);
        let get = app
            .oneshot(
                Request::builder()
                    .uri("/.fs/gone.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::NOT_FOUND);
    }

    mod versioned {
        use crate::test_support::test_state_with_history;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use std::sync::Arc;
        use tower::ServiceExt;

        async fn put(
            app: axum::Router,
            path: &str,
            body: &'static [u8],
            parent: Option<&str>,
        ) -> axum::http::Response<Body> {
            let mut req = Request::builder().method("PUT").uri(format!("/.fs/{path}"));
            if let Some(p) = parent {
                req = req.header("X-Parent-Hash", p);
            }
            app.oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn first_md_write_returns_content_hash() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));
            let resp = put(app, "p.md", b"hello", None).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let h = resp
                .headers()
                .get("X-Content-Hash")
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(h.len(), 64);
        }

        #[tokio::test]
        async fn fast_forward_with_matching_parent_succeeds() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));

            let r1 = put(app.clone(), "p.md", b"v1", None).await;
            assert_eq!(r1.status(), StatusCode::OK);
            let parent = r1
                .headers()
                .get("X-Content-Hash")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            let r2 = put(app, "p.md", b"v2", Some(&parent)).await;
            assert_eq!(r2.status(), StatusCode::OK);
            let new_hash = r2
                .headers()
                .get("X-Content-Hash")
                .unwrap()
                .to_str()
                .unwrap();
            assert_ne!(new_hash, parent);
        }

        #[tokio::test]
        async fn diverged_clean_merge_returns_ok_with_new_hash() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));

            // Base v0
            let r0 = put(app.clone(), "p.md", b"a\nb\nc\n", None).await;
            assert_eq!(r0.status(), StatusCode::OK);
            let base = r0
                .headers()
                .get("X-Content-Hash")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            // Server advances HEAD: edit line 1.
            let r1 = put(app.clone(), "p.md", b"A\nb\nc\n", Some(&base)).await;
            assert_eq!(r1.status(), StatusCode::OK);

            // Client commits against the old base, editing line 3 (non-overlapping).
            let r2 = put(app, "p.md", b"a\nb\nC\n", Some(&base)).await;
            assert_eq!(r2.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn diverged_conflict_returns_409_with_conflict_body() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));

            let r0 = put(app.clone(), "p.md", b"line1\nline2\nline3\n", None).await;
            let base = r0
                .headers()
                .get("X-Content-Hash")
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            let _ = put(app.clone(), "p.md", b"line1\nSERVER\nline3\n", Some(&base)).await;

            let resp = put(app, "p.md", b"line1\nCLIENT\nline3\n", Some(&base)).await;
            assert_eq!(resp.status(), StatusCode::CONFLICT);
            assert!(resp.headers().get("X-Content-Hash").is_some());
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let s = String::from_utf8(body.to_vec()).unwrap();
            assert!(s.contains("<<<<<<< server"));
            assert!(s.contains("SERVER"));
            assert!(s.contains("CLIENT"));
            assert!(s.contains(">>>>>>> client"));
        }

        #[tokio::test]
        async fn legacy_put_without_parent_hash_fast_forwards() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));

            assert_eq!(
                put(app.clone(), "p.md", b"v1", None).await.status(),
                StatusCode::OK
            );
            // Second write without X-Parent-Hash should NOT 409.
            assert_eq!(put(app, "p.md", b"v2", None).await.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn get_md_returns_content_hash_header() {
            let (state, _td) = test_state_with_history();
            let app = crate::build_router(Arc::new(state));
            put(app.clone(), "p.md", b"hello", None).await;

            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/.fs/p.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(resp.headers().get("X-Content-Hash").is_some());
        }

        #[tokio::test]
        async fn delete_md_clears_history() {
            let (state, _td) = test_state_with_history();
            let history = state.history.clone().unwrap();
            let app = crate::build_router(Arc::new(state));

            put(app.clone(), "p.md", b"hello", None).await;
            assert!(!history.get_head("p.md").unwrap().is_empty());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/.fs/p.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(history.get_head("p.md").unwrap(), "");
        }
    }
}
