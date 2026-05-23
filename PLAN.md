# Port pi's agent loop to dirge — faithful, phased, TDD

## Scope statement (correcting the earlier plan)

Earlier draft scoped only `prepareNextTurn`. The actual scope is **the
entire pi `runLoop` and surrounding machinery**, ported as closely as
the language difference allows. Pi is the reference; dirge is the
target. Don't invent; port.

Pi's source of truth:
- `~/src/pi/packages/agent/src/agent-loop.ts` (742 LOC — the loop)
- `~/src/pi/packages/agent/src/types.ts` (418 LOC — the type surface)
- `~/src/pi/packages/agent/test/agent-loop.test.ts` (1351 LOC — the spec)

The 21 test cases in pi's `agent-loop.test.ts` are THE SPEC. Every
phase maps to one or more of those tests, ported to Rust and adapted
to dirge's tool/agent abstractions.

## What we're replacing

Currently dirge uses rig's `MultiTurnStream` for the inner loop. Rig
owns turn iteration, tool dispatch, and history management. Dirge
only observes events emitted by rig. This means we CANNOT:

- Swap model/thinking between turns (rig commits both for the stream)
- Inject steering messages between turns
- Apply `prepareNextTurn` / `shouldStopAfterTurn` semantics
- Override tool results via `afterToolCall` cleanly
- Honor `terminate` hints on individual tool results
- Run tools in parallel
- Distinguish sequential-only tools (e.g. bash) from parallel-safe ones
- Provide dynamic API key resolution per request

After the port, dirge owns the loop. Rig provides only the single-turn
LLM call.

## Pi's surface area (what we port)

### Types

| Pi type | Port target |
|---|---|
| `AgentEvent` (12 variants) | `event::AgentEvent` (extend existing) |
| `AgentContext` | `agent::loop::Context` |
| `AgentLoopConfig` | `agent::loop::LoopConfig` |
| `AgentLoopTurnUpdate` | `agent::loop::TurnUpdate` |
| `ShouldStopAfterTurnContext` / `PrepareNextTurnContext` | `agent::loop::TurnHookContext` |
| `BeforeToolCallContext` / `BeforeToolCallResult` | `agent::loop::BeforeToolHook` shapes |
| `AfterToolCallContext` / `AfterToolCallResult` | `agent::loop::AfterToolHook` shapes |
| `AgentTool` (with `executionMode`, `prepareArguments`) | extend `rig::Tool` wrapper |
| `AgentToolResult<T>` (with `terminate`) | new `loop::ToolResult` |
| `ThinkingLevel` | new `event::ThinkingLevel` |
| `ToolExecutionMode` | new `loop::ToolExecutionMode` |
| `QueueMode` | new `loop::QueueMode` |

### Hooks (config callbacks)

| Pi hook | Port (plugin slot OR rust trait) |
|---|---|
| `convertToLlm` | rust closure — converts dirge messages → rig messages |
| `transformContext?` | rust closure — pruning / compression |
| `getApiKey?` | rust closure — dynamic key resolution |
| `shouldStopAfterTurn?` | plugin: `harness/stop-after-turn` + rust hook |
| `prepareNextTurn?` | plugin: `prepare-next-turn` (existing alias) + rust hook |
| `getSteeringMessages?` | UI's interjection_queue + plugin slot |
| `getFollowUpMessages?` | follow-up queue (new) + plugin slot |
| `beforeToolCall?` | existing `on-tool-start` + rust hook |
| `afterToolCall?` | existing `on-tool-end` + rust hook (extend to support full override) |

### Algorithm phases (the loop)

This is the LITERAL algorithm from `runLoop`:

```
runLoop:
  pendingMessages = getSteeringMessages() OR []
  firstTurn = true

  OUTER:
    hasMoreToolCalls = true
    INNER while hasMoreToolCalls OR pendingMessages.nonEmpty():
      if !firstTurn: emit turn_start; else firstTurn = false

      // Inject queued user messages
      drain pendingMessages → emit message_start/end; append to context

      // LLM call
      message = streamAssistantResponse(context, config, signal)
      append message to newMessages

      if message.stopReason in ["error", "aborted"]:
        emit turn_end (toolResults=[]); emit agent_end; return

      // Dispatch tools
      toolCalls = filter(message.content, type=toolCall)
      toolResults = []; hasMoreToolCalls = false
      if toolCalls.nonEmpty():
        batch = executeToolCalls(context, message, config, signal)
        toolResults = batch.messages
        hasMoreToolCalls = !batch.terminate
        for r in toolResults: append to context, newMessages

      emit turn_end (message, toolResults)

      // prepareNextTurn — model/thinking/context swap
      snapshot = config.prepareNextTurn?(ctx)
      if snapshot:
        context = snapshot.context ?? context
        config.model = snapshot.model ?? config.model
        config.reasoning = (snapshot.thinkingLevel undefined ? config.reasoning
                            : snapshot.thinkingLevel == "off" ? None
                            : snapshot.thinkingLevel)

      // shouldStopAfterTurn — graceful stop
      if config.shouldStopAfterTurn?(ctx) == true:
        emit agent_end; return

      // Refresh steering for next iter
      pendingMessages = getSteeringMessages() OR []

    // OUTER: poll follow-up queue
    followUp = getFollowUpMessages() OR []
    if followUp.nonEmpty(): pendingMessages = followUp; continue OUTER
    break OUTER

  emit agent_end
```

### Tool execution (executeToolCalls)

```
prepareToolCall(toolCall):
  tool = lookup by name → if missing: immediate error
  args = tool.prepareArguments?(toolCall.args) ?? toolCall.args   // compat shim
  args = validateAgainstSchema(tool, args)                        // throws → error
  before = config.beforeToolCall?(ctx)
  if signal.aborted: immediate "Operation aborted"
  if before?.block: immediate (reason or default blocked msg)
  return prepared

executePreparedToolCall(prepared):
  result = await tool.execute(id, args, signal, onUpdate)
  // onUpdate emits tool_execution_update events
  catch → error result

finalizeExecutedToolCall(prepared, executed):
  after = config.afterToolCall?(ctx)
  if after: result = { content: after.content ?? result.content,
                       details: after.details ?? result.details,
                       terminate: after.terminate ?? result.terminate }
            isError = after.isError ?? isError
  catch → error result

executeToolCallsSequential / executeToolCallsParallel:
  // Sequential = await per call; Parallel = Promise.all on prepared lambdas
  // Per-tool executionMode=sequential forces sequential
  // emit tool_execution_start BEFORE prepare; tool_execution_end AFTER finalize
  // emit message_start/end for tool-result message AFTER (parallel: in source
  //   order even if finalize completed out-of-order)
  return { messages, terminate: every result has terminate==true }
```

---

## Phasing

Each phase ships green tests + green build. Tests are ported directly
from pi's `agent-loop.test.ts`. Each test name in this plan corresponds
to a `it("should …")` in pi's file.

### Phase 0 — Scaffolding (no behavior change) ✅

**Goal**: introduce the new types and the empty new-loop module
behind a feature flag `new-loop`. Nothing uses them yet; default
build is unchanged.

**Files**:
- `src/agent/agent_loop/mod.rs` (new) — empty module, public types
- `src/agent/agent_loop/types.rs` — `Context`, `LoopConfig`, `TurnUpdate`,
  hook context structs, `ThinkingLevel`, `ToolExecutionMode`, `QueueMode`
- `src/agent/agent_loop/tool.rs` — `LoopTool` trait with
  `execute(id, args, signal, on_update)`, `prepare_arguments`, `execution_mode`
- `src/agent/agent_loop/result.rs` — `LoopToolResult { content, details, terminate }`
- `Cargo.toml` — `new-loop = []` feature

**Tests** (pure type-level): roundtrip serde of `ThinkingLevel`,
`ToolExecutionMode`, default values.

**Risk**: zero. Code is unreachable until phase 3.

---

### Phase 1 — `streamAssistantResponse` analog ✅

**Goal**: single-turn LLM call wrapper around `rig::agent::Agent::prompt`
that produces an `AssistantMessage`-equivalent + emits dirge events.
The leaf of pi's loop.

**Files**:
- `src/agent/agent_loop/stream.rs` (new) — `stream_assistant_response`
- Reuse existing event emit logic; emit `Token`, `Reasoning`, etc.
- Resolve API key dynamically via `LoopConfig::get_api_key`
- Apply `transform_context` if configured
- Apply `convert_to_llm` (Required)
- Return a `FinalAssistantMessage { content, stop_reason, error_message }`

**Tests (port from pi)**:
- `should emit events with AgentMessage types` (line 84) — single LLM
  call emits start → updates → end
- `should handle custom message types via convertToLlm` (131) —
  convertToLlm filters/maps non-LLM message types
- `should apply transformContext before convertToLlm` (186) —
  transform sees the raw transcript first

**Risk**: medium. Bridge from rig's single-turn API to pi's
event vocabulary. Needs a mock rig agent for tests.

---

### Phase 2 — Tool execution: sequential ✅

**Goal**: port `executeToolCallsSequential` + `prepareToolCall` +
`executePreparedToolCall` + `finalizeExecutedToolCall`. Wires
beforeToolCall, afterToolCall, terminate, prepareArguments.

**Files**:
- `src/agent/agent_loop/tools.rs` (new) — sequential dispatcher
- `src/agent/agent_loop/hooks.rs` (new) — `BeforeToolHook`,
  `AfterToolHook` traits with closure adapters

**Tests (port from pi)**:
- `should handle tool calls and results` (239)
- `should execute mutated beforeToolCall args without revalidation` (310)
- `should prepare tool arguments for validation` (372) — `prepareArguments`
  shim runs BEFORE schema validation; `beforeToolCall` mutates AFTER
- `should stop after a tool batch when every tool result sets
  terminate=true` (1067)
- `should allow afterToolCall to mark a tool batch as terminating` (1184)

**Risk**: medium. Hook contract has subtle ordering (prepareArguments →
validate → beforeToolCall → execute → afterToolCall).

---

### Phase 3 — Tool execution: parallel + per-tool sequential override ✅

**Goal**: port `executeToolCallsParallel`. Tools that declare
`executionMode == "sequential"` (e.g. `bash`) force the whole batch
sequential even with default parallel config.

**Files**:
- `src/agent/agent_loop/tools.rs` — add parallel dispatcher
- `src/agent/tools/bash.rs` — set `executionMode: Sequential`
- `src/agent/tools/edit.rs`, `write.rs`, `apply_patch.rs` — sequential
  (they touch the filesystem and could race)

**Tests (port from pi)**:
- `should emit tool_execution_end in completion order but persist
  tool results in source order` (452) — KEY parallel-correctness test
- `should force sequential execution when a tool has
  executionMode=sequential even with default parallel config` (653)
- `should force sequential execution when one of multiple tools has
  executionMode=sequential` (736)
- `should allow parallel execution when all tools have
  executionMode=parallel` (823)
- `should continue after parallel tool calls when not all tool results
  terminate` (1119)

**Risk**: high. Concurrent borrow management for tools that hold &mut
references. May need `Arc<Mutex<…>>` or per-tool state cloning.
Permission checker calls + ask channel need to be parallel-safe (probably
already are; verify).

---

### Phase 4 — The loop itself (`runLoop`) ✅

**Goal**: port `runLoop` and `runAgentLoop` / `runAgentLoopContinue`.
This is the keystone. After this phase the new-loop feature ships
behavior-equivalent runs through the new path.

**Files**:
- `src/agent/agent_loop/run.rs` (new) — `run_loop`, `run_agent_loop`,
  `run_agent_loop_continue`
- `src/agent/agent_loop/queue.rs` (new) — steering queue + follow-up
  queue with `QueueMode` (drain-all vs one-at-a-time)
- `src/agent/runner.rs` — feature-gated dispatch: under `new-loop`,
  delegate to `agent_loop::run_loop`; otherwise keep existing rig
  multi-turn path

**Tests (port from pi)** — the meat of the spec:
- `should use prepareNextTurn snapshot before continuing` (897) —
  model/thinking/context all swap correctly
- `should stop after the current turn when shouldStopAfterTurn
  returns true` (970)
- `should inject queued messages after all tool calls complete` (547) —
  steering ordering invariant
- `agentLoopContinue` cases (1233-1351)

**Risk**: HIGH. Keystone. The retry/recovery loop currently wraps
the whole stream — needs to wrap each single-turn call instead.
Interjection currently fires at rig's tool-result boundary —
needs to map onto pi's steering queue mechanism.

Mitigation: keep the existing path behind `--features !new-loop`
default until phase 4 passes the full ported spec. Flip default in
phase 4.5 (separate commit) after baking.

---

### Phase 4.5 — Integration: rig + dirge consumers → ported loop ✅

**SCOPE CORRECTION**: The original 4.5 was wildly underscoped as
"delete the old path." Flipping the default is actually a multi-
commit integration project — the new loop currently works only
with mock streams + mock tools. Sub-divided below.

Each sub-phase ships green builds + green tests + a concrete
integration validation. Order is roughly dependency-ordered;
later ones can interleave once their inputs land.

#### 4.5a — rig → StreamFn adapter ✅

**Goal**: build a `StreamFn` that wraps rig's single-turn API
(`agent.prompt(history)` or equivalent) so the loop can drive a
real LLM. This is the "physical layer" — without it the loop is
a beautiful algorithm with nothing to talk to.

**Files**:
- `src/agent/agent_loop/rig_stream.rs` (new) — `rig_stream_fn`
  factory taking a rig agent and producing a `StreamFn`

**Tests**: integration test that calls a stubbed rig provider
through the adapter. Doesn't need a real network — rig has its
own mock fixtures.

**Risk**: medium. Rig's stream-event vocabulary needs mapping
to our `StreamEvent` enum. Tool-call extraction is subtle.

#### 4.5b — rig::Tool → LoopTool adapter ✅

**Goal**: a generic wrapper that takes any `Box<dyn rig::Tool>` and
implements `LoopTool` on it. Every dirge tool (read / write / edit /
bash / grep / find_files / list_dir / apply_patch / semantic_* /
mcp_* / skill) gets a LoopTool surface for free.

**Files**:
- `src/agent/agent_loop/rig_tool.rs` (new) — `RigToolAdapter` struct
  + impl
- `src/agent/builder.rs` — alongside the existing `Vec<Box<dyn
  rig::ToolDyn>>` build, also produce `Vec<Arc<dyn LoopTool>>`
  via the adapter

**Tests**: wrap a real dirge tool (e.g. `read`), execute through
the LoopTool surface, verify identical output to the rig path.

**Risk**: medium. rig::Tool's args/output types are generic;
LoopTool uses `Value`. The adapter does the serde round-trip.

#### 4.5c — LoopEvent → AgentEvent translation ✅

**Goal**: bridge the loop's event vocabulary to dirge's existing
`AgentEvent`. The UI and ACP consume `AgentEvent`; until we
replace those, we translate at the boundary.

**Files**:
- `src/agent/agent_loop/bridge.rs` (new) — `translate_event(LoopEvent)
  -> Vec<AgentEvent>` (Vec because some pi events split into multiple
  dirge events — `MessageUpdate` with `phase=TextDelta` → `Token`)

**Tests**: round-trip every LoopEvent variant; assert the
AgentEvent stream matches what the old runner emits for an
equivalent algorithmic input.

**Risk**: low. Pure data translation. Easy to verify.

#### 4.5d — Plugin slots → before/after tool hooks ✅

**Goal**: dirge's existing plugin hooks (`harness/block`,
`harness/mutate-input`, `harness/replace-result`) become
`BeforeToolCallFn` / `AfterToolCallFn` closures consumed by the loop.

**Files**:
- `src/agent/agent_loop/plugin_hooks.rs` (new) — factory functions
  `before_hook_from_plugin_manager(&PluginManager) -> BeforeToolCallFn`
  etc.

**Tests**: install a Janet plugin that mutates args; verify the
tool sees mutated args end-to-end through the loop.

**Risk**: medium. Plugin hooks fire from async closures; the
PluginManager's locking pattern needs verification.

#### 4.5e — interjection_queue → getSteeringMessages ✅

**Goal**: dirge's existing `interjection_queue` (in `ui/mod.rs`)
becomes the source for `GetSteeringMessagesFn`.

**Files**:
- `src/agent/agent_loop/steering.rs` (new) — factory that takes a
  shared `Arc<Mutex<VecDeque<String>>>` and produces a
  `GetSteeringMessagesFn`

**Tests**: enqueue messages, run a loop, verify they're injected at
the next turn boundary.

**Risk**: low.

#### 4.5f — runner.rs replacement (BEHIND a flag) ✅

**Goal**: add a `--use-agent-loop` CLI flag (or config option) that
routes a run through the new loop instead of the rig multi-turn
stream. Both paths coexist; default is still the old path.

**Files**:
- `src/agent/runner.rs` — branch on the flag; dispatch to either
  `spawn_runner` (existing) or a new `spawn_runner_via_loop` that
  composes 4.5a + 4.5b + 4.5c + 4.5d + 4.5e

**Tests**: integration test that runs a multi-turn session through
the new path; assert identical observable behavior (event stream
matches) for a canned scenario.

**Risk**: high. First time the new loop touches real dirge state.
Expected edge cases: agent_line_started state, chamber rendering,
permission check signal threading.

#### 4.5g — Recovery / retry under the new path ✅

**Goal**: wrap each `stream_assistant_response` call with the existing
`recovery::classify_error` + Retry-After backoff. Verify auto-compact
on `ContextOverflow` works.

**Files**:
- `src/agent/agent_loop/retry.rs` (new) — `retrying_stream_fn` wrapper
  that intercepts errors and retries with the recovery policy

**Tests**: simulate a network error mid-turn; verify retry happens.

**Risk**: medium.

#### 4.5h — Flip default; delete old path ✅

**SCOPE CORRECTION (second pass)**: 4.5h-1 (ContextOverflow
classification) shipped. The rest of "flip default" is genuinely
a multi-commit project — sub-divided below. Each sub-phase ships
green builds + green tests + concrete progress toward the cutover.

The decomposition is dependency-rooted but each piece is
independent enough to be reordered if a blocker appears. Phases
h-3 / h-4 / h-5 are mutually independent; h-6 composes them; h-7
is the manual-verification gate; h-8 is the final cutover.

##### 4.5h-1 — Bridge ContextOverflow classification ✓ (shipped)

When `LoopEvent::AgentEnd` carries an assistant with
`stop_reason=Error` + a context-length signal, the bridge emits
`AgentEvent::ContextOverflow { prompt, error }` instead of
`Error`. UI flow (auto-compact + respawn) works through the new
path the same way it does today.

##### 4.5h-2 — `AnyAgent` → `StreamFn` extraction helper

**Goal**: produce a `StreamFn` from any `AnyAgent` variant
without forcing every callsite to enumerate them.

**Approach**: rig's `Agent.model: Arc<M>` is public. Add a
method `AnyAgent::build_stream_fn(&self) -> StreamFn` that
matches on `AnyAgentInner` and threads each variant through
`rig_stream_fn_from_model::<M>(model.clone(), tools)`. Tools
arg comes from 4.5h-4.

**Files**:
- `src/provider.rs` — `AnyAgent::build_stream_fn` method
- `src/agent/agent_loop/rig_stream_factory.rs` — possibly a
  per-provider tweak if any provider's `StreamingResponse`
  doesn't auto-satisfy the `Send + Sync + 'static + GetTokenUsage`
  bound

**Tests**: per-variant compile-time bounds check (assert each
variant produces a `Send + Sync + 'static` `StreamFn`). No
runtime stream test — that needs real API keys (deferred to
h-7).

**Risk**: low–medium. Some providers' streaming response types
may need `+ 'static` bounds added — fixable if so.

##### 4.5h-3 — Chunk timeout enforcement

**Goal**: wrap rig's stream poll with `tokio::time::timeout` so
a stalled provider stream surfaces as an Error event after the
configured timeout (default 300s, per-provider override). Match
the existing runner's behavior.

**Approach**: insert a per-event timeout layer in either
`rig_stream.rs::wrap_streamed_assistant` (per-poll) or
`rig_stream_factory.rs::invoke_one_stream` (per-`next().await`).
Tests use `tokio::time::pause` to simulate a stalled inner
stream.

**Files**:
- `src/agent/agent_loop/rig_stream.rs` OR `rig_stream_factory.rs`
  (decide on cleanest insertion point) — add `chunk_timeout:
  Duration` parameter; emit Error event on timeout
- The `rig_stream_fn_from_model` builder takes
  `chunk_timeout: Duration` and threads it through

**Tests**: stalled inner stream emits Error after timeout;
fast inner stream is unaffected; cancellation interacts
correctly.

**Risk**: low. Pure stream-wrapping; no protocol changes.

##### 4.5h-4 — Parallel `LoopTool` registry builder

**Goal**: alongside the existing `Vec<Box<dyn ToolDyn>>` build
that dirge does today, produce `Vec<Arc<dyn LoopTool>>` via
`RigToolAdapter::new()` for each tool. Mutating tools (bash,
edit, write, apply_patch) get `with_execution_mode(Sequential)`.

**Approach**: in `agent/builder.rs` (or wherever tools are
assembled today), add a parallel `build_loop_tools(...)` that
takes the same constructor args (permission, ask_tx, cache,
lsp_manager, etc.) and returns the LoopTool registry.

**Files**:
- `src/agent/builder.rs` — `build_loop_tools` function

**Tests**: assert each known tool in the result is wrapped;
sequential set has the right execution mode; integration test
that runs a wrapped `ReadTool` through the LoopTool surface
(already done by 4.5b's `adapter_matches_rig_path_for_real_dirge_tool`).

**Risk**: medium. The dirge tool list isn't tiny; per-tool
wrap order matters for sequential-mode tagging. Mechanical
work + a checklist.

##### 4.5h-5 — Background notifications + steering plumbing

**Goal**: the existing runner calls
`tools::background::prepend_pending_notifications(prompt)`
before every run; the new path needs the same. The UI's
`interjection_queue` (today drained between runs) needs to
flow into `LoopSpawnConfig.steering_queue` so mid-run
interjection works.

**Files**:
- The spawn site (4.5h-6's new function) — call
  `prepend_pending_notifications` on the prompt
- UI side: share the `Arc<Mutex<VecDeque<String>>>` between
  the UI input pump and `LoopSpawnConfig.steering_queue`

**Tests**: background notification flows into prompt;
steering injection visible in the loop (already proven by
4.5e + 4.5f).

**Risk**: low.

##### 4.5h-6 — Direct cutover: replace old path with new

**SCOPE NOTE**: Earlier draft introduced a `--use-agent-loop`
flag for migration safety. Reverted to match pi's "one path"
shape — pi has no flag, never had a legacy path. Dirge's
cutover is one commit replacing the rig multi-turn internals
of `provider::spawn_runner`, deleting `runner::run_stream`,
matching pi semantically.

**Goal**: replace the rig multi-turn dispatch inside
`provider::spawn_runner` with `spawn_loop_runner`. Delete
the now-dead `runner::run_stream` + recovery loop. Keep
`AgentRunner` shape so UI callsites don't change.

**Approach**:
- Add `AnyAgent::spawn_runner` internals that compose 4.5h-2
  (StreamFn) + 4.5h-3 (chunk timeout) + 4.5h-4 (LoopTool
  registry) + 4.5h-5 (steering + notifications)
- Adapt the resulting `LoopRunner` to the `AgentRunner` shape
  (signal → interject_tx semantics) via an
  `into_agent_runner()` helper or equivalent
- DELETE `runner::run_stream` (~600 LOC) and the recovery
  retry loop in `spawn_agent`
- Keep `runner::convert_history` and `runner::spawn_agent`'s
  public signature — internals fully rewritten
- Remove the `agent-loop` feature gate from `Cargo.toml`
- Remove `#![allow(dead_code)]` + `#![allow(unused_imports)]`
  module-level allows in `src/agent/agent_loop/mod.rs`

**Files**:
- `src/provider.rs` — internals only; signature preserved
- `src/agent/runner.rs` — strip multi-turn path
- `Cargo.toml` — remove `agent-loop = []`
- `src/agent/agent_loop/mod.rs` — drop module-level allows
- `src/agent/agent_loop/integration.rs` —
  `LoopRunner::into_agent_runner()` adapter

**Tests**: existing tests for the new path are the parity
proof. An additional integration test that exercises the
full `spawn_runner → AgentEvent stream` path through the new
internals catches any glue bugs. The 724 default tests must
still pass (existing UI/ACP/tool tests; they were never
coupled to the old path's internals).

**Risk**: HIGH. First contact with real dirge state. Expected
friction:
- signal vs interject_tx semantics (graceful-stop at tool
  result vs immediate cancel)
- chamber rendering state assumptions
- ACP-specific event massaging
- background notification timing
- session.add_message timing

Fix forward — no fallback path. h-7 (manual provider testing)
catches what runs through to actual LLMs.

##### 4.5h-7 — Real-provider smoke testing (manual)

**Goal**: validate the new path against actual Anthropic /
OpenAI / OpenRouter accounts.

**Test scenarios**:
1. Simple Q (no tools)
2. Single-tool use (read a file)
3. Multi-turn tool sequence (read → edit → bash test)
4. Mid-run interjection ("wait, also do X")
5. Rate limit recovery (force one or watch existing logs)
6. Context overflow → auto-compact (large transcript)
7. Plugin hook flow (with at least one local Janet plugin)

**Files**: none structural. Bug-fix commits land as discovered.

**Approach**: user runs `dirge` against configured providers
(the new path is the only path after h-6 — no flag to set);
we iterate on bugs. May reveal missing features that earlier
phases overlooked (e.g. specific provider quirks, ACP edge
cases).

**Risk**: high — unknown unknowns are most likely to surface
here. Multi-session, requires API keys. Fix-forward
discipline: each bug found gets a focused commit.

---

**Estimated sizes** (rough LOC + new tests per sub-phase):

| Sub-phase | LOC      | Tests | Cumulative state                            |
|-----------|----------|-------|---------------------------------------------|
| 4.5h-2    | 80       | 1-2   | StreamFn buildable from any provider        |
| 4.5h-3    | 100      | 3-4   | + Chunk timeout enforced                    |
| 4.5h-4    | 120      | 2-3   | + Full tool registry available as LoopTools |
| 4.5h-5    | 60       | 2     | + Background notifications + steering       |
| 4.5h-6    | -600 +250| 3-5   | New path replaces old; pi-shape achieved    |
| 4.5h-7    | bug fixes| n/a   | Real-provider verified                      |

Each sub-phase ships green CI + a concrete user-visible step
forward. Phases h-2 through h-5 are mutually independent and
reorderable. h-6 is the cutover; h-7 is the manual
verification gate.

---

### Phase 5 — Plugin hook wiring ✅

**Goal**: surface every pi hook to Janet plugins via the existing
slot mechanism. Auto-applied at the right loop points.

**Slots (port from pi semantics)**:
- `harness-next-model` → `prepareNextTurn.model` (already exists; just
  re-wired)
- `harness-next-thinking-level` → `prepareNextTurn.thinkingLevel`
- `harness-next-context-system-prompt` / `harness-next-context-messages`
  → `prepareNextTurn.context`
- `harness-stop-after-turn` → `shouldStopAfterTurn` (drained per turn)
- `harness-steering-messages` → `getSteeringMessages` (drained per
  turn)
- `harness-followup-messages` → `getFollowUpMessages` (drained at
  outer-loop boundary)

**Janet helpers**:
- `harness/set-next-thinking-level` `(low|medium|high|xhigh|off|minimal)`
- `harness/request-stop-after-turn`
- `harness/add-steering` `(content)`
- `harness/add-followup` `(content)`

**Tests**: each slot has a Janet integration test (set slot from
on-tool-end hook → verify behavior on next turn).

**Risk**: low. Slot mechanism is well-trodden in dirge.

---

### Phase 6 — Recovery + interjection + abort under new loop ✅

**Goal**: make sure every existing dirge feature works under the new
loop architecture. This is the long-tail hardening phase.

**Specific paths**:
- `recovery::classify_error` wrapping each `stream_assistant_response`
  call (not the whole run)
- `Retry-After` header parsing still works
- Network error → backoff → resume preserves history
- Auto-compact on `ContextOverflow` → retry through the new loop
- Ctrl+C interrupts the in-flight LLM stream cleanly
- Ctrl+C while a tool runs aborts via the AbortSignal-equivalent
- `Esc Esc` rewind
- `/quit` mid-run
- Tool permission deny → tool result with denial message → next turn
  proceeds normally

**Tests**: regression suite that replays canned event sequences and
asserts identical observable behavior to the pre-port baseline.

**Risk**: medium. Edge cases.

---

### Phase 7 — Custom message types (`CustomAgentMessages`) ✅

**Goal**: pi allows app-defined non-LLM message variants
(notifications, artifacts) that `convertToLlm` filters before sending
to the model. Port the abstraction so dirge plugins can inject UI-only
messages without polluting the LLM context.

**Files**:
- `src/agent/agent_loop/messages.rs` — `LoopMessage` enum extension
  point; default `convert_to_llm` filters non-LLM variants
- Plugin slot: `harness-custom-message` to push UI-only messages
- UI consumer renders custom messages in chat without sending to model

**Tests**:
- `should handle custom message types via convertToLlm` (131)
- Verify custom messages reach the UI but not the LLM

**Risk**: low. Additive; no existing behavior changes.

---

### Phase 8 — Polish, parity verification, deprecations ✅

**Goal**: every test in pi's `agent-loop.test.ts` passes its dirge
counterpart. Deprecation cleanup.

**Tasks**:
- Audit pi's test file; verify each `it(…)` has a corresponding
  passing test in dirge
- Deprecate `prepare-next-run` (alias for `prepare-next-turn`); emit
  warning when used; remove in next minor
- Update README / docs to describe the new hook surface
- Add a `docs/agent-loop.md` walkthrough mirroring pi's algorithm

**Tests**: parity assertion test that diff'ing dirge's loop algorithm
against pi's `runLoop` finds no semantic gaps.

**Risk**: low. Documentation + parity verification.

---

## Verification gates per phase

Before each phase commits:
1. `cargo build --all-features` clean
2. `cargo test` green
3. `cargo fmt --check` clean
4. The phase's ported pi tests pass
5. Existing tests still pass
6. PLAN.md updated to mark the phase ✅

## Commit cadence

One phase per commit, except phases 4 and 4.5 which are paired (4
introduces, 4.5 flips default). Each commit:
- Title: `feat(agent): phase N — <one-line goal>`
- Body cites the pi test cases ported and any honest scope notes
- No commit ships untested behavior

## Estimated LOC

| Phase | LOC | Tests added |
|---|---|---|
| 0 | ~250 | 5 |
| 1 | ~350 | 3 |
| 2 | ~400 | 5 |
| 3 | ~350 | 5 |
| 4 | ~500 | 4 |
| 4.5 | -300 (deletion) | 0 (existing) |
| 5 | ~250 | 6 |
| 6 | ~150 | 8 |
| 7 | ~200 | 2 |
| 8 | ~100 | 1 |
| **Total** | **~2250** | **39** |

## Out-of-scope (not in this plan, may be follow-up plans)

- **Pi's `AgentHarness`** (`packages/agent/src/harness/agent-harness.ts`,
  995 LOC) — the higher-level "agent harness" wrapping `agentLoop`
  with compaction, retry, session management. Dirge already has its
  own equivalents (recovery, compact, session). Could be re-evaluated
  after the core loop port lands.
- **Pi's compaction policy** (`harness/compaction/compaction.ts`) —
  dirge has its own `/compress` + auto-compact. Could be ported
  separately if a specific divergence emerges.
- **Pi's skills system** — dirge has its own skill discovery; not
  the same shape.
- **Pi's `StreamFn` injection** — dirge uses the rig provider
  abstraction. The port uses rig's single-turn API throughout.

These four items are dirge's existing equivalents and don't need
porting for the loop change. They may diverge in subtle behavior
from pi, but those divergences are isolated from the `runLoop` port.

## Risk summary

| Phase | Risk | Why |
|---|---|---|
| 0 | None | Pure scaffolding |
| 1 | Med | Bridges rig single-turn API → pi event vocab |
| 2 | Med | Hook ordering subtlety |
| 3 | **High** | Concurrent tool dispatch + borrow management |
| 4 | **High** | The keystone; retry/recovery wrap point changes |
| 4.5 | Med | Deleting the old path; nothing left to fall back to |
| 5 | Low | Slot mechanism well-trodden |
| 6 | Med | Edge cases under integration |
| 7 | Low | Additive |
| 8 | Low | Documentation |

## Order of operations

Strict linear: 0 → 1 → 2 → 3 → 4 → 4.5 → 5 → 6 → 7 → 8.

Phases 1 / 2 / 3 are independent in spirit but share the type
surface from phase 0. Phases 1 / 2 / 3 must all land before phase 4
because the loop calls into all three.

Phase 6 can interleave with 5 if a particular hook integration
surfaces an unrelated edge case, but the default order is 5 → 6.
