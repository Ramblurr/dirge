//! Shared LSP operation dispatch.
//!
//! The `lsp` agent tool ([`crate::agent::tools::lsp`]) and the plugin
//! `harness/lsp` bridge ([`crate::lsp::harness`]) both turn an operation
//! name + position into an [`LspManager`] call and a JSON value. That
//! mapping — the op→method match and the 1-based→0-based coordinate
//! convention — lives here so the two callers can't drift (they had:
//! the tool accepted `goToDefinition`/`findReferences` aliases, the
//! harness didn't). Each caller keeps its own surrounding policy
//! (permission checks, path validation, output formatting, error shape).
//!
//! Note: `diagnostics` is intentionally NOT an [`Operation`] — it reads
//! already-published state rather than issuing a positional request, so
//! the harness handles it separately and the tool doesn't expose it.

use std::path::Path;

use serde_json::{Value, json};

use crate::lsp::manager::{LspManager, TouchMode};

/// A position- or symbol-based LSP operation. Names (and their aliases)
/// match the `lsp` tool's documented operation set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Definition,
    References,
    Hover,
    DocumentSymbol,
    WorkspaceSymbol,
    Implementation,
    PrepareCallHierarchy,
    IncomingCalls,
    OutgoingCalls,
}

impl Operation {
    /// Parse an operation name. Accepts the opencode-style camelCase
    /// aliases (`goToDefinition`, `findReferences`, `goToImplementation`)
    /// in addition to the canonical names.
    pub fn parse(s: &str) -> Option<Operation> {
        match s {
            "definition" | "goToDefinition" => Some(Operation::Definition),
            "references" | "findReferences" => Some(Operation::References),
            "hover" => Some(Operation::Hover),
            "documentSymbol" => Some(Operation::DocumentSymbol),
            "workspaceSymbol" => Some(Operation::WorkspaceSymbol),
            "implementation" | "goToImplementation" => Some(Operation::Implementation),
            "prepareCallHierarchy" => Some(Operation::PrepareCallHierarchy),
            "incomingCalls" => Some(Operation::IncomingCalls),
            "outgoingCalls" => Some(Operation::OutgoingCalls),
            _ => None,
        }
    }

    /// Whether the operation consumes the `line`/`character` position.
    /// `documentSymbol`/`workspaceSymbol` ignore it.
    pub fn needs_position(self) -> bool {
        !matches!(self, Operation::DocumentSymbol | Operation::WorkspaceSymbol)
    }
}

/// Run a position/symbol operation against the language servers and return
/// the raw JSON value of the response.
///
/// `line`/`character` are **1-based** (editor convention); the conversion
/// to the 0-based LSP wire format happens here — the single canonical
/// conversion point. The file is synced to the servers first
/// (`touch_file`); diagnostics are NOT awaited (that is the edit tool's
/// concern). `query` is only used by `workspaceSymbol`.
pub async fn run(
    manager: &LspManager,
    op: Operation,
    path: &Path,
    line: u32,
    character: u32,
    query: &str,
) -> Value {
    // Sync the file with the server before any positional query.
    manager.touch_file(path, TouchMode::Notify).await;

    // 1-based editor coordinates → 0-based LSP wire format.
    let line = line.saturating_sub(1);
    let ch = character.saturating_sub(1);

    match op {
        Operation::Definition => json!(manager.definition(path, line, ch).await),
        Operation::References => json!(manager.references(path, line, ch).await),
        Operation::Hover => json!(manager.hover(path, line, ch).await),
        Operation::DocumentSymbol => json!(manager.document_symbol(path).await),
        Operation::WorkspaceSymbol => json!(manager.workspace_symbol(path, query).await),
        Operation::Implementation => json!(manager.implementation(path, line, ch).await),
        Operation::PrepareCallHierarchy => {
            json!(manager.prepare_call_hierarchy(path, line, ch).await)
        }
        Operation::IncomingCalls => json!(manager.incoming_calls(path, line, ch).await),
        Operation::OutgoingCalls => json!(manager.outgoing_calls(path, line, ch).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_canonical_and_alias_names() {
        assert_eq!(Operation::parse("definition"), Some(Operation::Definition));
        assert_eq!(
            Operation::parse("goToDefinition"),
            Some(Operation::Definition)
        );
        assert_eq!(
            Operation::parse("findReferences"),
            Some(Operation::References)
        );
        assert_eq!(
            Operation::parse("goToImplementation"),
            Some(Operation::Implementation)
        );
        assert_eq!(Operation::parse("nope"), None);
    }

    #[test]
    fn only_symbol_ops_skip_position() {
        assert!(!Operation::DocumentSymbol.needs_position());
        assert!(!Operation::WorkspaceSymbol.needs_position());
        assert!(Operation::Definition.needs_position());
        assert!(Operation::IncomingCalls.needs_position());
    }
}
