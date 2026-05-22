# Port pi's `prepareNextTurn` faithfully — phased TDD plan

## Reference

Pi's source of truth (read these before each phase):
- `~/src/pi/packages/agent/src/types.ts` — `AgentLoopConfig`, `AgentLoopTurnUpdate`, `ThinkingLevel`
- `~/src/pi/packages/agent/src/agent-loop.ts` — `runLoop`, the `prepareNextTurn` call site at line 220-239
- `~/src/pi/packages/agent/src/agent.ts` — `Agent` class, how state is threaded

## Architectural goal

Move dirge's turn dispatch OUT of rig's multi-turn stream and into our own
loop. Each iteration calls rig's single-turn API, runs tools, appends history,
fires hooks. Exactly the shape `runLoop` has in pi.

## Constraint

Every phase ships green tests + green build. No phase leaves the runner in a
half-converted state. Behavior is preserved across phases — phase 4 is the
first phase that USERS see new behavior.

---

## Phase 1 — ThinkingLevel enum + provider plumbing

**Goal**: dirge can carry a `ThinkingLevel` from config to the LLM request,
even though nothing varies it yet.

**TDD slice**:
1. Test that `ThinkingLevel::High` serializes/deserializes from config JSON.
2. Test that the Anthropic provider builder routes `ThinkingLevel::High` to
   the `thinking` request param.
3. Test that the OpenAI provider builder routes it to `reasoning_effort`.
4. Test that `Off` is a no-op (no request param emitted).

**Files**:
- `src/event.rs` — new `ThinkingLevel` enum, matching pi's six variants
  (`Off | Minimal | Low | Medium | High | Xhigh`)
- `src/config/mod.rs` — `thinking_level: Option<ThinkingLevel>` config field
- `src/provider/*.rs` (or wherever provider wiring lives) — per-provider
  request-param mapping
- `src/cli.rs` — optional `--thinking <level>` flag

**Out of scope**: changing how the runner uses it. The level is wired but
not yet mutable mid-run.

**Risk**: low. Additive config. Each provider's mapping is independent.

---

## Phase 2 — Steering message queue

**Goal**: dirge can buffer messages that will be injected at the next turn
boundary, without consuming them yet.

**TDD slice**:
1. Test that `SteeringQueue::push(msg)` then `drain()` returns the message.
2. Test that `drain()` empties the queue.
3. Test ordering: multiple `push`es return in FIFO order.
4. Test that a new plugin slot `harness-steering-messages` exposes the
   queue to Janet plugins.

**Files**:
- `src/agent/steering.rs` (new) — `SteeringQueue` struct
- `src/plugin/worker.rs` — Janet helper `harness/add-steering`
- `src/plugin/mod.rs` — `take_pending_steering_messages()`

**Out of scope**: actually injecting at turn boundaries. That requires the
new loop (phase 3).

**Risk**: low. Pure data structure + plugin wiring.

---

## Phase 3 — Self-driven multi-turn loop

**Goal**: replace `agent.stream_chat()` + `MultiTurnStream` consumption
with our own loop that drives rig's single-turn API. Output is byte-
equivalent at the `AgentEvent` boundary — consumers don't change.

**TDD slice** (each test runs against a mock rig agent that returns
canned responses):
1. Single-turn run (no tool calls) emits `TurnStart → tokens → TurnEnd
   → Done` in the same order as the old runner.
2. Multi-turn run (one assistant turn with a tool call, then a final
   text turn) emits the full sequence.
3. Tool dispatch happens through dirge's tool registry, not rig's
   built-in.
4. History is correctly accumulated across turns (turn N+1 sees turn N's
   assistant message and tool results).
5. The retry/recovery loop (`recovery::classify_error`, `Retry-After`,
   backoff) still wraps each single-turn call.
6. Interjection at a tool-result boundary still works.
7. ContextOverflow path still works (auto-compact + resume).

**Files**:
- `src/agent/loop.rs` (new) — `AgentLoop` struct, `run()` method matching
  pi's `runLoop` shape
- `src/agent/runner.rs` — strip the rig multi-turn consumption; delegate
  to `AgentLoop::run`. Keep the public `spawn_runner` API stable so
  consumers (`ui/mod.rs`, `extras/acp/mod.rs`) don't change.

**Out of scope**: the new hooks (phase 4-5). The loop fires `prepare-next-
turn` and `should-stop-after-turn` at the RIGHT POINTS but they don't do
anything observable yet.

**Risk**: HIGH. Tool dispatch is currently rig's job; we'd need to:
- Parse rig's `ToolCall` items from a single-turn stream
- Dispatch each through dirge's tool registry
- Build `ToolResult` messages
- Append to the message history

Mitigations:
- Mock-rig tests run before integration tests
- Keep the rig stream code in place behind a feature flag during transition
  (`--features new-loop`); flip default in a later commit after baking
- Integration regression suite: replay existing session JSON, assert
  identical event sequence

**Deliverable shape**: this phase is the BIGGEST. Probably 400-600 LOC
across runner.rs + new loop module + tests. May want to split into:
- 3a: extract single-turn dispatch helper (no behavior change)
- 3b: drive the loop ourselves; tool dispatch still via rig
- 3c: own the tool dispatch too

If 3c is too risky, dirge can ship with 3b and still get most of the
benefit (we control turn boundaries; rig still owns tool dispatch within
a single turn).

---

## Phase 4 — `prepare-next-turn` per-turn firing + auto-apply

**Goal**: faithful port of pi's `prepareNextTurn`. Plugin sees a context,
returns a snapshot (model / thinking-level / context), dirge applies all
three before the next turn within the same run.

**TDD slice**:
1. Hook fires once per turn (count assertion across a multi-turn run).
2. `harness-next-model` set to `"X"` → next turn's LLM call uses model X.
3. `harness-next-thinking-level` set to `"high"` → next turn's request
   carries `reasoning_effort: high`.
4. Setting `"off"` clears reasoning explicitly (matching pi line 235).
5. Steering messages enqueued during the hook are injected at the next
   turn (uses phase 2's queue).
6. No-op when the hook returns nothing (current model/thinking persists).

**Files**:
- `src/plugin/worker.rs` — rename `prepare-next-run` → `prepare-next-turn`;
  add `harness-next-thinking-level` slot + `harness/set-next-thinking-level`
  helper
- `src/plugin/mod.rs` — `take_pending_next_thinking_level()`
- `src/agent/loop.rs` — fire the hook at the right point in the loop;
  apply the model/thinking mutations to the next iteration

**Migration note**: `prepare-next-run` becomes a deprecated alias for
`prepare-next-turn` for one release, then removed. Document in the commit.

**Risk**: medium. Touches the new loop + provider request param plumbing.
Tests cover all six contract points.

---

## Phase 5 — `should-stop-after-turn`

**Goal**: faithful port of pi's `shouldStopAfterTurn`. Plugin can request
the loop to exit gracefully after the current turn completes.

**TDD slice**:
1. Plugin sets `harness-stop-after-turn` to `true` → next iteration's
   `should-stop` check returns true → loop emits Done and exits.
2. Current turn's tool dispatch completes normally even when stop is
   pending (matches pi: "current assistant response and any tool
   executions finish normally").
3. The slot is cleared after each read (avoid sticky-stop).

**Files**:
- `src/plugin/worker.rs` — new slot + helper
- `src/plugin/mod.rs` — accessor
- `src/agent/loop.rs` — check at the right point

**Risk**: low. Single boolean slot drained once per turn.

---

## Phase 6 — Integration + regression hardening

**Goal**: make sure all the existing abort/interjection/recovery paths
still work cleanly through the new loop.

**TDD slice**:
1. Ctrl+C mid-tool-execution aborts cleanly.
2. Esc-Esc (rewind) reaches the right turn boundary.
3. `/quit` mid-run exits without hanging.
4. ContextOverflow recovery (auto-compact + retry) works through the new
   loop with one round-trip.
5. Network error → retry-after backoff → resume works.
6. Tool permission deny → tool result with denial message → next turn
   proceeds.

**Files**:
- `src/agent/loop.rs` + `src/agent/runner.rs` — patch any rough edges
  the regression suite finds

**Risk**: medium. Edge cases only show up under integration.

---

## Out-of-scope (intentional)

- **`getFollowUpMessages`** — pi's outer-loop continuation. We can fold
  this into steering or skip; dirge's current "user types again"
  flow already covers the use case.
- **`beforeToolCall` / `afterToolCall`** — dirge already has
  `on-tool-start` / `on-tool-end` hooks. Different API shape; same
  capability.
- **`transformContext`** — pi exposes this as a hook; dirge has
  `/compress` and the auto-compact path. Could be added later if a
  plugin needs programmatic context shaping.
- **`toolExecution: "sequential" | "parallel"`** — dirge currently
  runs tools sequentially via rig. Parallel dispatch is a larger
  concurrent-borrow refactor; defer.
- **`shouldStopAfterTurn`** can be implemented in Phase 5 (above)
  but the broader "agent_end" / "follow-up messages" outer loop
  isn't faithfully replicated. Dirge's current model is "one user
  prompt = one run"; pi's is more elaborate.

---

## Commit cadence

One commit per phase. Each commit's message follows the existing pattern:
- Title: `feat(agent): phase N — <one-line goal>`
- Body: what changed, what tests cover it, what's deferred to the next
  phase, any honest scope notes.

Total estimated LOC: ~1500 across 6 commits, weighted heavily toward
Phase 3.

## Verification gate

Before each phase commits:
1. `cargo build --all-features` clean
2. `cargo test` green (current count + N new)
3. `cargo fmt --check` clean
4. The phase's TDD tests exist and pass
5. Existing tests still pass (no behavioral regression)

## Order of operations

Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6.

Phase 1 and 2 are independent and can swap order, but Phase 3 depends on
both being landed (it consumes ThinkingLevel and SteeringQueue). Phase 4
depends on Phase 3. Phase 5 depends on Phase 3. Phase 6 depends on
Phases 4 + 5.
