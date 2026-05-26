# dirge: the coding agent that fits in your pocket and punches above its weight

Most coding agents are resource hogs. The market leader clocks in at ~300MB RAM just sitting idle. Dirge is written in Rust and weighing in at 25MB on disk and running at **13MB idle, ~30MB working**. You could run 20 copies of it and still use less memory than a single instance of the popular alternatives.

But the lean footprint is table stakes. What makes dirge worth a serious look is the combination of three things that no other agent is currently doing. Dirge has a genuinely pluggable embedded scripting system inspired by Pi, a DeepSeek-optimized tool harness, and a per-project learning architecture that gets smarter every time you use it.

## Janet: the plugin system coding agents actually need

TODO: rewrite this to say that most agents aren't terribly customizable and merely rely on MCPs to provide functionality, with Pi being one excaption. Explain that Dirge plugin system is modeled on Pi, and explain what makes Pi approach special https://pi.dev/
Most agent plugin systems fall into one of two camps. Either you get a config file with a handful of flags and callbacks, or you get an MCP server that requires running a separate process, managing a JSON-RPC transport, and accepting that your plugin will be invoked through the same LLM that's trying to solve your actual problem.

Dirge embeds [Janet](https://janet-lang.org) which is a small, embeddable Lisp directly into the agent process. Plugins are `.janet` files you drop into `~/.config/dirge/plugins/` or `./.dirge/plugins/`. They run on a dedicated worker thread, separate from the agent loop and the UI, so a misbehaving plugin can't starve your session.

### Why Janet?

Janet is the spiritual successor to Lua, designed for exactly this job:

- **The entire language fits in ~1MB.** The `janetrs` crate embeds the VM without ceremony. It doesn't need dynamic linking or shared libraries, and has a negligible startup cost.
- **S-expressions all the way down.** Plugin hooks receive and return structured data that looks exactly like the code you're writing. There's no impedance mismatch between configuration and code is data.
- **Single-threaded by design.** No GIL, no race conditions, no synchronization headaches. One VM, one worker thread, serialized hook dispatch. You get the safety of immutability without the ceremony of channels and mutexes.
- **Batteries for scripting.** PEG macros for parsing, fibers for cooperative concurrency, destructuring for clean data extraction — Janet is a real programming language, not a watered-down DSL.

A minimal plugin looks like this:

```janet
(defn on-prompt [ctx]
  (when (string/find "security" (ctx :prompt))
    (harness/notify "running with security mindset" :info)))
```

But the full harness API is a proper operating surface. Plugins can:

- **Intercept tool calls:** block, mutate-input, or replace-result on any tool before or after it runs. First-blocker-wins for deny rules; last-write-wins for transformations.
- **Register slash commands:** drop a `/deploy` or `/review` command that runs arbitrary Janet logic.
- **Register custom tools:** with parameter schemas, execution modes, and argument preprocessors — the LLM sees them as first-class tools.
- **Register custom LLM providers:** point Janet at your local vLLM/Ollama/LMStudio endpoint.
- **Post notifications and render custom entries:** typed bookmarks, timers, whatever structured output you want in the chat.
- **Control the session tree:** fork, label, navigate, and create new sessions programmatically.
- **Open blocking dialogs:** `harness/confirm` and `harness/select` that pause the worker thread until the user responds — sync, not callback spaghetti.

There are 11 lifecycle hooks: `on-init`, `on-prompt`, `on-response`, `on-turn-start`, `on-turn-end`, `on-message-update`, `on-tool-start`, `on-tool-end`, `on-error`, `on-complete`, `prepare-next-run`. A multi-file plugin (directory of `.janet` files loaded in lexicographic order into a shared environment) can orchestrate complex workflows across them all.

The existing example plugins demonstrate the range: workflow orchestration (architect → implementor → review via inversion of control), path protection (deny writes to critical paths), destructive-command confirmation, persona selection, turn timing, and even local LLM provider registration — all in under 100 lines of Janet each.

This is what the Pi project got right about extensibility: if you expose the agent lifecycle as hooks and give users a real language, they'll build things you never anticipated. dirge is the first coding agent to inherit that design.

---

## How we made DeepSeek beat Opus 4.7

"Open-source models are bad at tool calling." You've heard this. The conventional wisdom says that if you want reliable tool use, you pay for a commercial model that internalized every API contract during pretraining.

We spent two days looking at billions of tokens through a sibling CLI and came to a different conclusion: **"bad at tool calling" is almost always a harness problem, not a model problem.**

By the end of that investigation, DeepSeek v4-pro was beating Opus 4.7 on our internal evals 6 out of 10 times. The model didn't change. The harness got more forgiving in the places it needed to be.

### The four shape failures

Across DeepSeek-flash, DeepSeek v4-pro, GLM, and Qwen, the same four mistakes repeat with almost identical distribution:

1. **`null` for optional field** — emitting `{"path": "x", "offset": null}` instead of omitting `offset`
2. **JSON-string instead of array** — emitting `{"paths": "[\"a\",\"b\"]"}` as a string containing JSON
3. **Empty placeholder object instead of array** — emitting `{"items": {}}` when the schema wants an array
4. **Bare string instead of array-of-string** — emitting `{"paths": "foo"}` instead of `["foo"]`

Four repairs, ~30-100 lines each. That's the whole catalogue. When someone says "this open model can't do tool calls," I now assume one of those four — and so far, that's been right about 90% of the time.

### Order matters (we learned this the hard way)

JSON-string parse (#2) must run BEFORE bare-string wrap (#4), or `"[\"a\",\"b\"]"` becomes `["[\"a\",\"b\"]"]` — a singleton array containing the original JSON string instead of the intended two-element array. It cost us a debugging session. It's now pinned by a unit test.

### The funniest failure is also the most revealing

DeepSeek-flash, when asked to edit a file, sometimes emits the path as a markdown auto-link:

```
filePath: "/Users/x/proj/[notes.md](http://notes.md)"
```

Our write tool obediently created files literally named `[notes.md](http://notes.md)`. This isn't a hallucination — it's the post-training chat distribution leaking through the tool boundary. The model was rewarded for auto-linking in conversational output and is applying that prior in a context where it makes no sense. The fix is two regex lines that unwrap only the degenerate case where link text equals url-without-protocol. Real markdown like `[click](https://example.com)` passes through untouched.

**"Tool confusion" is a more useful frame than "capability gap."** The model knows how to format a path. It just hasn't been told clearly enough that this path is going to `fopen`, not into a chat bubble. So we encode that hint at the schema level — `pathString()` instead of `z.string()` — and the leak is plugged for every path field at once.

### Validate-then-repair, not preprocess-then-validate

The naive approach is a preprocessing pass: walk the args, strip nulls, parse stringified arrays, then validate. This is wrong. When you preprocess, you encode a prior about what's broken and apply it even when nothing is. The sibling CLI tried it — valid inputs whose `content` field happened to be JSON-shaped got rewritten before hitting disk. Silent corruption, easy to miss in a smoke test.

The right design inverts the order:

1. **Try the input as-is.** If it parses, ship it. Valid inputs are never touched.
2. **On failure, localize.** Walk the validator's issue list — each issue has a path (`/items/0/path`) and a complaint (`expected array, found string`). For each issue path, try the four repairs in order until one applies at that specific path.
3. **Retry once.** On success, log `tool_input_repaired`. On failure, log `tool_input_invalid` and return a model-readable retry message.

The validator is doing the work of localizing the bug for you. You spend repair budget only at the exact paths the schema disagreed at. This also gives you per-tool telemetry for free: you can watch repair rates per (model, tool) and notice when a model regresses on a specific contract before users do.

### Relational invariants need different fixes

Shape repairs handle wrong-type/missing-key/wrong-container problems. But `read_file` has a *relational* invariant: "if you provide offset, you must also provide limit, and vice versa." DeepSeek kept calling `read_file({path, limit: 30})` and getting an error back. You can't fix this with input repair because each field is independently valid — the bug is in the relationship.

So we taught the function the model's intent instead. `limit` alone → `offset = 0`. `offset` alone → `limit = 2000`. Then surface the decision in the result:

> Note: limit was not provided; defaulted to 2000 lines. To read more or fewer, retry with both offset and limit.

No `Error:` prefix, so the TUI doesn't paint it red. The model sees what we picked and can self-correct on the next turn if our guess was wrong.

**Repair where you can. Extend semantics where you can't. Surface the choice either way.**

### Beyond repair: ports from DeepSeek-Reasonix

The repair layer handles malformed tool calls. But making DeepSeek perform well also required structural changes to how the agent loop itself works. dirge ports several components from [DeepSeek-Reasonix](https://github.com/esengine/DeepSeek-Reasonix) — the open-source agent loop designed specifically for DeepSeek's strengths and quirks:

- **Schema flattening** — DeepSeek models handle flat parameter lists far more reliably than nested JSON. dirge flattens tool schemas at construction time (the LLM sees `path`, `content`, `offset`, `limit` as flat parameters) and re-nests them at dispatch. This eliminates a whole class of "expected object, got string" failures at the schema level before the repair layer ever fires.

- **Tool call scavenging** — when DeepSeek outputs a tool call in its reasoning text but fails to emit it in the structured `tool_calls` field, dirge scavenges the reasoning content with regex extraction, matched against allowed tool names. This recovers tool calls the model formed correctly but failed to serialize — a common failure mode specific to DeepSeek's reasoning-first architecture.

- **Storm mode** — tracks a consecutive tool-call failure counter. When it crosses a threshold, the system enters "storm mode": tool results are truncated, the retry budget tightens, and the model is nudged to try a different approach instead of looping.

- **Multi-tier compaction** — ported from Reasonix's context manager, dirge estimates token fold at each turn start and decides whether to compress mid-turn context or let it ride. The flash-first strategy uses a cheap model for context management decisions so the main model's prompt cache stays intact.

- **Mid-turn steering** — when the user interjects mid-run, dirge wraps the steering message in a format the model recognizes as an override, not another conversational turn. This is critical for DeepSeek which, unlike commercial models, doesn't natively handle "ignore everything before this" semantics well.

- **Reasoning effort control** — DeepSeek v4-pro supports reasoning effort levels (`low`, `medium`, `high`). dirge wires this through the OpenAI-compatible `reasoning` parameter path, letting users dial reasoning up for complex debugging sessions and down for mechanical edits.

### Error formatting: teach the machine what went wrong

When the repair layer fails, the model needs an actionable message — not `missing field 'path' at line 1 column 10`. That's a parser's error; the model can't repair from it because it carries no schema context.

dirge formats tool errors as structured hints:

```
Tool input rejected: the `path` field is required but was missing
Expected: { "path": string, "content": string? }
Got:      { "content": "..." }
Try:      add a `path` field with the absolute file path, e.g. `/Users/.../file.txt`
```

The model recovers from this far more reliably than from `expected ',' or '}' at line 3 column 25`.

### The frame shift

A lot of what looks like model capability is actually contract design. A strict schema is a choice with a cost — it filters out noise, but it also filters out recoverable noise from any model that hasn't memorized your exact JSON contract. The largest commercial models eat that cost invisibly because they've seen enough of every contract during pretraining. Open models pay it loudly and get dismissed for it.

The harness is where you mediate between distributions. Four small repairs, two regex lines for auto-links, one relational default, schema flattening, tool call scavenging — **the model didn't change.** The contract got more forgiving in exactly the places it needed to be.

---

## The learning loop: an agent that remembers

Most coding agents are amnesiac. Every session starts from scratch. The agent doesn't remember that your project uses `eslint-config-custom` and not `@company/eslint-config`, or that the mock server for integration tests needs `--feature=test-utils` to start, or that you spent 45 minutes last week debugging a race condition in the auth middleware.

dirge is building a per-project learning architecture that changes this. It's a full port of Hermes-agent's four-layer memory system, adapted for the coding context and stored entirely in `.dirge/` at your project root — so each project builds its own independent knowledge base.

Here's the architecture:

```
┌─────────────────────────────────────────────────┐
│ Layer 4: Curator (periodic skill maintenance)    │
│ Lifecycle transitions, consolidation, archiving  │
├─────────────────────────────────────────────────┤
│ Layer 3: Skill System (procedural memory)        │
│ CRUD for "how to do X in this project"           │
├─────────────────────────────────────────────────┤
│ Layer 2: Memory Store (declarative memory)       │
│ Project facts + anti-patterns that were tried    │
├─────────────────────────────────────────────────┤
│ Layer 1: Background Review (the learning nudge)  │
│ Fork at session end, evaluate what was learned   │
├─────────────────────────────────────────────────┤
│ Foundation: Session DB + Search + Compression    │
│ SQLite + FTS5 + structured summaries             │
└─────────────────────────────────────────────────┘
```

All of it lives in `.dirge/` at your project root:

```
.dirge/
├── memory/
│   ├── MEMORY.md         # Build commands, conventions, architecture
│   └── PITFALLS.md       # "Don't use async here because X"
├── skills/               # Procedural knowledge
│   ├── project-build/
│   │   └── SKILL.md      # How to build, test, lint
│   ├── project-architecture/
│   │   └── SKILL.md      # Module map, invariants, patterns
│   └── .archive/         # Curator-archived (never deleted)
├── sessions/
│   └── state.db          # SQLite with FTS5 full-text search
└── config.yaml
```

### How it works, layer by layer

**Session database with FTS5.** Every session transcript is persisted in SQLite with full-text search. This means the agent can search its own history: "How did we solve the database migration issue last month?" The search tool has three calling shapes — discovery (FTS5 query with bookends), scroll (anchored window around a message), and browse (recent sessions chronologically). No LLM cost, pure SQLite. Lineage deduplication ensures that sessions split by compression don't clutter results.

**Memory store: what we know and what we learned not to do.** Two markdown files with `§` section delimiters. `MEMORY.md` accumulates project facts — build commands, naming conventions, architecture patterns, library quirks. `PITFALLS.md` is the anti-knowledge base — things tried and failed, environment-specific issues, test fixtures that misbehave. A frozen snapshot is captured at session start and injected into the system prompt. Mid-session writes go to disk immediately but never touch the prompt, preserving the LLM's prefix cache. The design is battle-tested: injection scanning, drift detection (external modification via another process), atomic writes via tempfile+rename, `fcntl.flock` for concurrent writer serialization.

**Skill system: procedural memory for the project.** Skills capture "how to do this class of task for this specific codebase." The agent creates and improves them through experience. Each skill is a directory with `SKILL.md` (YAML frontmatter + markdown body) and optional supporting files under `references/`, `templates/`, `scripts/`. The CRUD surface includes a fuzzy-match patching system — LLM-generated `old_string` values often have minor formatting mismatches with actual file content, so the patcher normalizes whitespace, handles indentation differences, and uses block-anchor matching for disambiguation.

**Background review: the intake valve.** At session end, dirge forks the agent with limited tools (memory + skill management only) and asks it to evaluate what was learned about the project. The fork runs autonomously on a separate thread — writes land in the memory and skill stores, the main session continues unaffected. The review prompt is coding-specific: "What build/test commands were discovered?" "Were there user corrections about how things should be done?" "Did a loaded skill turn out wrong or missing steps?" "Nothing to save" is valid but not the default — most coding sessions produce at least one learning.

**Curator: the janitor.** Every 7 days (configurable), the curator reviews agent-created skills. Stale skills (30 days inactive) get flagged. Very stale skills (90 days) move to `.archive/` — never deleted, always recoverable. The curator can consolidate overlapping skills into umbrella skills and patch outdated ones. It never touches bundled/shipped skills. Pinned skills bypass all transitions.

**Context compression.** When a long session approaches the model's context limit, the middle turns are compressed via an auxiliary (cheaper/faster) model. The result is a structured summary with resolved questions, pending questions, active task, key decisions, and remaining work — prefixed with a filter-safe preamble that tells the model "this is reference, not active instructions." The session splits on compression, creating a lineage chain that session search uses for deduplication.

### Why this matters

A coding agent that learns per-project changes the economics of long-running projects. The first session on a new codebase is exploratory — it discovers build commands, maps module structure, learns conventions. Without memory, every subsequent session repeats that exploration. With the learning loop, session two picks up where session one left off, and by session ten the agent knows your project almost as well as you do.

The per-project design is deliberate. Knowledge that's true for your Rust project isn't true for your Python project. Global memory conflates them. Per-project memory keeps them separate, contextual, and relevant.

---

## The rest of the package

dirge ships with everything you'd expect from a modern coding agent and a few things you wouldn't:

- **Multi-provider** — OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, GLM, Ollama, plus custom OpenAI-compatible endpoints
- **20+ built-in tools** — read, write, edit, bash, grep, find_files, glob, apply_patch, webfetch, websearch, and more
- **LSP integration** — attaches real language servers (rust-analyzer, typescript, pyright, gopls, clojure-lsp, jdtls, clangd, ruby-lsp) and surfaces compile errors on the same turn the agent writes code
- **Tree-sitter semantic tools** — list_symbols, get_symbol_body, find_callers, find_callees across 10 languages, all AST-powered
- **Permission system** — four modes, per-tool glob patterns, session allowlists, doom-loop detection
- **Git worktrees** — branch-per-task workflow with `/worktree`, `/wt-merge`, `/wt-exit`
- **Subagent support** — `task` tool spawns isolated research or analysis subagents
- **Session tree** — fork, branch, and navigate conversation history
- **Mid-execution interjection** — type while the agent is running to queue a follow-up
- **Inline ASCII avatar** — a 5-cell face that reflects what the agent is doing right now (yes, it's silly; yes, it's genuinely useful for keeping track of what's happening)

The semantic code tools deserve a special mention. When you pass `--features semantic,semantic-ts,semantic-python` (and friends), the agent gets AST-level code analysis that most agents charge premium tiers for: find all call sites of a function, extract a symbol's body by precise byte range, list every export in a project. It's tree-sitter all the way down — no LSP required for these queries, no server startup, no workspace initialization. Just fast, indexed traversal.

---

**dirge is open source (GPL-3.0).** Install with `cargo install dirge`, set your API key, and go. The Janet plugin system, the DeepSeek repair layer, and the per-project learning loop are all opt-in — the default experience is a fast, capable coding agent. The depth is there when you need it.

If you're tired of agents that eat half a gig of RAM before doing anything useful, or if you've been burned by open models that "can't do tool calls," or if you want an agent that actually gets better the more you use it — give dirge a try.

[https://github.com/yogthos/dirge](https://github.com/yogthos/dirge)
