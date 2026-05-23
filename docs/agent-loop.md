# dirge agent_loop — pi-style multi-turn architecture

Dirge's `src/agent/agent_loop/` is a faithful Rust port of pi's
`packages/agent/src/agent-loop.ts` (`runAgentLoop` /
`runAgentLoopContinue` / `runLoop` / `streamAssistantResponse` /
`executeToolCalls*`). This doc walks through the loop's
algorithm, the hook surface, and where each piece lives — read
alongside `pi/packages/agent/src/agent-loop.ts` to verify the
port is faithful.

## Module map

| File | Role | Pi source |
|---|---|---|
| `run.rs` | `run_loop` / `run_agent_loop` / `run_agent_loop_continue` — the keystone | `agent-loop.ts:95-269` |
| `stream.rs` | `stream_assistant_response` + `StreamFn` type | `agent-loop.ts:275-368` |
| `tools.rs` | `execute_tool_calls` umbrella dispatcher + sequential / parallel paths + `prepare_tool_call` + `execute_prepared_tool_call` + `finalize_executed_tool_call` | `agent-loop.ts:370-737` |
| `types.rs` | `Context`, `LoopConfig`, `TurnUpdate`, `ThinkingLevel`, `ThinkingBudgets`, `ToolExecutionMode`, `QueueMode` | `types.ts` |
| `hooks.rs` | `BeforeToolCallFn`, `AfterToolCallFn`, `PrepareNextTurnFn`, `ShouldStopAfterTurnFn`, `GetSteeringMessagesFn`, `GetFollowupMessagesFn`, `TurnHookContext` | `types.ts:84-260` |
| `message.rs` | `LoopMessage`, `AssistantMessage`, `ToolResultMessage`, `UserMessage`, `ContentBlock`, `StreamEvent`, `LoopEvent` | `types.ts:218-470` |
| `result.rs` | `LoopToolResult`, `BeforeToolCallResult`, `AfterToolCallResult` | `types.ts:280-355` |
| `tool.rs` | `LoopTool` trait, `AbortSignal`, `LoopToolUpdate` | `types.ts:361-403` |
| `bridge.rs` | `LoopEvent` → dirge's existing `AgentEvent` (UI / ACP compat) | dirge-specific |
| `integration.rs` | `spawn_loop_runner` composition + `LoopRunner` | dirge-specific |
| `rig_stream.rs` | `wrap_rig_stream` — adapter from rig's `StreamingCompletionResponse` to pi's `StreamEvent` | dirge-specific |
| `rig_stream_factory.rs` | `rig_stream_fn_from_model_with_provider` + per-provider reasoning mapping | dirge-specific |
| `rig_tool.rs` | `RigToolAdapter` — wraps `rig::ToolDyn` as `LoopTool` | dirge-specific |
| `retry.rs` | `retrying_stream_fn` — recovery wrapper around StreamFn | dirge-specific |
| `steering.rs` | `steering_from_queue` — shared queue → `GetSteeringMessagesFn` | dirge-specific |
| `plugin_hooks.rs` | factories that adapt Janet plugin hooks to pi's hook surface | dirge-specific |

## The algorithm (pi `runLoop` → `run.rs::run_loop`)

```
run_loop(current_context, new_messages, config, signal, emit, stream_fn):
  first_turn = true
  pending_messages = config.get_steering_messages?.() || []

  OUTER:
    has_more_tool_calls = true
    INNER while has_more_tool_calls OR pending_messages not empty:
      if !first_turn: emit turn_start
      else first_turn = false

      # Inject pending steering / follow-up messages
      for msg in pending_messages:
        emit message_start; emit message_end
        context.messages.push(msg); new_messages.push(msg)
      pending_messages = []

      # Stream the next assistant turn
      msg = stream_assistant_response(context, config, signal, emit, stream_fn)
      new_messages.push(msg)

      # Terminal stop reasons
      if msg.stop_reason ∈ [Error, Aborted]:
        emit turn_end (tool_results: []); emit agent_end; return

      # Dispatch tools
      tool_calls = filter msg.content for type=ToolCall
      tool_results = []; has_more_tool_calls = false
      if tool_calls non-empty:
        batch = execute_tool_calls(context, msg, config, signal, emit)
        tool_results = batch.messages
        has_more_tool_calls = !batch.terminate
        for result in tool_results:
          context.messages.push(result); new_messages.push(result)

      emit turn_end (msg, tool_results)

      # Between-turn hooks
      snapshot = config.prepare_next_turn?.(hook_ctx)
      if snapshot:
        if snapshot.context: context = snapshot.context
        # model + thinking_level: warn-only today (rig API limit)

      if config.should_stop_after_turn?.(hook_ctx):
        emit agent_end; return

      pending_messages = config.get_steering_messages?.() || []
    # INNER end

    # Outer-loop follow-up poll
    follow_up = config.get_followup_messages?.() || []
    if follow_up non-empty:
      pending_messages = follow_up
      continue OUTER

    break OUTER
  emit agent_end
```

This matches pi `agent-loop.ts:155-269` step-for-step. Inline
comments in `run.rs` cite the exact pi line per block.

## Hook surface

Pi exposes 6 config hooks; dirge ports all of them as
`LoopConfig` fields:

| Pi hook | Dirge field | Plugin slot |
|---|---|---|
| `convertToLlm` | `convert_to_llm: ConvertToLlmFn` | (Rust closure; not Janet) |
| `transformContext` | `transform_context: Option<TransformContextFn>` | (Rust closure) |
| `getApiKey(provider)` | `get_api_key: Option<GetApiKeyFn>` | (Rust closure) |
| `beforeToolCall` | `before_tool_call: Option<BeforeToolCallFn>` | `on-tool-start` + `harness/block` / `harness/mutate-input` |
| `afterToolCall` | `after_tool_call: Option<AfterToolCallFn>` | `on-tool-end` + `harness/replace-result` |
| `prepareNextTurn` | `prepare_next_turn: Option<PrepareNextTurnFn>` | `on-tool-end` + `harness/set-next-thinking-level` |
| `shouldStopAfterTurn` | `should_stop_after_turn: Option<ShouldStopAfterTurnFn>` | any hook + `harness/request-stop-after-turn` |
| `getSteeringMessages` | `get_steering_messages: Option<GetSteeringMessagesFn>` | any hook + `harness/add-steering` + `harness/add-custom-message` |
| `getFollowUpMessages` | `get_followup_messages: Option<GetFollowupMessagesFn>` | any hook + `harness/add-followup` |

The plugin slots are dirge-specific bridges:
`plugin_hooks::*_from_plugin_manager` factories produce
`*Fn` closures that read the corresponding Janet slot via the
shared `PluginManager` mutex.

## Provider stream options (`StreamOptions`)

Pi's `SimpleStreamOptions` shape is mirrored as `StreamOptions`
(in `stream.rs`):

```rust
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub headers: HashMap<String, String>,
    pub metadata: HashMap<String, Value>,
    pub request_timeout: Option<Duration>,
    pub signal: AbortSignal,
}
```

Per-call options threaded from `LoopConfig` to each `StreamFn`
invocation. The rig stream factory packs reasoning + headers +
metadata into rig's `additional_params` with **per-provider
shapes**:

- Anthropic: `thinking: { type: "enabled", budget_tokens }`
- OpenAI / DeepSeek / GLM / Custom / OpenRouter:
  `reasoning: { effort: "low"|"medium"|"high" }`
- Gemini: `thinking_config: { thinking_budget }`
- Ollama / unknown: generic `reasoning_level` fallback

See `rig_stream_factory.rs::build_provider_additional_params`
for the dispatch.

## Tool dispatch (sequential vs parallel)

Pi semantics (`executeToolCalls` at `agent-loop.ts:370-388`):
the batch runs sequentially if EITHER:
1. `config.toolExecution === "sequential"`, OR
2. Any tool in the batch declares `executionMode: "sequential"`

Dirge mirrors via `tools.rs::execute_tool_calls`:

```rust
let has_sequential = tool_calls.iter().any(|tc| {
    context.tools.iter().find(|t| t.name() == tc.name)
        .and_then(|t| t.execution_mode())
        == Some(ToolExecutionMode::Sequential)
});
if config.tool_execution == Sequential || has_sequential {
    execute_tool_calls_sequential(...)
} else {
    execute_tool_calls_parallel(...)
}
```

`build_loop_tools` (in `agent/builder.rs`) tags mutating tools
as Sequential — `write`, `edit`, `bash`, `apply_patch`, etc.
Read-only tools (`read`, `grep`, `list_dir`, `find_files`)
leave the default at Parallel.

The parallel path emits `tool_execution_end` events in
COMPLETION order but `message_end` events for tool results in
SOURCE order — pi's contract (test `agent-loop.test.ts:452`).
Tested in `tools.rs::test_tool_execution_end_completion_order_results_source_order`.

## Cancellation

`AbortSignal` is shared end-to-end:

1. **Loop level** — `run_loop` checks signal at every turn
   boundary via the assistant-message stop_reason path. A
   stream that emits `Error("aborted...")` exits the loop.
2. **Stream level** — `wrap_streamed_assistant` polls signal
   between chunks. A cancel during the rig HTTP stream emits
   an Error event mid-stream (phase 6 R3 fix).
3. **Tool level** — `execute_prepared_tool_call` wraps the
   tool future in `tokio::select!` against a signal-poll
   loop (`wait_for_cancel`). A cancel during a long-running
   tool returns an "aborted" error result within ~50ms.
   The tool's future is dropped; background processes may
   leak (rig::Tool surface has no cancellation hook).

## Recovery / retry

`retry.rs::retrying_stream_fn` wraps an inner `StreamFn` to
auto-retry transient errors (Network, RateLimit). Classification
via `recovery::classify_error`; backoff via `RecoveryPolicy`
combining exponential schedule + Retry-After parsing from the
error message.

Retry only fires BEFORE any committed content (text or tool-call
deltas). After commit, retrying would duplicate tokens — Error
passes through and the loop exits.

## Message → Value → rig::Message round-trip

The placeholder `Context.messages: Vec<Value>` carries our
LoopMessage variants serialized as JSON:

```rust
LoopMessage::User → {"role": "user", "content": "..."}
LoopMessage::Assistant → {"role": "assistant", "content": [blocks], ...}
LoopMessage::ToolResult → {"role": "toolResult", "toolCallId": ..., ...}
LoopMessage::Custom(v) → v.clone()  // arbitrary; role!=LLM-bound
```

`default_convert_to_llm` keeps only `role ∈ {user, assistant,
toolResult, system}`. Custom variants and other application-
defined roles get filtered before the StreamFn sees them.

The rig stream factory then converts each filtered Value to a
`rig::completion::Message` via `value_to_rig_message`, splits
the list into `(last, history)`, and builds a
`CompletionRequest`.

## Pi test parity (phase 8 audit)

All 19 `it(…)` blocks in `pi/packages/agent/test/agent-loop.test.ts`
have ported dirge counterparts:

| Pi line | Pi name | Dirge test |
|---|---|---|
| 84 | emit events with AgentMessage types | `run::test_emits_full_agent_loop_event_sequence` |
| 131 | custom message types via convertToLlm | `stream::test_convert_to_llm_filters_custom_messages` + `integration::default_convert_to_llm_filters_custom_messages` |
| 186 | transformContext before convertToLlm | `stream::test_transform_context_runs_before_convert_to_llm` |
| 239 | tool calls and results | `run::test_full_loop_with_tool_then_final_text` |
| 310 | mutated beforeToolCall args without revalidation | `tools::test_before_tool_call_mutates_args` |
| 372 | prepare tool arguments for validation | `tools::test_prepare_arguments_shim` |
| 452 | tool_execution_end completion order; results source order | `tools::test_tool_execution_end_completion_order_results_source_order` |
| 547 | inject queued messages after tool calls | `run::test_steering_messages_injected_after_tool_calls` + `steering::integration_steering_queue_injects_between_turns` |
| 653 | sequential override when single tool is sequential | `tools::test_per_tool_sequential_forces_sequential_route` |
| 736 | sequential override when one of many is sequential | `tools::test_one_sequential_among_many_forces_sequential` |
| 823 | allow parallel when all tools are parallel | `tools::test_all_parallel_runs_concurrent` |
| 897 | prepareNextTurn snapshot before continuing | `run::test_prepare_next_turn_snapshot_applied` |
| 970 | stop after current turn when shouldStopAfterTurn returns true | `run::test_should_stop_after_turn_stops_loop` |
| 1067 | stop after tool batch when every result has terminate | `run::test_terminate_stops_loop_after_tool_batch` |
| 1119 | continue when not all tool results terminate | `run::test_continue_when_not_all_terminate` + `tools::test_parallel_batch_not_terminating_when_mixed` |
| 1184 | afterToolCall marks batch as terminating | `run::test_after_tool_call_terminate_stops_loop` |
| 1234 | throw when context has no messages | `run::test_continue_errors_on_empty_context` |
| 1249 | continue without emitting user message events | `run::test_continue_does_not_reemit_user_message_events` |
| 1291 | allow custom message types as last message | `run::test_continue_accepts_custom_last_message` |

Run with: `cargo test agent_loop::`.

## Honest deviations from pi (documented)

These are language-driven differences or known gaps with no
production caller demanding the gap closure:

1. **`config.model` / `thinking_level` mid-run swap surfaces a
   tracing warning, doesn't apply.** The `StreamFn` closure
   captures the rig CompletionModel at construction. Rebuilding
   it mid-run would require a `Fn(Context) -> StreamFn` factory.
   The hook contract accepts the update; the warning makes it
   visible. See `run.rs::run_loop` lines 304-325.

2. **`opts.api_key` per-call rotation ignored.** Rig clients
   carry the key at construction; per-request override isn't a
   rig API today. `StreamOptions.api_key` is present for pi
   parity but the rig adapter doesn't honor it. The legacy
   provider env-var fallback covers most use cases.

3. **`opts.request_timeout` per-call ignored.** Same reason —
   rig configures HTTP timeout at client construction. Field
   present; not honored. Dirge's per-chunk timeout
   (`chunk_timeout` on `AnyAgent`) covers the typical case.

4. **`opts.signal` honored at the wrap_streamed_assistant
   boundary** (phase 6 R3). The rig HTTP request itself can't
   be cancelled mid-flight by signal — when signal fires, we
   stop polling the stream but the in-flight HTTP request
   continues until the server closes it. Negligible for
   typical streaming providers (they flush incrementally).

5. **`AgentEvent::ToolCall` emitted at `ToolExecutionStart`
   (after the LLM finishes streaming the tool_call block),
   not during streaming.** Bridge timing choice — tool dispatch
   is downstream of the stream, so emitting `ToolCall` from
   `ToolExecutionStart` keeps the order canonical. Observable
   change vs the legacy dirge runner; not a regression vs pi.

6. **`AgentEvent::Custom` not yet defined.** Bridge drops
   `LoopEvent::MessageStart { Custom }` — UI doesn't render
   custom messages today. Phase 7 added the LoopMessage::Custom
   variant + Janet plugin push helper + LLM-filter contract;
   the UI consumer is a separate commit.

7. **`harness-next-model`** is read at end-of-run by the UI
   (`ui/mod.rs:2359`) for the `/model X` swap path.
   `prepare_next_turn_from_plugin_manager` deliberately does
   NOT drain this slot (would steal it from the UI consumer).
   Mid-run model swap requires the same rig API growth as
   item 1.

## Production wiring

Every `provider::AnyAgent::spawn_runner` call routes through
the new path. Composition:

```
spawn_runner(prompt, history) -> AgentRunner
  cache.clear();
  tool_defs = self.loop_tools.iter().map(loop_tool_to_rig_definition)
  inner_stream_fn = self.build_stream_fn(tool_defs)   // 4.5h-2 + 4.6 reasoning
  stream_fn = retrying_stream_fn(inner_stream_fn, RecoveryPolicy::default())
  loop_history = rig_history_to_loop_messages(history)
  system_prompt = self.preamble + rig_history_system_prompt(&history)
  cfg = LoopSpawnConfig {
      stream_fn, system_prompt, history: loop_history, prompt,
      tools: self.loop_tools,
      plugin_mgr: plugin::hook::global(),    // 4.5d + phase 5
      provider_name: Some(self.provider_name()),  // for getApiKey
      ...
  };
  spawn_loop_runner(cfg).into_agent_runner()  // signal ↔ interject_tx
```

The legacy `runner::run_stream` + `runner::spawn_agent` (~600
LOC) are deleted as of phase 4.5h-6 cutover. Only `convert_history`
and `run_print` (non-streaming headless path) remain in
`runner.rs`.

## Real-provider testing

H-7 smoke tests (`agent_loop/h7_smoke.rs`, `#[ignore]`-gated)
run against actual provider APIs. Verified on:

- **DeepSeek** (`deepseek-chat`) — scenarios 1, 2, 3, 5 (simple Q,
  turn boundaries, tool dispatch, auth error)
- **GLM** (`glm-5.1` via Zhipu) — scenarios 1, 3 (simple Q,
  tool dispatch with reasoning)

Run: `cargo test agent_loop::h7_smoke -- --ignored --nocapture`

The runbook at `docs/H7_AGENT_LOOP_TEST.md` covers the manual
scenarios (mid-run interjection, context overflow, plugin
hooks) that require interactive UI or special setup.
