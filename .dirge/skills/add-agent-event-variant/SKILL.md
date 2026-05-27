# add-event-variant

Adding a new variant to enums that flow through the agent pipeline in dirge. Three enums form a pipeline: `StreamEvent` → `LoopEvent` → `AgentEvent`. When adding a variant to any of them, ALL exhaustive match arms must be updated. The compiler finds most — use `cargo check --bin dirge` to locate remaining ones.

---

## Adding a `StreamEvent` variant

`src/agent/agent_loop/message.rs` — add the variant to the enum

Files to update:
1. `src/agent/agent_loop/retry.rs` — inner stream match (yield vs break logic) + all test label matches
2. `src/agent/agent_loop/stream.rs` — main dispatch loop inside `stream_assistant_response`
3. `src/agent/agent_loop/rig_stream.rs` — `label()` fn + test label matches
4. `src/agent/agent_loop/rig_stream_factory.rs` — test label matches

## Adding a `LoopEvent` variant

`src/agent/agent_loop/message.rs` — add the variant to the enum

Files to update:
1. `src/agent/agent_loop/message.rs` — `kind()` discriminant method
2. `src/agent/agent_loop/bridge.rs` — `translate()` method + `agent_event_kind` test helper
3. `src/agent/agent_loop/stream.rs` — any emit.send() call sites for the new event

## Adding an `AgentEvent` variant

`src/event.rs` — add the variant

Files to update:
1. `src/event.rs` — add the variant
2. `src/agent/agent_loop/bridge.rs` — `translate()` method + `agent_event_kind` test helper (~line 1043)
3. `src/agent/agent_loop/h7_smoke.rs` — `print_event()` function
4. `src/agent/agent_loop/integration.rs` — `agent_event_kind()` helper
5. `src/extras/acp/mod.rs` — ACP event loop match
6. `src/ui/mod.rs` — main UI event handler (large match, add arm before terminal events like `UserMessage`)
7. `src/provider/mod.rs` — `run_print` path (wildcard `_ => {}`, won't break but check)
8. `src/agent/review.rs` — (wildcard `_ => {}`, won't break)

### Verification

```bash
cargo test --bin dirge  # 1356 tests must pass
cargo check --bin dirge  # zero warnings
```

### Recent examples

- `AgentEvent::RetryNotice` — retry layer emits before re-attempting stream; bridge translates from `StreamEvent::Retry` (retry.rs) → `LoopEvent::RetryNotice` (stream.rs) → `AgentEvent::RetryNotice` (bridge.rs); UI renders dim "⟳ retry N (Xms)…" banner
- `StreamEvent::Retry` — non-terminal; retry.rs yields it after classification+backoff sleep; stream.rs translates to LoopEvent::RetryNotice; inner stream match in retry.rs treats it as unexpected (only outer loop produces it)
- `LoopEvent::RetryNotice` — carries attempt, delay_ms, error; bridge emits AgentEvent::RetryNotice
- `AgentEvent::UserMessage` — steering-injected user messages rendered as chat entries
- `AgentEvent::ContextCompacted` — compression fired, UI persists session rotation to DB