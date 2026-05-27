---
description: When implementing a feature that ports from Hermes Python to Rust, follow this methodology
triggers:
  - "port from Hermes"
  - "PLAN_LEARNING.md"
  - "implement round"
  - "Hermes reference"
---

# Hermes-to-Rust Porting Methodology

Systematic approach for porting Hermes Python features to dirge Rust.

## Phase 0: PLAN_LEARNING.md Cross-Check (periodic, before starting new feature)

PLAN_LEARNING.md goes stale fast — features get implemented but the plan isn't updated. Before working on any feature listed in the plan, cross-check:

1. Read the PLAN_LEARNING.md section for the feature
2. Read the relevant dirge source file(s) to see what's actually implemented
3. Read the Hermes reference file(s) to see the canonical behavior
4. Mark gaps as: ✅ Fixed (already done), 🔴 Open (still needs work), 🟡 New (not in plan, found during cross-check)
5. For 🔴 and 🟡 gaps, document: Hermes line numbers, what Hermes does, what dirge does, why it matters

**Parallel Hermes reads:** Use background subagents (`task` with `background=true`) for reading large Hermes files. Cap is 4 in-flight subagents — check `task_status` before spawning more, and don't spawn replacement tasks while others are running. Failed subagents (e.g. filesystem access issues) are unhelpful — prefer inline reads for small files.

**Gap severity levels:**
- CRITICAL: non-functional (e.g. per-turn writes missing → session search useless)
- HIGH: wrong behavior or missing major feature
- MEDIUM: incomplete but functional
- LOW: cosmetic, future optimization

**Update PLAN_LEARNING.md after cross-check** — mark fixed gaps and add newly discovered ones so the next audit starts from accuracy.

## Phase 2: Architecture (30 min)

1. Decide file layout — new files vs extending existing ones
2. Define Rust structs matching Hermes class shapes exactly
3. Plan migrations if schema changes needed (user_version gating, backfill)
4. Decide feature gates: `#[cfg_attr(not(feature = "X"), allow(dead_code))]` for feature-dependent code

## Phase 3: Implementation (per round)

1. **Stub**: Create files, define structs, stub methods with `todo!()`
2. **Core logic**: Port Hermes line by line — every guard clause, every error message
3. **Integration**: Wire into existing call sites
4. **Tests**: Write integration tests that exercise the full pipeline end-to-end
5. **Verify**: `cargo test --bin dirge <filter>`, `cargo check --bin dirge`

## Critical rules

- NEVER "simplify" — if Hermes has a guard clause, it caught a real bug
- Match error messages exactly to aid debugging
- FTS5 formula changes need DELETE + INSERT SELECT backfill (NOT `'rebuild'`)
- Schema migrations: sequential version checks, `IF NOT EXISTS` for triggers, handle "duplicate column name"
- Every round: `cargo test --bin dirge` must stay green
