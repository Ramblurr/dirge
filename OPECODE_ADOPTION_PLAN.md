# Opencode Feature Adoption Plan

Features ranked by agent-workflow impact, with implementation phases.

---

## Phase 1: `feature/question-tool` — user clarification mid-execution

**Source:** `src/agent/tools/question.rs` (new), `src/ui/` (renderer changes), `src/permission/`

### Problem

The system prompt says "If you have doubts or need clarification, ask the user directly. Do not guess or assume." — but there is no tool to do this. The agent must guess, leading to wrong implementations and wasted turns.

### What opencode has

A `question` tool the agent calls with structured questions. The tool blocks until the user answers, then returns the answers as structured data.

Question schema per-item:
```
question: string           // the question text
header: string (optional)  // section heading
options: [{ label, description }] // choices
multi_select: bool         // allow multiple picks
custom: bool (default true) // add "type your own" option
```

The answer comes back as `answers: string[][]` — one array of selected labels per question.

### Implementation

**New tool: `src/agent/tools/question.rs`**

```
question tool:
  parameters:
    questions: array of { question, header?, options, multi_select?, custom? }
  
  on invoke:
    1. check permission
    2. send AgentEvent::QuestionAsked { id, questions } to UI
    3. wait on a oneshot channel for the user's reply
    4. if rejected, return error (tool fails, agent can react)
    5. if answered, format answers back to agent
```

**UI changes: `src/ui/mod.rs`**

- Listen for `AgentEvent::QuestionAsked`
- Enter a modal question prompt loop (similar to permission ask but for questions)
- Render the question list with selectable options
- Keyboard: arrow keys + Enter to select, Tab to move between questions in multi-q mode
- Return reply via the oneshot channel

**Event additions: `src/event.rs`**

```rust
AgentEvent::QuestionAsked {
    id: CompactString,
    questions: Vec<QuestionItem>,
}

AgentEvent::QuestionReply {
    id: CompactString,
    answers: Vec<Vec<String>>,
}
```

### No external dependencies needed.

### Tests

- Question tool sends event and blocks until reply
- Single-select question returns picked label
- Multi-select question returns array of picked labels
- Custom=true adds "Type your own" option
- Reject returns error to agent
- UI renders questions with proper highlighting

---

## Phase 2: `feature/web-tools` — web search and fetch

**Source:** `src/agent/tools/websearch.rs`, `src/agent/tools/webfetch.rs` (new)

### Problem

The agent can't look up current docs, API references, or debug with up-to-date info. Everything beyond training cutoff is invisible.

### What opencode has

`websearch` — searches via exa/parallel MCP backends, returns structured results
`webfetch` — fetches a URL and converts to markdown/text/html

### Implementation

**`websearch` tool**

```
parameters:
  query: string
  num_results: number (default: 10)
  
implementation:
  - Use exa API (simplest — single HTTP POST, no MCP overhead)
  - Requires EXA_API_KEY env var
  - POST https://api.exa.ai/search with query + numResults
  - Parse response, return as structured text
```

**`webfetch` tool**

```
parameters:
  urls: string[]
  max_chars: number (default: 3000)

implementation:
  - Use exa's contents endpoint or direct HTTP fetch with html2text
  - Return page content as markdown
  - HTTP URLs auto-upgrade to HTTPS
```

### Configuration

```json
{
  "tools": {
    "websearch": { "enabled": true },
    "webfetch": { "enabled": true }
  }
}
```

Both disabled by default. Gate on env var `WEBSEARCH_ENABLED=true` or config.

### Dependencies

- `reqwest` (already in Cargo.toml for MCP?)
- `html2text` crate for webfetch fallback

### Tests

- Websearch returns structured results for valid query
- Websearch handles API errors gracefully
- Webfetch retrieves and converts page content
- Webfetch handles 404s and timeouts
- Tools disabled without config/env produce clear error

---

## Phase 3: `feature/background-tasks` — async subagent execution

**Source:** `src/agent/tools/task.rs` (modify), `src/agent/tools/task_status.rs` (new)

### Problem

The current `task` tool is synchronous — the agent blocks until the subagent finishes. Can't run research in parallel with implementation. Context window gets bloated with subagent results.

### What opencode has

`task(background=true)` — launches subagent asynchronously, returns a `task_id` immediately
`task_status(task_id, wait=false)` — polls for result, or `wait=true` to block until done
Background results injected as synthetic user messages when the main thread is idle

### Implementation

**Modify `task` tool**
- Add `background: Option<bool>` parameter
- When `background=true`, spawn subagent in a tokio task, store handle in a `HashMap<String, JoinHandle>`, return `task_id` + "state: running"
- When `background=false` (default), existing synchronous behavior

**New `task_status` tool**
- `task_id: string` (required)
- `wait: bool` (default false)
- Polls the background task store
- Returns: `task_id`, `state` (running/completed/error/cancelled), and result if done

**Background task store: `src/agent/tools/background.rs`**

```rust
type BackgroundStore = Arc<Mutex<HashMap<String, BackgroundTask>>>;

struct BackgroundTask {
    handle: JoinHandle<Result<String, String>>,
    state: TaskState,
}

enum TaskState { Running, Completed(String), Failed(String), Cancelled }
```

The agent builder creates a `BackgroundStore` and passes it to task/task_status tools.

### Tests

- Background task returns running state immediately
- task_status reports completed when done
- task_status with wait=true blocks until completion
- Multiple concurrent background tasks tracked independently
- Cancelled tasks show cancelled state

---

## Phase 4: `feature/structured-compaction` — better context summaries

**Source:** `src/agent/prompt.rs` (modify COMPACTION_PROMPT)

### Problem

Current compaction prompt is loose. The output varies in quality and machine-usability.

### What opencode has

A structured template:

```
## Goal
## Constraints & Preferences
## Progress (Done / In Progress / Blocked)
## Key Decisions
## Next Steps
## Critical Context
## Relevant Files
```

### Implementation

Replace `COMPACTION_PROMPT` with structured template. No code changes beyond the prompt string.

```
You are a conversation summarizer. Distill the following into these sections:

## Goal
The user's explicit objective. One sentence.

## Progress
- **Done:** concrete items completed with file paths
- **In Progress:** what was being worked on
- **Blocked:** what's preventing progress and why

## Key Decisions
Decisions made, alternatives rejected, and rationale.

## Next Steps
Ordered list of what to do next. Include exact commands and file paths.

## Relevant Files
List each file with a one-line description of its role.

## Critical Context
Facts, constraints, error messages, or environment details needed to resume.
```

### Tests

- Compaction produces all required sections
- Sections are parseable/structured

---

## Phase 5: `feature/plan-tools` — agent-initiated mode switches

**Source:** `src/agent/tools/plan_enter.rs`, `src/agent/tools/plan_exit.rs` (new)

### Problem

Plan mode is currently user-initiated only. The agent can't say "this is complex, let me plan first."

### What opencode has

`plan_enter` — agent calls this to suggest switching to plan mode. Asks user via question mechanism.
`plan_exit` — agent calls this when plan is ready, suggests switching to build mode.

### Implementation

These are thin wrappers around the existing prompt-switching infrastructure. When called:

1. `plan_enter`:
   - Uses the question tool internally to ask: "Switch to plan mode? [Yes/No]"
   - If yes, triggers prompt reload with `plan.md` prompt
   - Agent re-initializes with plan system prompt

2. `plan_exit`:
   - Writes PLAN.md if it doesn't exist
   - Asks: "Plan ready. Switch to implementation? [Yes/No]"
   - If yes, triggers prompt reload with `code.md` prompt

Requires adding a mechanism to trigger prompt reload mid-session (currently prompt is set at session start). Could be done via `AgentEvent::SwitchPrompt(String)` that the runner handles on next iteration.

### Tests

- plan_enter asks user and switches prompt on yes
- plan_exit writes PLAN.md and switches prompt on yes
- Both handle "no" gracefully (tool returns, agent continues)

---

## Phase 6: `feature/apply-patch` — multi-file edits in one call

**Source:** `src/agent/tools/apply_patch.rs` (new)

### Problem

Cross-cutting changes across multiple files require one `edit` call per file, bloating turn count.

### What opencode has

Custom patch format supporting create + update + delete + rename in a single tool call. The patch is a structured JSON with operations.

### Implementation

```
apply_patch tool:
  parameters:
    operations: array of {
      action: "create" | "update" | "delete" | "rename"
      path: string
      content: string (for create/update)
      new_path: string (for rename)
      old_text: string (for update — exact match to replace)
      new_text: string (for update — replacement)
    }

  execute in order, stop on first failure
  return summary of results per file
```

### Tests

- Create file that doesn't exist
- Update file with exact text match
- Rename file
- Delete file
- Multiple operations in one call
- First failure stops remaining operations
- Atomicity: failure leaves files in correct state

---

## Phase 7: `feature/glob-tool` — ergonomic file matching

**Source:** `src/agent/tools/glob.rs` (new)

### Problem

`find_files` uses regex on filenames. Glob patterns (`**/*.rs`, `src/**/*.tsx`) are more natural for path matching.

### What opencode has

A `glob` tool that accepts a standard glob pattern and returns matching file paths.

### Implementation

Use `glob` crate (already possibly in Cargo.toml).

```
glob tool:
  parameters:
    pattern: string  // e.g. "src/**/*.rs"
    path: string?    // root directory (default: cwd)

  respects .gitignore same as find_files
  returns file list sorted by path
```

Make this an alias/supplement — keep `find_files` for regex use cases.

### Tests

- `**/*.rs` finds all Rust files recursively
- `src/agent/**/*.rs` finds files in agent subtree
- Empty results for non-matching pattern
- Respects .gitignore

---

## Phase 8: `feature/agent-reminders` — context injection on mode switch

**Source:** `src/agent/builder.rs` (modify)

### Problem

When switching between plan/build agents, there's no synthetic context to tell the agent what's expected. The agent rediscoveres the plan file path on each context switch.

### What opencode has

Synthetic user/system messages injected when switching modes:
- "A plan file exists at X. Execute the plan."
- "You are now in plan mode. Create a plan at PLAN.md."

### Implementation

When reloading prompt after mode switch, inject a synthetic system message at the top of history:

```rust
match new_prompt_mode {
    "plan" => inject_context("Create a detailed implementation plan in PLAN.md..."),
    "code" if plan_file_exists() => inject_context(&format!("A plan file exists at {plan_path}. Execute the plan step by step.")),
    _ => {}
}
```

### Tests

- Plan prompt switch injects planning reminder
- Code prompt switch with existing PLAN.md injects execution reminder
- Code prompt switch without PLAN.md injects nothing

---

## Implementation Order

```
question-tool → web-tools → background-tasks → structured-compaction
  → plan-tools → apply-patch → glob-tool → agent-reminders
```

### Rationale

1. **question-tool** — closes the biggest agent workflow gap. No external dependencies. Immediate quality-of-life improvement.
2. **web-tools** — second biggest capability gap. Enables current docs, debugging, API reference lookup.
3. **background-tasks** — enables parallel work, reduces context bloat. Dependent on task tool architecture being solid.
4. **structured-compaction** — pure prompt change, no code risk. Improves context quality across all sessions.
5. **plan-tools** — builds on question-tool (uses it internally). Enables agent-initiated planning.
6. **apply-patch** — reduces round-trips for multi-file changes. Requires careful implementation of operation ordering.
7. **glob-tool** — ergonomic improvement, low priority compared to capability gaps above.
8. **agent-reminders** — minor context injection. Nice to have but low impact.

### Each phase

- Is a single git branch
- Includes TDD tests
- Is independently mergeable (no ordering dependency except plan-tools → question-tool)
