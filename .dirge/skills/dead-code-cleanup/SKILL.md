# dead-code-cleanup

Systematic dead code removal following dirge conventions.

## Dead code cleanup workflow

### Process (do in this order)

1. `cargo check --bin dirge 2>&1 | grep "never used\|never read\|warning: "` to enumerate
2. Group by category:
   - **Bug**: `#[cfg_attr(not(feature), allow)]` that's stale — code IS used, annotation is wrong
   - **Forgotten wiring**: feature is implemented+tested but never called from integration point
   - **Future infrastructure**: implemented+tested, awaiting pipeline component (compression LLM, slash commands)
   - **Genuine dead code**: no tests, no plan reference, no Hermes source — delete
3. For each warning, check `PLAN_LEARNING.md` for feature spec
4. Check `~/src/hermes/` for reference implementation to confirm wiring intent
5. Wire up if feature needed and tested, keep `#[allow]` + rationale comment if future infra, delete if junk

### Rules (in priority order)

1. **DELETE legacy code** — don't annotate. If the old way is fully replaced, remove it.
2. **`#[cfg(test)]`** for test-only exports, constants, helper functions
3. **`#[cfg_attr(not(feature = "X"), allow(dead_code))]`** for feature-gated items (`plugin`, `acp`)
4. **`#[allow(dead_code)]` + doc comment** for API surface pending integration (port contract)
5. **NEVER `#![allow(dead_code)]` at module level** — hides real dead code

### Exceptions

- `agent_loop/mod.rs` uses `#![allow(unused_imports)]` on its re-export block
- `compression.rs` has ~10 `#[allow]` annotations — all Round 9 infrastructure awaiting auxiliary LLM pipeline (LoopConfig.compact_model). Verified against Hermes context_compressor.py. Do NOT remove.
- `session_db.rs`: `last_init_error()`, `set_parent_session()` await slash-command consumers
- `skills/manager.rs`: `archive()`/`restore()` await skill tool wiring
- `skills/usage.rs`: `set_pinned()` (web API only in Hermes — awaits /pin slash), `skill_names()` (tool schemas, autocomplete)
- `session_db.rs`: `SearchResult.role` — populated from SQL, not yet read by consumers

### Pitfalls

- **`end_session()` in `persist_turn_to_db()` causes session content leakage**: marking session "done" after every turn makes previous session content dump alongside user input. Keep `#[cfg(test)]` — only two Hermes callers: compression rotation and explicit user exit.
- **Don't add `end_session()` to turn-persistence paths** — it's not idempotent in effect despite being idempotent in SQL.
- **UsageStore.clone() for mutation from `&self` context**: since `SkillTool.call()` takes `&self`, usage counter bumps require `self.usage.clone()` to get an owned mutable copy. Intentional — best-effort telemetry, the clone is cheap.
- **Double `session.add_message(MessageRole::User)` causes chat display duplication**: user messages were added to session twice — once at input time (ui/mod.rs ~line 2054) and once in the `AgentEvent::UserMessage` handler. The event handler should render only; session mutation belongs at input time.
- **`cfg_attr(not(feature = "plugin"), allow(dead_code))` on fields that are ALWAYS dead**: fields like `BeforeToolCallContext.assistant_message` that current plugin hooks don't read should use plain `#[allow(dead_code)]` + explanatory doc comment. Reserve `cfg_attr` for fields that ARE read when the feature is active.

### Verification

```bash
cargo check --bin dirge  # must produce ZERO warnings
cargo test --bin dirge   # all 1306 tests must pass
```

### What was removed in the canonical cleanup

- `MEMORY_REVIEW_PROMPT`, `SKILL_REVIEW_PROMPT` (only `COMBINED_REVIEW_PROMPT` used)
- `ZAgent` type alias (unused)
- `create_client` OpenRouter builder (replaced by `provider::create_client`)
- `ChatMessage` type alias (unused)
- `CHARS_PER_TOKEN_ESTIMATE` constant (unused)
- `steering_from_queue_with_sanitizer` + test (redundant — base fn already sanitizes)
- `LoopError` enum + `run_agent_loop_continue` + 4 tests (legacy pi continue path)
- Module-level `#![allow(dead_code)]` from: `agent_loop/mod.rs`, `lsp/mod.rs`, `ui/box_render.rs`