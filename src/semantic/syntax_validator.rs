//! Tree-sitter syntax validation for content that's about to be
//! written to disk. Phase 2 of `docs/AGENTIC_LOOP_PLAN.md`: catch
//! the LLM writing syntactically-broken code BEFORE the bytes
//! land in the filesystem, so the model sees the error in the
//! same turn and can self-correct (instead of writing broken code
//! and discovering it via `cargo check` two turns later).
//!
//! Called from `write::call`, `edit::call`, `apply_patch::call`.
//! Default-on when a tree-sitter language is registered for the
//! file's extension; default-off (returns no errors) otherwise.
//!
//! Per-feature gating: each language requires its corresponding
//! `semantic-<lang>` Cargo feature to compile in. Without any
//! feature, this module is a no-op stub.
//!
//! Error budget: capped at `MAX_ERRORS` per call so a totally-
//! broken file doesn't dump 1000 errors into the tool result.

use std::path::Path;

/// One syntax error discovered by tree-sitter. Carries enough
/// detail for the model to localize the fix without re-reading
/// the file.
#[derive(Debug, Clone)]
pub struct SyntaxError {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub column: usize,
    /// Short snippet of the problematic source range (≤ 80 chars
    /// or one line, whichever is shorter).
    pub snippet: String,
    /// Whether tree-sitter classified this as an ERROR node (true
    /// syntax error) or a MISSING node (tree-sitter inferred a
    /// missing token like `;`).
    pub is_missing: bool,
}

impl SyntaxError {
    /// Format for inclusion in a tool-error message.
    pub fn render(&self) -> String {
        let kind = if self.is_missing {
            "missing token"
        } else {
            "syntax error"
        };
        format!(
            "  {kind} at {line}:{col}: {snippet}",
            kind = kind,
            line = self.line,
            col = self.column,
            snippet = self.snippet,
        )
    }
}

/// Cap on the number of errors surfaced per call. Tree-sitter can
/// cascade — one missing brace produces dozens of downstream
/// ERROR nodes — so a flat truncation keeps the tool result
/// readable.
const MAX_ERRORS: usize = 10;

/// Resolve the file extension to a tree-sitter Language. Returns
/// `None` for files we don't know how to parse, OR when the
/// matching `semantic-<lang>` feature isn't compiled in. The
/// caller should treat `None` as "skip validation" (silent
/// fall-through), not "error".
fn language_for_path(path: &Path) -> Option<tree_sitter::Language> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        #[cfg(feature = "semantic-rust")]
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),

        #[cfg(feature = "semantic-ts")]
        "ts" | "tsx" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),

        #[cfg(feature = "semantic-ts")]
        "js" | "jsx" | "mjs" | "cjs" => {
            // TSX grammar handles JSX too; close enough for syntax
            // validation. The semantic extractor uses a separate
            // JS adapter; we accept slightly higher false-negative
            // rate here in exchange for not pulling in a second
            // grammar crate just for syntax checking.
            Some(tree_sitter_typescript::LANGUAGE_TSX.into())
        }

        #[cfg(feature = "semantic-python")]
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE.into()),

        #[cfg(feature = "semantic-go")]
        "go" => Some(tree_sitter_go::LANGUAGE.into()),

        #[cfg(feature = "semantic-ruby")]
        "rb" | "rake" | "gemspec" => Some(tree_sitter_ruby::LANGUAGE.into()),

        #[cfg(feature = "semantic-java")]
        "java" => Some(tree_sitter_java::LANGUAGE.into()),

        #[cfg(feature = "semantic-c")]
        "c" => Some(tree_sitter_c::LANGUAGE.into()),

        #[cfg(feature = "semantic-cpp")]
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(tree_sitter_cpp::LANGUAGE.into()),

        #[cfg(feature = "semantic-clojure")]
        "clj" | "cljs" | "cljc" | "edn" | "bb" => Some(tree_sitter_clojure::LANGUAGE.into()),

        #[cfg(feature = "semantic-bash")]
        "sh" | "bash" => Some(tree_sitter_bash::LANGUAGE.into()),

        _ => None,
    }
}

/// Walk the syntax tree and collect ERROR / MISSING nodes. Capped
/// at `MAX_ERRORS`. Each error includes line:col plus a short
/// source snippet so the model can localize without re-reading.
fn collect_errors(tree: &tree_sitter::Tree, source: &str) -> Vec<SyntaxError> {
    let mut errors: Vec<SyntaxError> = Vec::new();
    let cursor = tree.walk();
    let mut stack: Vec<tree_sitter::Node> = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if errors.len() >= MAX_ERRORS {
            break;
        }
        if node.is_error() || node.is_missing() {
            let start = node.start_position();
            let snippet = snippet_for(node, source);
            errors.push(SyntaxError {
                line: start.row + 1,
                column: start.column + 1,
                snippet,
                is_missing: node.is_missing(),
            });
            // Skip walking deeper inside an error node — the
            // children are noise once the parent is known to be
            // broken.
            continue;
        }
        let _ = cursor; // silence unused-variable when the loop walks via `node.child()`
        // Push children in reverse so the walk is left-to-right.
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i) {
                stack.push(child);
            }
        }
    }
    errors
}

/// Best-effort short snippet for an error node. Returns the
/// node's source text trimmed to ≤ 80 chars on one line. Falls
/// back to the line containing the error when the node spans
/// multiple lines.
fn snippet_for(node: tree_sitter::Node, source: &str) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(source.len());
    if start >= end {
        // Missing nodes have zero byte span; pull the line they
        // sit on so the model can see context.
        let line_start = source[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = source[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(source.len());
        return source[line_start..line_end]
            .chars()
            .take(80)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    let raw = &source[start..end];
    let line: String = raw.chars().take_while(|c| *c != '\n').collect();
    line.chars()
        .take(80)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Validate `content` against the tree-sitter grammar registered
/// for `path`'s extension. Returns `Ok(())` for clean parses, for
/// unknown extensions, and for any environment where the matching
/// `semantic-<lang>` feature isn't built. Returns `Err(Vec<...>)`
/// only when the grammar is available AND found real errors.
///
/// Designed as a CHEAP pre-write check — typical execution time
/// for a 10 KiB Rust file is <2ms on modern hardware. The call
/// site decides whether to surface the errors as a tool failure
/// (the safest default for `write` / `edit` / `apply_patch`).
pub fn check_syntax(path: &Path, content: &str) -> Result<(), Vec<SyntaxError>> {
    let Some(lang) = language_for_path(path) else {
        return Ok(()); // unknown extension or feature not built
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
        // Grammar version mismatch — skip rather than block the
        // write. Validation is best-effort.
        return Ok(());
    }
    let Some(tree) = parser.parse(content, None) else {
        return Ok(());
    };
    if !tree.root_node().has_error() {
        return Ok(());
    }
    let errors = collect_errors(&tree, content);
    if errors.is_empty() {
        // has_error() returned true but the walk didn't find any
        // — shouldn't happen but defensive.
        return Ok(());
    }
    Err(errors)
}

/// Convenience wrapper: format a `Vec<SyntaxError>` as a single
/// multi-line string suitable for inclusion in a tool error
/// message. Call sites do not need to format individually.
pub fn format_errors(path: &Path, errors: &[SyntaxError]) -> String {
    let mut out = format!(
        "Syntax check failed for {}: {} error(s) detected by tree-sitter. \
         Fix and re-submit. (This is a pre-write guard — the file was NOT modified.)\n",
        path.display(),
        errors.len(),
    );
    for err in errors {
        out.push_str(&err.render());
        out.push('\n');
    }
    if errors.len() == MAX_ERRORS {
        out.push_str(&format!(
            "  …(truncated at {} errors; fix the listed issues and re-check)\n",
            MAX_ERRORS,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn clean_rust_passes() {
        let path = PathBuf::from("/tmp/foo.rs");
        assert!(check_syntax(&path, "fn main() {}\n").is_ok());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn broken_rust_returns_errors() {
        let path = PathBuf::from("/tmp/foo.rs");
        // Missing closing brace.
        let result = check_syntax(&path, "fn main() {\n  let x = 1;\n");
        let errors = result.expect_err("expected syntax errors");
        assert!(!errors.is_empty());
    }

    #[test]
    fn unknown_extension_skips_silently() {
        let path = PathBuf::from("/tmp/foo.thisisntreal");
        assert!(check_syntax(&path, "(((((").is_ok());
    }

    #[test]
    fn no_extension_skips_silently() {
        let path = PathBuf::from("/tmp/Makefile");
        assert!(check_syntax(&path, "all:\n\techo hello\n").is_ok());
    }

    #[cfg(feature = "semantic-python")]
    #[test]
    fn broken_python_returns_errors() {
        let path = PathBuf::from("/tmp/foo.py");
        // Unclosed paren.
        let result = check_syntax(&path, "def foo(\n");
        let errors = result.expect_err("expected syntax errors");
        assert!(!errors.is_empty());
    }

    #[cfg(feature = "semantic-rust")]
    #[test]
    fn format_errors_includes_path_and_count() {
        let path = PathBuf::from("/tmp/x.rs");
        let result = check_syntax(&path, "fn main( { ");
        let errors = result.expect_err("expected errors");
        let rendered = format_errors(&path, &errors);
        assert!(rendered.contains("/tmp/x.rs"));
        assert!(rendered.contains("error(s) detected"));
    }
}
