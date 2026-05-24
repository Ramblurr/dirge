pub mod ask;
pub mod checker;
pub mod pattern;

/// Push the active prompt's `deny_tools` list into the permission
/// checker so subsequent tool calls observe the new restriction.
/// Best-effort: a poisoned mutex falls through to `into_inner`,
/// matching the recovery pattern used elsewhere on the checker.
/// `None` perm (e.g. `--no-tools` builds) is a no-op.
pub fn apply_prompt_deny(perm: &Option<checker::PermCheck>, deny: &[String]) {
    if let Some(p) = perm {
        let mut guard = p.lock().unwrap_or_else(|e| e.into_inner());
        guard.set_prompt_deny_tools(deny.to_vec());
    }
}

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolPerm {
    Simple(Action),
    Granular(HashMap<String, Action>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PermissionConfig {
    #[serde(rename = "*")]
    pub default: Option<Action>,
    pub bash: Option<ToolPerm>,
    pub read: Option<ToolPerm>,
    pub write: Option<ToolPerm>,
    pub edit: Option<ToolPerm>,
    pub grep: Option<ToolPerm>,
    pub find_files: Option<ToolPerm>,
    pub list_dir: Option<ToolPerm>,
    /// `glob` — fast filename matcher. Read-only filesystem walker;
    /// per-pattern rules let users restrict which paths the LLM can
    /// glob (e.g. allow only project root, deny `/etc/*`). Adversarial-
    /// review #5 added.
    pub glob: Option<ToolPerm>,
    /// `repo_overview` — structural codebase map. Read-only walker;
    /// per-pattern rules can restrict which roots it can summarize.
    /// Adversarial-review #5 added.
    pub repo_overview: Option<ToolPerm>,
    pub write_todo_list: Option<ToolPerm>,
    /// `apply_patch` — bulk multi-file patch tool. Mutates the
    /// filesystem like `write`/`edit`; deserves per-pattern rules.
    pub apply_patch: Option<ToolPerm>,
    /// `lsp` — language-server queries (definition, references,
    /// hover, etc.). Reads project files via the language server.
    pub lsp: Option<ToolPerm>,
    /// `question` — interactive user-input solicitation tool. Per-
    /// pattern rules let users restrict which kinds of questions
    /// the agent can ask.
    pub question: Option<ToolPerm>,
    /// `webfetch` — HTTP(S) fetch tool. Pattern rules can be used to
    /// restrict the URLs (e.g., \"https://docs.example.com/*\":
    /// allow).
    pub webfetch: Option<ToolPerm>,
    /// `websearch` — Exa-backed web search. Pattern rules restrict
    /// the query strings.
    pub websearch: Option<ToolPerm>,
    /// `task` — subagent runner. The pattern is the subagent prompt.
    pub task: Option<ToolPerm>,
    /// `task_status` — companion query tool for `task`. Read-only;
    /// included for completeness so users can deny it independently
    /// (e.g. to force background-only invocations).
    pub task_status: Option<ToolPerm>,
    /// `memory` — persistent project memory store. Pattern rules
    /// restrict the memory keys / operations.
    pub memory: Option<ToolPerm>,
    /// `skill` — Claude-compatible skill loading. Pattern rules
    /// restrict which skills can be loaded.
    pub skill: Option<ToolPerm>,
    /// Semantic code-graph tools (tree-sitter-backed): `list_symbols`,
    /// `get_symbol_body`, `find_definition`, `find_callers`,
    /// `find_callees`. One per tool — pattern matches against the
    /// tool's primary argument (path for body/list, symbol name for
    /// find_*).
    pub list_symbols: Option<ToolPerm>,
    pub get_symbol_body: Option<ToolPerm>,
    pub find_definition: Option<ToolPerm>,
    pub find_callers: Option<ToolPerm>,
    pub find_callees: Option<ToolPerm>,
    /// `mcp_tool` — generic catch-all for ALL MCP-provided tools.
    /// Each MCP tool is permission-checked as
    /// `mcp_tool:<server>:<tool>`; pattern rules here match against
    /// that string. e.g.
    /// `{ \"mcp_tool:filesystem:*\": \"deny\" }` blocks every tool
    /// from the `filesystem` MCP server.
    pub mcp_tool: Option<ToolPerm>,
    pub external_directory: Option<HashMap<String, Action>>,
    pub doom_loop: Option<Action>,
    /// M2 (dirge-cep): unified per-tool rule map. Lets a user write
    /// rules for ANY tool name (including plugin / MCP / future-added
    /// tools) without dirge extending its `PermissionConfig` struct.
    ///
    /// Schema (JSON):
    /// ```json
    /// "permission": {
    ///   "tools": {
    ///     "bash":       { "rm *": "deny", "git *": "allow" },
    ///     "write":      { "/etc/**": "deny", "**": "ask" },
    ///     "skill":      "allow",
    ///     "plugin_xyz": "ask"
    ///   }
    /// }
    /// ```
    ///
    /// Mirrors opencode's permission shape (Schema.StructWithRest with
    /// Schema.Record(String, Rule)) and maki's per-tool TOML sections.
    /// Coexists with the legacy per-tool fields above for back-compat:
    /// both are merged into the same `HashMap<tool, Vec<(Pattern,
    /// Action)>>` inside `PermissionChecker::new`. If both name the
    /// same tool, the `tools` map wins (it's the explicit new shape).
    ///
    /// Deprecation path: the legacy `bash`/`read`/`write`/... fields
    /// stay through one release cycle, then get removed once docs and
    /// example configs migrate to `tools`. Internally the checker
    /// treats them as syntactic sugar for `tools.{name}`.
    pub tools: Option<HashMap<String, ToolPerm>>,
}

/// Per-session security mode. Selected via `--yolo` / `--accept-all` /
/// `--restrictive` CLI flags or the `default_permission_mode` config
/// key. Mode precedence (high to low): `Yolo > Accept > Restrictive >
/// Standard`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SecurityMode {
    /// Every rule in `PermissionConfig` is consulted; tools with no
    /// matching rule fall back to the `*` default action.
    Standard,
    /// Like `Standard`, but any tool whose rule resolves to `Allow`
    /// *via the `*` fallback* (no explicit allow rule matched) gets
    /// upgraded to `Ask`. Explicit allow rules still allow; explicit
    /// deny rules still deny. The semantic difference from
    /// `Standard`: "if nothing explicitly approved this, ask the
    /// user." It does NOT flip every Allow to Ask.
    Restrictive,
    /// Auto-allows tools whose targets resolve inside the working
    /// directory; tools touching paths outside `cwd` still consult
    /// `external_directory` rules. Useful for fast iteration on a
    /// trusted project.
    Accept,
    /// Bypasses every check. Use with caution.
    Yolo,
}

impl std::fmt::Display for SecurityMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityMode::Standard => write!(f, "standard"),
            SecurityMode::Restrictive => write!(f, "restrictive"),
            SecurityMode::Accept => write!(f, "accept"),
            SecurityMode::Yolo => write!(f, "yolo"),
        }
    }
}

pub fn default_bash_rules() -> Vec<(&'static str, Action)> {
    // Allow-list ordering / shape — three buckets:
    //   1. Read-only inspection (cat / ls / grep / etc.)
    //   2. Project-scoped dev workflow inside CWD (cargo / git
    //      writes that stay local / make / npm test / language
    //      runners). Same trust model as the CWD-scoped write/edit
    //      allow installed in `checker.rs:install_cwd_allow_rules`:
    //      if you trust the agent to edit project files, running
    //      project code is the same trust level.
    //   3. Filesystem mutators (mkdir / touch / mv / cp) — they
    //      ALSO route their path arguments through the `write` rules
    //      via `extract_mutation_paths`, so the CWD-allow on write
    //      still gates the actual filesystem destination.
    //
    // Patterns use `**` (any chars including `/`) instead of exact
    // match because every prior exact pattern (`cargo build`,
    // `git status`, etc.) silently re-prompted on common flagged
    // invocations like `cargo build --release` or `git status -s` —
    // friction that drove the "permissions are too aggressive"
    // complaint.
    //
    // Intentionally NOT auto-allowed:
    //   - `git push **`           — side effect outside the project
    //   - `git rebase/reset/stash`— destructive, can lose work
    //   - `npm install **`, `pip install **` — executes install
    //     scripts as arbitrary code outside the repo tree
    //   - `sudo **`               — privilege escalation always asks
    //   - `curl/wget`             — network egress always asks
    vec![
        // Read-only inspection
        ("ls **", Action::Allow),
        ("cd **", Action::Allow),
        ("pwd", Action::Allow),
        ("echo **", Action::Allow),
        ("which **", Action::Allow),
        ("type **", Action::Allow),
        ("cat **", Action::Allow),
        ("head **", Action::Allow),
        ("tail **", Action::Allow),
        ("wc **", Action::Allow),
        ("sort **", Action::Allow),
        ("uniq **", Action::Allow),
        ("cut **", Action::Allow),
        ("diff **", Action::Allow),
        ("grep **", Action::Allow),
        ("rg **", Action::Allow),
        ("find **", Action::Allow),
        ("file **", Action::Allow),
        ("stat **", Action::Allow),
        ("env", Action::Allow),
        ("date **", Action::Allow),
        ("whoami", Action::Allow),
        ("hostname", Action::Allow),
        // Git — local read/write inside the repo
        ("git status **", Action::Allow),
        ("git log **", Action::Allow),
        ("git diff **", Action::Allow),
        ("git show **", Action::Allow),
        ("git branch **", Action::Allow),
        ("git add **", Action::Allow),
        ("git commit **", Action::Allow),
        ("git checkout **", Action::Allow),
        ("git switch **", Action::Allow),
        ("git pull **", Action::Allow),
        ("git fetch **", Action::Allow),
        ("git remote **", Action::Allow),
        ("git tag **", Action::Allow),
        ("git blame **", Action::Allow),
        ("git restore **", Action::Allow),
        ("git rev-parse **", Action::Allow),
        ("git rev-list **", Action::Allow),
        ("git ls-files **", Action::Allow),
        ("git config --get **", Action::Allow),
        // Rust toolchain
        ("cargo check **", Action::Allow),
        ("cargo build **", Action::Allow),
        ("cargo test **", Action::Allow),
        ("cargo fmt **", Action::Allow),
        ("cargo clippy **", Action::Allow),
        ("cargo run **", Action::Allow),
        ("cargo doc **", Action::Allow),
        ("cargo tree **", Action::Allow),
        ("cargo metadata **", Action::Allow),
        ("rustc --version", Action::Allow),
        // Filesystem mutators — path args still route through
        // `write` rules via `extract_mutation_paths` (F1 dirge-dvy),
        // so the CWD-allow on write still gates the destination.
        ("mkdir **", Action::Allow),
        ("touch **", Action::Allow),
        ("mv **", Action::Allow),
        ("cp **", Action::Allow),
        ("ln **", Action::Allow),
        ("chmod **", Action::Allow),
        // Node / npm / yarn / pnpm — runners (NOT installers)
        ("npm test **", Action::Allow),
        ("npm run **", Action::Allow),
        ("npm ls **", Action::Allow),
        ("npx **", Action::Allow),
        ("node **", Action::Allow),
        ("yarn run **", Action::Allow),
        ("pnpm run **", Action::Allow),
        // Python — runners + read-only pip
        ("python **", Action::Allow),
        ("python3 **", Action::Allow),
        ("pytest **", Action::Allow),
        ("ruff **", Action::Allow),
        ("black **", Action::Allow),
        ("mypy **", Action::Allow),
        ("pip list **", Action::Allow),
        ("pip show **", Action::Allow),
        ("pip freeze", Action::Allow),
        // Go
        ("go build **", Action::Allow),
        ("go test **", Action::Allow),
        ("go run **", Action::Allow),
        ("go fmt **", Action::Allow),
        ("go vet **", Action::Allow),
        ("go mod **", Action::Allow),
        // Make + general task runners
        ("make **", Action::Allow),
        ("just **", Action::Allow),
        // Hard denies — destructive system-level operations
        ("rm -rf /**", Action::Deny),
        ("sudo rm -rf /**", Action::Deny),
        ("dd **", Action::Deny),
        ("mkfs **", Action::Deny),
        ("fdisk **", Action::Deny),
        ("mkswap **", Action::Deny),
    ]
}
