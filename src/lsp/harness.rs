//! Plugin-facing LSP query dispatcher.
//!
//! Bridges the Janet `harness/lsp` call (see `plugin::worker`) to the
//! async [`LspManager`]: parse the JSON request the worker forwarded, run
//! the query, and return a JSON-encoded result string. Mirrors the `lsp`
//! tool's operation set and its 1-based line/column convention.
//!
//! Only compiled when both `plugin` and `lsp` are enabled — without
//! `lsp` there's no `LspManager`; without `plugin` there's no caller.

use std::path::Path;

use serde_json::{Value, json};

use crate::lsp::manager::LspManager;
use crate::lsp::query::{self, Operation};

/// The JSON request shape built by `harness/__lsp` in the plugin worker.
#[derive(serde::Deserialize)]
struct Request {
    op: String,
    file: String,
    #[serde(default)]
    line: u32,
    #[serde(default)]
    char: u32,
    #[serde(default)]
    query: String,
}

/// Run one plugin LSP query and return a JSON string. Errors (bad
/// request, unknown op) are returned as `{"error": "..."}` JSON so the
/// plugin always gets a parseable value.
pub async fn run_query(manager: &LspManager, request_json: &str) -> String {
    let req: Request = match serde_json::from_str(request_json) {
        Ok(r) => r,
        Err(e) => return json!({ "error": format!("invalid lsp request: {e}") }).to_string(),
    };
    let path = Path::new(&req.file);

    // `diagnostics` reads already-published state — it isn't a positional
    // query, so it's handled here rather than through the shared dispatch.
    if req.op == "diagnostics" {
        // The manager keys diagnostics by the path it opened the file
        // under; try the literal path, then its canonical form.
        let diags = manager
            .diagnostics_for(path)
            .or_else(|| {
                path.canonicalize()
                    .ok()
                    .and_then(|c| manager.diagnostics_for(&c))
            })
            .unwrap_or_default();
        return json!(diags).to_string();
    }

    // Everything else goes through the shared op dispatch (coordinate
    // conversion + touch_file + op→method match), which the lsp tool uses
    // too — so the op set and aliases can't drift between the two.
    match Operation::parse(&req.op) {
        Some(op) => {
            let result: Value = query::run(manager, op, path, req.line, req.char, &req.query).await;
            result.to_string()
        }
        None => json!({ "error": format!("unknown lsp op: {}", req.op) }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::manager::LspManager;
    use crate::lsp::spawn::{Spawned, Spawner};
    use futures::future::BoxFuture;
    use std::sync::Arc;

    /// A spawner that never produces a server. Used for the error-path
    /// tests, where the request is rejected before (bad JSON) or no LSP
    /// server matches the file (unknown op on a bare temp path), so spawn
    /// is never actually invoked.
    struct NoServerSpawner;
    impl Spawner for NoServerSpawner {
        fn spawn<'a>(
            &'a self,
            _server_id: &'a str,
            _root: &'a Path,
        ) -> BoxFuture<'a, std::io::Result<Spawned>> {
            Box::pin(async { Err(std::io::Error::other("no server in test")) })
        }
    }

    fn test_manager() -> LspManager {
        LspManager::new(Arc::new(NoServerSpawner), std::env::temp_dir())
    }

    #[tokio::test]
    async fn invalid_json_returns_error_object() {
        let out = run_query(&test_manager(), "not json at all").await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(
            v.get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|s| s.contains("invalid lsp request")),
            "got {out}"
        );
    }

    #[tokio::test]
    async fn unknown_op_returns_error_object() {
        // A file with no matching LSP server → touch_file is a no-op (no
        // spawn), then the unknown op short-circuits to an error.
        let req = json!({ "op": "frobnicate", "file": "/tmp/none.xyz" }).to_string();
        let out = run_query(&test_manager(), &req).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v.get("error").and_then(|e| e.as_str()),
            Some("unknown lsp op: frobnicate"),
            "got {out}"
        );
    }

    #[tokio::test]
    async fn accepts_operation_aliases_shared_with_the_lsp_tool() {
        // `goToDefinition` is the alias the lsp tool accepts for
        // `definition`. The harness must resolve it too (shared dispatch),
        // not reject it as an unknown op. No server matches the temp file,
        // so the result is a null/empty value — the point is the absence of
        // an "unknown lsp op" error.
        let req = json!({ "op": "goToDefinition", "file": "/tmp/none.xyz", "line": 1, "char": 1 })
            .to_string();
        let out = run_query(&test_manager(), &req).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        let err = v.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(!err.contains("unknown"), "alias should resolve, got {out}");
    }

    #[tokio::test]
    async fn diagnostics_for_untracked_file_is_empty_array() {
        let req = json!({ "op": "diagnostics", "file": "/tmp/never-opened.rs" }).to_string();
        let out = run_query(&test_manager(), &req).await;
        assert_eq!(out, "[]", "got {out}");
    }
}
