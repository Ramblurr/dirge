# dirge — Comprehensive Audit Report

**Date:** 2026-05-26
**Scope:** Full project audit across 7 logical feature areas (~92K LoC, 178 Rust files)
**Method:** Seven parallel clean-context audit agents, one per subsystem, with README claims as anchor points.

This report consolidates findings into a single prioritized punch list. Each finding is keyed `<area>-<n>` so issues can be filed as `bd create` tasks in batches.

Areas:
- **PERM** — permission system, sandbox, deny_tools
- **PROV** — provider clients, retry, recovery
- **LOOP** — agent loop, tool dispatch, compression, cache, bridge
- **TOOL** — built-in tool implementations
- **SESS** — session persistence, tree, compaction, FTS
- **EXT** — LSP, semantic, MCP, ACP, worktree, loop subsystem
- **UI** — TUI, prompts, skills, Janet plugins

---

## 1. Critical & High-Severity Cross-Cutting Themes

These patterns recur and should drive policy fixes rather than one-off patches:

| Theme | Affected findings |
|---|---|
| Outbound text → terminal/server without sanitization | UI-1, UI-2, TOOL-1, TOOL-4 |
| Race conditions on shared-state mutations (cache, versions, inflight) | LOOP-3, LOOP-5, EXT-3, EXT-4 |
| Trust boundary erosion: plugin/MCP/custom-provider config → arbitrary effect | PROV-1, EXT-11, UI-1, UI-3 |
| README ↔ implementation drift | LOOP-4, SESS-2, EXT-6, PROV-2 |
| Doom-loop / abuse-mitigation defenses that are easily evaded | PERM-1, PERM-2 |

---

## 2. CRITICAL

### PROV-1 — Custom-provider `base_url` is unvalidated → conversation exfiltration
**Files:** `src/provider/mod.rs:99-114`, `src/provider/client.rs:31-44, 97-106`, `src/main.rs:572-597`
A malicious `config.json` or compromised plugin can set `custom_providers.foo.base_url = "http://attacker/v1"`. Every prompt, file content, and tool result is then POSTed there. No scheme check, no allowlist, no insecure-opt-in warning. Plugin-registered providers go through the same path.
**Fix:** Reject non-`https` schemes by default; require `allow_insecure: true` per provider; warn loudly the first time a custom URL is used in a session; refuse plugin-registered names that collide with built-ins (`openai`, `anthropic`, …).

### LOOP-1 — `set_by_path` `usize` underflow panic on empty path
**File:** `src/agent/agent_loop/schema_flatten.rs:176`
`let last = path.len() - 1;` and a downstream `.expect("guaranteed object after check/insert")` (line 194) panic on adversarial flattened tool input. Affects any provider routed through schema flattening.
**Fix:** Handle `path.is_empty()` explicitly; replace `.expect` with a graceful skip + warn.

---

## 3. HIGH

### Permission / Sandbox

- **PERM-1 — Doom-loop window is 16; trivially evaded by 14 decoy calls.** `src/permission/checker.rs:771-786`. Switch to a per-key counter with TTL decay.
- **PERM-2 — Doom-loop trips *after* the 3rd call (already executed 2 repeats), off-by-one vs. README.** Same file. Move increment after the check, or document.
- **PERM-5 — Bash fallback (no `semantic-bash` feature) ignores heredocs.** `src/agent/tools/bash.rs:425-433`. `bash <<EOF\nrm -rf …\nEOF` passes the rule check because the splitter never sees the heredoc body. Treat `<<` / `<<-` as the "complex" trigger.
- **PERM-6 — Complex-bash branch skips path-side mutation extraction.** `src/semantic/adapters/bash.rs:307-386` & `src/agent/tools/bash.rs`. `echo "$(rm /etc/passwd)"` slips through if the outer `echo *` is allowed. Run `extract_mutation_paths` + `extract_redirect_targets` on the complex path too.
- **PERM-7 — MCP `deny_tools` defense lives in callers, not the checker.** `src/extras/mcp/tool.rs:128-144`. Future regression that drops the explicit probe re-opens the bypass. Move the concrete-name probe into `PermissionChecker::check`.
- **PERM-19 — Accept mode auto-allows `webfetch`, `task`, `memory`, `skill`, `apply_patch`.** `src/permission/checker.rs:544`. README says only `bash`/`mcp_tool` still ask in Accept mode; `webfetch` can exfiltrate, `task` recurses an agent. Add these to `is_high_risk_non_path_tool` (rename to `is_high_risk_tool`).

### Provider / Retry

- **PROV-2 — README claims a retry banner; none is emitted.** `src/agent/agent_loop/retry.rs:107-110, 156-165`. Retries are silent; users see a multi-second freeze. Emit `StreamEvent::Retry { attempt, delay, error }` and bridge it to a UI notice.
- **PROV-3 — Subagent / `btw_query` has no retry, no backoff.** `src/provider/mod.rs:364-382`, `src/agent/tools/task.rs:218-303`. A single 503 kills the subagent. Route through `spawn_runner` with `retrying_stream_fn`.
- **PROV-4 — Provider autodetect picks DeepSeek > OpenRouter when both keys set; no startup log.** `src/provider/mod.rs:176-204`, `src/cli.rs:222-229`. Surprising silent switch. Log resolved provider when autodetect was the source; or require `--provider` when multiple keys are present.
- **PROV-5 — Retry blocked too early: first ToolCallStart delta marks stream "committed."** `src/agent/agent_loop/retry.rs:184-195`. README says retry is blocked when tools have *executed*. Track a separate `tools_dispatched` flag flipped in `LoopTool::execute`.

### Agent Loop

- **LOOP-2 — `tool_input_repair` mutates path fields *before* permission check.** `src/agent/agent_loop/tools.rs:206-242`. Repair is conservative today, but the unaudited future could widen it; log original args alongside the repaired form.
- **LOOP-3 — Tool cache key omits cwd, env, file mtime.** `src/agent/tools/{read,grep,list_dir,find_files,glob,repo_overview}.rs`. External process writes (LSP, plugin-spawned bash via Janet) leave the cache stale until the next dirge-driven write. Include file mtime / cwd in key.
- **LOOP-4 — Interjection signal collapses graceful-stop and hard-abort.** `src/agent/agent_loop/integration.rs:108-127`. README promises "stops at next tool-result boundary"; actual behavior is `signal.cancel()` → tools see a synthetic-error result that the next turn's LLM reads as a real failure. Add a distinct graceful-interject channel.
- **LOOP-5 — Inflight set leaks on parallel-cancel.** `src/agent/agent_loop/tools.rs:633-668`. Drop guard pattern.
- **LOOP-6 — `fix_tool_call_pairing` drops *all* partially-matched tool results.** `src/agent/agent_loop/heal.rs:143-147`. Emit synthetic errors for the missing IDs and keep matched ones.

### Built-in Tools

- **TOOL-1 — webfetch SSRF defenses are hostname-only.** `src/agent/tools/webfetch.rs:80-152`. Bypass via DNS rebinding, decimal/octal/hex IPv4 (`http://2852039166/`), IPv4-mapped IPv6 (`http://[::ffff:127.0.0.1]/`). Resolve host, validate every resolved `SocketAddr`, pin into a custom resolver/connector.

### Session

- **SESS-1 — No cycle detection in tree walk → OOM on tampered session JSON.** `src/session/mod.rs:619-654`, `src/session/compact.rs:142-153`. Add visited-set checks to `switch_to_leaf`, `fork_at`, `clone_at`.
- **SESS-2 — "Round 9 context compression" is wired only as tool-output pruning.** `src/agent/agent_loop/run.rs:435-510`, `src/agent/compression.rs:18-22`. The LLM-summary path (`build_summary_prompt`, `summarize_with_model`, …) is `#[allow(dead_code)]`. `ContextCompacted` event reports a synthetic id but never rotates the session, never inserts the summary message, never persists. **Either gate the README claim or wire the missing steps.**
- **SESS-3 — Session files written world-readable (0644 default).** `src/session/storage.rs:55-100` via `src/fs_atomic.rs:80-109`. Contains user prompts, file contents read, command outputs — often secrets. Set 0600 on first write; chmod the session directory too.
- **SESS-4 — Post-compress leaf rewinds to *first* kept, not last.** `src/session/compact.rs:202-214`. `messages.get(1)` should be `messages.last()`.
- **SESS-5 — `/sessions ""` (trailing space) matches all sessions → silent load/delete of the only session.** `src/ui/slash.rs:406-456`, `src/session/storage.rs:702-722`. Reject empty prefix in `find_sessions_by_prefix`.

### External Integrations

- **EXT-1 — C++ header sniff matches bytes inside comments and strings.** `src/semantic/adapters/mod.rs:107-124`. `// refactor class-based API` routes a C header to the C++ adapter. Tokenize / strip comments before matching.
- **EXT-2 — Semantic index cache key (mtime + size) loses to 1s-granularity filesystems.** `src/semantic/index.rs:65-73`. Include `ctime` or a content hash of the first N KiB.
- **EXT-3 — LSP `notify_open` version-vs-content race under concurrent edits.** `src/lsp/client.rs:119-179`. Version bumped under mutex; file read happens after lock release. Two parallel edits can ship newer content with older version. Read under the lock or per-path async mutex.
- **EXT-4 — Push diagnostics accepted without version check.** `src/lsp/client.rs:79-101`. Stale-v1 diagnostics returned to a v3 caller — "clean" report hides real errors. Compare `params.version` against tracked version; drop older.
- **EXT-5 — JSON-RPC dispatch only handles `id` as `u64`; string IDs silently dropped.** `src/lsp/rpc.rs:205`. Caller waits 30 s instead of resolving. Parse `id` permissively.
- **EXT-6 — README claims `--prompt <name>` CLI flag for ACP launch-mode lock; flag does not exist.** `src/cli.rs`, `src/main.rs:334`. Add the flag or correct the README.

### UI / Plugins

- **UI-1 — Plugin `harness/confirm` title/question rendered without ANSI strip.** `src/ui/mod.rs:4627, 4691`. Janet plugin can embed escape sequences to repaint the screen and impersonate a real permission prompt. Pass both through `ansi::strip_escapes(_, KEEP_NEWLINE)`.
- **UI-2 — Plugin slash-command output rendered without ANSI strip.** `src/ui/slash.rs:1872-1875`. Same class as UI-1.
- **UI-3 — Synchronous `eval` blocks main path for 10 minutes on runaway plugin.** `src/plugin/worker.rs:690` (`RECV_TIMEOUT = 10 min`). A `(while true)` plugin hook freezes every host-side `eval`. Use 5-30s timeout for non-dialog evals and surface a "plugin timed out" notice.

---

## 4. MEDIUM

### Permission / Sandbox

- **PERM-3** — `is_external_path` uses snapshot canonicalization; symlink changes after `set_working_dir` go undetected. `src/permission/checker.rs:733-760`. Re-canonicalize on each check or document snapshot semantics.
- **PERM-4** — `glob_to_regex` makes trailing `/**` not match the bare directory (`/etc/**` misses `/etc`). `src/permission/pattern.rs:222-228`. Treat trailing `/**` as `(/.*)?$`.
- **PERM-8** — MCP-exported `edit_file` not subject to path-tool rules like `edit: { "/etc/**": deny }`. Only `mcp_tool` rules govern it. Document, or add opt-in alias layer.
- **PERM-10** — `is_sensitive_env_name` SAFE_EXACT lets `GITHUB_TOKEN` / `GH_TOKEN` / `SSH_AUTH_SOCK` reach bash even under `--sandbox`. `src/sandbox.rs:144-147`. Warn at session start; scrub-by-default + opt-in.
- **PERM-11** — Env scrub is name-based; `DATABASE_URL` / `MONGODB_URI` / `REDIS_URL` carry credentials in their *values*. `src/sandbox.rs:118-152`. Pattern-scan values for `://user:pass@`.
- **PERM-15** — Frontmatter parser silently accepts empty list when `deny_tools` is written as YAML block form (`- edit` / `- write`). `src/context/prompts.rs:81-108`. **Plan mode silently fails open.** Detect block-list shape and reject or parse it.

### Provider

- **PROV-6** — Context-length detection misses several provider strings (Anthropic `max_tokens is too large`, Cohere/Mistral `too many tokens`, DeepSeek `Range of input length`). `src/agent/recovery.rs:322-333`.
- **PROV-7** — Gemini 429 with `RESOURCE_EXHAUSTED` falls through `Other`; never retried. `src/agent/recovery.rs:283-289`.
- **PROV-8** — OpenAI `insufficient_quota` retried as RateLimit; burns 3 retries on a permanent billing failure. Same file.
- **PROV-9** — `/compress` summarizer fails when most needed because the entire conversation is shoved in with no chunking. `src/provider/summarize.rs:73-135`. Binary-truncate on `ContextLength` error.
- **PROV-10** — UTF-8 bug in `parse_http_date_retry_after`: full-message lowercase changes byte offsets vs. the original. `src/agent/recovery.rs:207`. Reuse `parse_after_label`'s case-insensitive scan.

### Agent Loop

- **LOOP-7** — `prune_tool_outputs` filters by `role == "tool"` but loop transcripts use `"toolResult"`. **Compaction is a no-op in production.** `src/agent/agent_loop/run.rs:438-510`, `src/agent/compression.rs:113-148`. Normalize role first, or match both. Tied to LOOP-18 (`tool_name` vs `toolName` casing mismatch).
- **LOOP-8** — Plugin hook dispatch holds the mutex synchronously; no documented timeout enforcement at this layer. `src/agent/agent_loop/plugin_hooks.rs:87-90, 177-180`. Wrap in `tokio::time::timeout`.
- **LOOP-9** — `bridge.last_text_emitted` never resets at turn boundary → all of turn 2's first text gets emitted as one big `Token` event, destroying streaming UX. `src/agent/agent_loop/bridge.rs:265-322`.
- **LOOP-10** — `tool_name_by_id` map grows unbounded when a tool call lacks a matching End event. Same file:340.
- **LOOP-11** — `ToolExecutionUpdate` uses `try_send`; updates silently dropped on fast tools. `src/agent/agent_loop/tools.rs:321-326`.
- **LOOP-12** — `scavenge_tool_calls` extracts tool calls from reasoning; dedupe key isn't canonical across `1` vs `1.0` / key order, can let duplicates slip in. `src/agent/agent_loop/run.rs:288-327`.

### Tools

- **TOOL-2** — `grep` `include` glob → regex doesn't escape regex metachars. `src/agent/tools/grep.rs:40-54`. Copy `glob.rs:57-88`'s escaper.
- **TOOL-3** — `apply_patch` doesn't enforce absolute paths (`write`/`edit`/`read` do). `src/agent/tools/apply_patch.rs:74-173`. Add `is_absolute()` early-return.
- **TOOL-4** — `websearch` returns external content verbatim → prompt-injection vector. `src/agent/tools/websearch.rs:68-89`. Wrap with "Search results (untrusted):" header. Same for `webfetch`.
- **TOOL-5** — `lsp` tool joins relative paths to cwd without canonicalizing — `..` traversal possible. `src/agent/tools/lsp.rs:217-232`. Use `Scope::PathResolve` or canonicalize.

### Session

- **SESS-6** — `pop_last_message` fallback leaves orphan tree entries. `src/session/mod.rs:587-602`. Always remove from `tree.entries` + `message_store`.
- **SESS-7** — `sanitize_fts5_query` step 4 strips only one leading boolean operator (`"AND OR foo"` → `"OR foo"` → FTS5 rejects). `src/extras/session_search.rs:502-508`. Loop until stable. Also: step 5 may mangle hyphenated terms inside preserved quoted phrases.
- **SESS-8** — Resume silently replays tool side effects from a session captured days ago in a different working tree. `src/session/mod.rs`. Warn when `working_dir` differs or `updated_at` >24h old.
- **SESS-9** — `compress_reporting` summarizer has no token budget; long sessions overflow the summarizer model's context. `src/provider/summarize.rs:73-135`.
- **SESS-10** — `reset_to_new` doesn't clear `loaded_mtime` / `current_prompt_name`. `src/session/mod.rs:739-770`.

### External Integrations

- **EXT-7** — `nearest_root` walks above `stop_at` when file lives outside the worktree → opens any `Cargo.toml` it finds up to `/`. `src/lsp/server.rs:36-67`. Assert file is a descendant of `stop_at`.
- **EXT-8** — Worktree branch validation permits names starting with `-` (looks like a git flag) and `..`. `src/extras/git_worktree/mod.rs:80-91`, `src/ui/slash.rs:990`. Use `--` separator in argv; reject leading `-`, `..`, `~`, `:`, control chars.
- **EXT-9** — `/wt-exit` doesn't check uncommitted changes; `/wt-merge` delegates to the LLM with no atomic guarantee. `src/ui/slash.rs:1077-1096`. Add `git status --porcelain` guard; provide `/wt-prune`.
- **EXT-10** — `find_callers` is regex-based; cross-language `Foo.bar()` qualifier semantics ignored. `src/semantic/index.rs:208`.
- **EXT-11** — MCP children: arbitrary code at connect-time; schemas trusted blindly; tool-name collisions with built-ins not detected. `src/extras/mcp/client.rs:136-158`, `src/extras/mcp/tool.rs:78-107`. Namespace MCP tool names; warn on collisions; document trust model.
- **EXT-12** — Backoff exponent could overflow `1u64 << exp` if cap is ever raised past 63. `src/lsp/manager.rs:118-121`. Compute and clamp on the Duration directly.

### UI / Plugins

- **UI-4** — Skill discovery walks every cwd ancestor, not just project roots. `src/skill.rs:91-106`. Restrict to `.git`-rooted ancestors.
- **UI-5** — Skill discovery follows symlinks. `src/skill.rs:44-50`. Use `symlink_metadata` and skip symlinks.
- **UI-6** — Paste buffer has no size cap; multi-MB pastes accumulate. `src/ui/input.rs:362-411`. Reject / truncate above ~1 MB.

---

## 5. LOW / INFO (abridged)

Lower-severity findings (cache-generation race, BOM stripping, FTS5 secret indexing, file-picker truncation, schema-downgrade silent dataloss, etc.) are detailed in the per-agent reports but not re-listed here. Notable mentions:

- **SESS-14** — FTS5 ingests raw tool outputs including secrets. `src/extras/session_db.rs:165-188`. Add `bd db purge` / redaction pass.
- **SESS-15** — Schema downgrade silently zeros newer fields and overwrites. `src/session/storage.rs:137-147`. Refuse to save when `file_version > SCHEMA_VERSION`.
- **SESS-13** — `/undo` does not revert file writes / bash side effects but says "removed N messages". Add warning when popped assistant message had tool calls.
- **TOOL-7** — bash output cap applied *after* full buffer in memory. `src/agent/tools/bash.rs:301-315`. Check inside the drain loop.
- **TOOL-15** — bash inherits full parent env; API keys reachable via `printenv` even under `--sandbox`. (Gated by permissions but worth surfacing in docs.)
- **UI-7** — File picker silently truncates at 200 results, no indication.

---

## 6. Suggested Fix Sequencing

1. **Security-critical, immediate:** PROV-1, TOOL-1, SESS-3, UI-1, UI-2, PERM-19, PERM-5, PERM-6
2. **Correctness regressions hiding in production:** LOOP-7/LOOP-18 (compaction no-op), SESS-2 (compression mis-advertised), EXT-3/EXT-4 (LSP stale diagnostics), LOOP-9 (streaming UX broken after turn 1)
3. **README/implementation drift:** PROV-2 (silent retries), LOOP-4 (interjection semantics), EXT-6 (`--prompt` flag), PERM-15 (block-form `deny_tools` silently empty)
4. **Robustness for tampered/large input:** SESS-1, LOOP-1, UI-6, PERM-1/PERM-2 (doom-loop)
5. **MCP/Plugin trust model:** EXT-11, UI-3, PERM-7, PROV-12

Each numbered finding above is intended to be one `bd create` issue. The cross-cutting themes in §1 are good candidates for design notes / formal `--design` entries on the parent epics.

---

## 7. Areas Not Audited (out of scope this pass)

- Build / Cargo features matrix correctness (cross-product of `acp`, `loop`, `git-worktree`, `mcp`, `semantic-*`, `plugin`)
- Windows-specific behavior (README acknowledges it's untested)
- Performance / memory benchmarks beyond stated heuristic claims
- Theme rendering and color-blind accessibility

---

*Generated 2026-05-26 from 7 parallel subagent audits totaling ~1.1M tokens of analysis.*
