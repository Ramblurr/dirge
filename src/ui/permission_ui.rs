const ALLOW_PLACEHOLDER: &str = "<edit this pattern>";

/// Whether a pattern was returned by `suggest_pattern` as the
/// "empty input — please type a real pattern" placeholder rather
/// than a real glob. Used by the ask-dialog to detect when the
/// user pressed "allow always" on a degenerate input and refuse
/// to store the placeholder as an actual allowlist entry.
pub(crate) fn is_placeholder_pattern(p: &str) -> bool {
    p == ALLOW_PLACEHOLDER
}

pub(crate) fn suggest_pattern(tool: &str, input: &str) -> String {
    // Refuse to suggest a catch-all wildcard for empty / whitespace-
    // only input. A user mis-clicking "(a) allow always" on an empty
    // invocation would otherwise pin an "allow everything for this
    // tool forever" rule into their session. The placeholder string
    // is intentionally not a valid glob — the UI shows it as the
    // suggested pattern, the user edits it before confirming.
    const PLACEHOLDER: &str = ALLOW_PLACEHOLDER;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PLACEHOLDER.to_string();
    }
    match tool {
        "bash" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{} *", first)
        }
        // Path-arg tools: suggest a `<parent>/**` glob from the input
        // path. One arm for all of them — previously read/write/edit/
        // list_dir, apply_patch, and the semantic tools each had an
        // identical copy of this body (dirge-t1wh).
        "read" | "write" | "edit" | "list_dir" | "apply_patch" | "list_symbols"
        | "get_symbol_body" | "find_definition" | "find_callers" | "find_callees" => {
            let path = std::path::Path::new(trimmed);
            let parent = path
                .parent()
                .map(|p| p.to_string_lossy())
                .unwrap_or(std::borrow::Cow::Borrowed(""));
            if parent.is_empty() {
                "**".to_string()
            } else {
                format!("{}/**", parent)
            }
        }
        "grep" | "find_files" => {
            let first = trimmed.split_whitespace().next().unwrap_or(PLACEHOLDER);
            format!("{}*", first)
        }
        "mcp_tool" => {
            let mut parts = trimmed.splitn(3, ':');
            let umbrella = parts.next().unwrap_or("");
            let server = parts.next().unwrap_or("");
            if umbrella.eq_ignore_ascii_case("mcp_tool") && !server.is_empty() {
                format!("mcp_tool:{}:*", server)
            } else {
                PLACEHOLDER.to_string()
            }
        }
        "webfetch" => "webfetch:*".to_string(),
        "websearch" => "websearch:*".to_string(),
        "task" | "task_status" | "question" => "**".to_string(),
        "glob" | "repo_overview" | "skill" | "memory" | "write_todo_list" | "lsp" => {
            "**".to_string()
        }
        _ => PLACEHOLDER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `suggest_pattern` returns a literal placeholder for empty
    /// input. The ask-dialog path that consumes it must detect the
    /// placeholder and refuse to add it as an allowlist entry —
    /// otherwise pressing "a" (allow always) on an empty invocation
    /// would silently store `<edit this pattern>` as a real pattern.
    /// The detection is exposed via `is_placeholder_pattern` so the
    /// dialog code is unit-testable.
    #[test]
    fn placeholder_pattern_is_detectable() {
        let p = suggest_pattern("bash", "");
        assert!(
            is_placeholder_pattern(&p),
            "empty input should yield a detectable placeholder; got {p:?}",
        );
        let p = suggest_pattern("grep", "  \t  ");
        assert!(is_placeholder_pattern(&p));
        // A legit suggestion is NOT flagged as a placeholder.
        let p = suggest_pattern("bash", "cargo test");
        assert!(!is_placeholder_pattern(&p), "real pattern flagged: {p:?}");
    }

    // Whitespace-only or empty input must NOT collapse to a "* *"
    // / "*" wildcard pattern that matches every subsequent call.
    // The audit flagged this as a footgun: a user accidentally
    // hitting "(a) allow always" on an empty bash invocation would
    // permanently auto-allow ALL bash. Now we return a literal
    // placeholder + the user has to type the pattern themselves.
    #[test]
    fn suggest_pattern_refuses_wildcard_on_empty_input() {
        // Bash: empty / whitespace input should NOT yield "* *".
        let p = suggest_pattern("bash", "");
        assert_ne!(p, "* *", "empty bash input must not yield catch-all");
        assert!(
            !p.contains('*'),
            "empty input should not contain wildcards: {p:?}"
        );

        let p = suggest_pattern("bash", "   \t  ");
        assert_ne!(
            p, "* *",
            "whitespace-only bash input must not yield catch-all"
        );
        assert!(
            !p.contains('*'),
            "ws-only input should not contain wildcards: {p:?}"
        );

        // grep / find_files: same — empty must not yield "*"
        let p = suggest_pattern("grep", "");
        assert!(
            !p.contains('*'),
            "empty grep input must not yield wildcard: {p:?}"
        );

        // Unknown tool with empty input shouldn't yield catch-all.
        let p = suggest_pattern("mcp_tool:foo", "");
        assert!(!p.contains('*'), "unknown tool empty input: {p:?}");
    }

    // Non-empty inputs still produce the expected suggestion.
    #[test]
    fn suggest_pattern_works_for_non_empty_inputs() {
        assert_eq!(suggest_pattern("bash", "cargo test --all"), "cargo *");
        assert_eq!(suggest_pattern("grep", "fn foo bar"), "fn*");
    }

    /// User-reported bug: "allow always" on a write inside `src/`
    /// stored `src/*` (single `*`, no slash-spanning), so the next
    /// write under `src/agent/…` re-prompted. Maki's equivalent
    /// (`maki-agent/src/permissions.rs:519`) uses `parent/**`. Pin
    /// that the fix is in place for every path-shaped tool.
    #[test]
    fn suggest_pattern_path_tools_use_recursive_glob() {
        assert_eq!(suggest_pattern("write", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("edit", "src/main.rs"), "src/**");
        assert_eq!(
            suggest_pattern("write", "src/agent/tools/foo.rs"),
            "src/agent/tools/**"
        );
        assert_eq!(suggest_pattern("read", "src/main.rs"), "src/**");
        assert_eq!(suggest_pattern("list_dir", "src/agent"), "src/**");
        // Files at the repo root: `Path::parent` is "" — keep the
        // existing `**` fallback so the rule is broad but explicit.
        assert_eq!(suggest_pattern("write", "main.rs"), "**");
    }

    /// User-reported bug: `[a] allow always` on an MCP tool call
    /// silently degraded to `allow once` because the catch-all
    /// `_ => PLACEHOLDER` branch fired for `mcp_tool`. Result: the
    /// permission allowlist never got an entry and every
    /// subsequent call to the same MCP server re-prompted the
    /// user.
    #[test]
    fn suggest_pattern_derives_server_wildcard_for_mcp_tool() {
        let p = suggest_pattern("mcp_tool", "mcp_tool:lattice:lattice_expand");
        assert_eq!(p, "mcp_tool:lattice:*");
        // Multi-segment server names also work.
        let p = suggest_pattern("mcp_tool", "mcp_tool:my-server:do_thing");
        assert_eq!(p, "mcp_tool:my-server:*");
    }

    /// Malformed MCP input (missing colons, wrong umbrella) still
    /// falls through to the placeholder rather than producing a
    /// nonsense pattern.
    #[test]
    fn suggest_pattern_mcp_tool_malformed_input_uses_placeholder() {
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool", "garbage"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool:"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "mcp_tool::"
        )));
        assert!(is_placeholder_pattern(&suggest_pattern(
            "mcp_tool",
            "wrong:lattice:foo"
        )));
    }
}
