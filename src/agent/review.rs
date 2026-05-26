//! Background review at session end.
//!
//! Port of Hermes's `agent/background_review.py`. After every session,
//! a forked agent with limited tools (memory + skill only) reviews the
//! transcript and writes project learnings to MEMORY.md, PITFALLS.md,
//! and skills.
//!
//! The review runs as a fire-and-forget tokio task — it never blocks
//! the main session. If it fails, the error is logged and the session
//! continues unaffected.
//!
//! Key design decisions from Hermes preserved:
//! - Fork, don't inline (separate agent instance, no prompt-cache pollution)
//! - Tool whitelist (only memory + skill tools)
//! - Same credentials as parent session
//! - Frozen conversation snapshot
//! - Fire-and-forget (daemon thread pattern)

use crate::extras::dirge_paths::ProjectPaths;
use crate::provider::AnyAgent;

/// Review prompt focused on project memory and pitfalls.
/// Port of Hermes's `_MEMORY_REVIEW_PROMPT` adapted for coding context.
const MEMORY_REVIEW_PROMPT: &str = r#"Review the conversation above and update project memory.

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools — they are not loaded and will fail.

**MEMORY.md** (project facts, conventions, architecture):
- What build/test commands were discovered or confirmed?
- What naming conventions, file layout patterns, or import styles were used?
- What architecture patterns emerged (how modules relate, error handling style)?
- What library quirks or tool behaviors were discovered?
- Were there any user corrections about how things should be done?

**PITFALLS.md** (anti-patterns and things to avoid):
- Was something tried and failed? Capture what was attempted and WHY it failed.
- Were there environment-specific issues that need documentation?
- Were there test fixtures or mocks that behaved unexpectedly?
- Were there any "gotchas" discovered with the build system or tooling?

For each finding, use the `memory` tool to add an entry. Be specific and actionable — a future session should benefit from what you learned.

"Nothing to save." is valid but should not be the default. Most coding sessions produce at least one learning."#;

/// Review prompt focused on procedural skills.
/// Port of Hermes's `_SKILL_REVIEW_PROMPT` adapted for coding context.
const SKILL_REVIEW_PROMPT: &str = r#"Review the conversation and improve project skills.

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools.

**SKILLS**: procedural improvements.
- Did a skill that was loaded turn out wrong, outdated, or missing steps? PATCH IT NOW using the `skill` tool.
- Did a non-trivial technique, workaround, or debugging workflow emerge from the session?
- Did the user correct your style, approach, or workflow? Embed the lesson.
- Were there test patterns or debugging strategies used successfully?

Preference order for skills:
1. UPDATE a currently-loaded skill (the one in play)
2. UPDATE an existing umbrella skill
3. CREATE a new class-level skill

Start by listing existing skills, then decide what to update or create.

"Nothing to update." is valid but should not be the default."#;

/// Combined review prompt — reviews both memory and skills in one pass.
const COMBINED_REVIEW_PROMPT: &str = r#"Review the conversation above and do TWO things:

**CRITICAL: You have ONLY the `memory` and `skill` tools available.** Do not attempt to use read, write, edit, bash, or any other tools.

**1. Update MEMORY:**
- What project facts, conventions, or build commands were confirmed?
- What pitfalls or anti-patterns were discovered?
- Any user corrections about how things should be done?

**2. Update SKILLS:**
- Did any loaded skills turn out wrong or outdated? PATCH them.
- Did a non-trivial workflow or debugging strategy emerge? CREATE a skill.
- Did the user correct your approach? Embed that lesson.

Use the `memory` tool to add entries to MEMORY.md (facts) or PITFALLS.md (pitfalls).
Use the `skill` tool to list, view, patch, or create skills.

Be specific and actionable. Future sessions should benefit from what you learned.
"Nothing to save." is valid but should not be the default."#;

/// Spawn a background review task that evaluates the just-completed
/// session and writes learnings to project memory and skills.
///
/// This is fire-and-forget — it runs in a `tokio::spawn` task and
/// returns immediately. Failures are logged to stderr and never
/// block the user.
pub fn spawn_background_review(agent: AnyAgent, _paths: ProjectPaths, transcript: String) {
    tokio::spawn(async move {
        // Build a review runner with only memory + skill tools.
        let review_runner =
            agent.spawn_review_runner(COMBINED_REVIEW_PROMPT.to_string(), transcript);

        // Drain events. We don't render them — the review runs
        // silently in the background.
        let mut rx = review_runner.event_rx;
        let mut had_error = false;
        while let Some(event) = rx.recv().await {
            use crate::event::AgentEvent;
            match event {
                AgentEvent::Error(msg) => {
                    tracing::warn!(
                        target: "dirge::review",
                        error = %msg,
                        "Background review encountered an error"
                    );
                    had_error = true;
                }
                AgentEvent::Done { .. } => {
                    break;
                }
                _ => {
                    // Tokens, tool calls, etc. — consumed silently.
                }
            }
        }

        if !had_error {
            tracing::info!(
                target: "dirge::review",
                "Background review completed — project knowledge updated"
            );
        }
    });
}
