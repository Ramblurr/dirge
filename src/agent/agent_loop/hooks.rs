//! `beforeToolCall` and `afterToolCall` config hooks.
//!
//! Faithful port of pi's hook surface at agent-loop.ts:578-708.
//!
//! Pi's hooks are JavaScript callbacks that receive a context
//! object and may MUTATE the args in place (test pi:310). Rust
//! can't compose `&mut` cleanly with `Pin<Box<dyn Future>>`, so
//! we pass `args` by value and return the (possibly mutated)
//! args alongside the hook result. The dispatcher threads the
//! returned args forward — semantically identical to pi's
//! mutate-in-place but with explicit data flow.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::message::{AssistantMessage, LoopMessage, ToolResultMessage};
use super::result::{AfterToolCallResult, BeforeToolCallResult, LoopToolResult};
use super::types::{Context, TurnUpdate};

/// Context passed to `beforeToolCall`. Port of pi
/// `BeforeToolCallContext` (types.ts:84).
///
/// Fields are owned values (clones) so the hook closure can be
/// `Fn(Ctx) -> Future` rather than `Fn(&Ctx) -> Future` — the
/// latter is hairy with `Pin<Box<dyn Future>>` lifetimes. Pi's
/// hooks receive references to mutable JS objects; we trade a
/// small clone overhead for a clean async-fn shape.
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call_id: String,
    pub tool_call_name: String,
    /// Validated args. The hook may mutate these (via the
    /// returned `BeforeToolCallReturn.args`) — pi tests this at
    /// agent-loop.test.ts:310.
    pub args: Value,
}

/// Return value of `beforeToolCall`. Pi returns
/// `Promise<BeforeToolCallResult | undefined>` AND mutates the
/// context's `args` in place. Since Rust can't elegantly mutate
/// through a moved value, the closure returns BOTH the result
/// (possibly None) and the (possibly mutated) args.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallReturn {
    /// Pi's return value: `block?` + `reason?`. `None` means
    /// "let the call proceed unchanged".
    pub result: Option<BeforeToolCallResult>,
    /// Possibly-mutated args. Even when `result` is None, these
    /// args are what the tool executes with. Hooks that don't
    /// mutate should return the input args unchanged.
    pub args: Value,
}

/// `beforeToolCall` hook signature. Pi (types.ts:262):
///   `(context: BeforeToolCallContext, signal?) => Promise<BeforeToolCallResult | undefined>`
pub type BeforeToolCallFn = Arc<
    dyn Fn(BeforeToolCallContext) -> Pin<Box<dyn Future<Output = BeforeToolCallReturn> + Send>>
        + Send
        + Sync,
>;

/// Context passed to `afterToolCall`. Port of pi
/// `AfterToolCallContext` (types.ts:96).
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call_id: String,
    pub tool_call_name: String,
    pub args: Value,
    pub result: LoopToolResult,
    pub is_error: bool,
}

/// `afterToolCall` hook signature. Pi (types.ts:276):
///   `(context: AfterToolCallContext, signal?) => Promise<AfterToolCallResult | undefined>`
///
/// Returning `None` keeps the executed result verbatim; returning
/// `Some(AfterToolCallResult { … })` overrides any of the four
/// fields per pi's merge semantics (content/details/isError/
/// terminate replace in full when Some).
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------
// Phase 4 hooks: prepareNextTurn, shouldStopAfterTurn,
// getSteeringMessages, getFollowupMessages.
// ---------------------------------------------------------------

/// Context passed to `prepareNextTurn` AND `shouldStopAfterTurn`.
/// Pi has `PrepareNextTurnContext extends ShouldStopAfterTurnContext`
/// (types.ts:133) — same shape, two aliases. We define one struct.
///
/// Port of pi `ShouldStopAfterTurnContext` (types.ts:112):
///   `{ message, toolResults, context, newMessages }`
#[derive(Debug, Clone)]
pub struct TurnHookContext {
    /// The assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool result messages from this turn (empty if no tools).
    pub tool_results: Vec<ToolResultMessage>,
    /// Current agent context AFTER the turn's assistant + tool
    /// results have been appended.
    pub context: Context,
    /// Messages this loop invocation will return if it exits at
    /// this point. Pi semantics: includes initial prompts for
    /// `agentLoop`; excludes pre-existing context messages for
    /// `agentLoopContinue` (types.ts:120).
    pub new_messages: Vec<LoopMessage>,
}

/// `prepareNextTurn` hook signature. Port of pi
/// `prepareNextTurn?` (types.ts:215):
///   `(context) => AgentLoopTurnUpdate | undefined | Promise<...>`
///
/// `None` means "no changes — continue with current state". The
/// returned `TurnUpdate`'s `Some` fields replace the
/// corresponding loop config / context for the next turn.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(TurnHookContext) -> Pin<Box<dyn Future<Output = Option<TurnUpdate>> + Send>>
        + Send
        + Sync,
>;

/// `shouldStopAfterTurn` hook signature. Port of pi
/// `shouldStopAfterTurn?` (types.ts:208):
///   `(context) => boolean | Promise<boolean>`
///
/// Returning `true` requests a graceful stop after the current
/// turn — the loop emits `agent_end` and exits without polling
/// steering or follow-up queues.
pub type ShouldStopAfterTurnFn =
    Arc<dyn Fn(TurnHookContext) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// `getSteeringMessages` hook signature. Port of pi
/// `getSteeringMessages?` (types.ts:230):
///   `() => Promise<AgentMessage[]>`
///
/// Polled at: (a) loop entry, (b) after each `turn_end` /
/// `prepareNextTurn` / `shouldStopAfterTurn` cycle. Returned
/// messages inject BEFORE the next assistant response.
pub type GetSteeringMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<LoopMessage>> + Send>> + Send + Sync>;

/// `getFollowupMessages` hook signature. Port of pi
/// `getFollowUpMessages?` (types.ts:243):
///   `() => Promise<AgentMessage[]>`
///
/// Polled at the OUTER-loop boundary — when the inner loop has
/// no more tool calls AND no pending steering. Non-empty return
/// triggers the outer loop to re-enter the inner loop with these
/// messages as pending.
pub type GetFollowupMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<LoopMessage>> + Send>> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    /// `BeforeToolCallReturn::default()` is the no-op outcome —
    /// result=None, args=Null. Hooks that "did nothing" return
    /// effectively this shape (with the input args instead of
    /// Null).
    #[test]
    fn before_return_default() {
        let r = BeforeToolCallReturn::default();
        assert!(r.result.is_none());
        assert_eq!(r.args, Value::Null);
    }
}
