//! `LoopTool` trait — port of pi's `AgentTool<TParameters, TDetails>`
//! (types.ts:361).
//!
//! Phase 0: trait definition. No implementations yet. Phase 2 wires
//! existing rig tools through this trait so the new loop can
//! dispatch them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;

use super::result::LoopToolResult;
use super::types::ToolExecutionMode;

/// Cooperative cancellation signal passed to tool `execute` calls.
///
/// Rust equivalent of pi's `AbortSignal` (browser/Node API at
/// types.ts:373). Tools poll `is_cancelled()` between long
/// steps and bail out cleanly. The loop sets it from one place
/// (Ctrl+C / `/quit` / Esc-Esc) and every tool currently running
/// observes the same flag.
///
/// LOOP-4: separate `interjected` flag from `cancelled`. The
/// `cancelled` flag is for hard aborts (Ctrl+C, kill signal) —
/// tools see it and return synthetic errors. The `interjected`
/// flag is for graceful interjection (user hits Esc) — it stops
/// the loop at the next turn boundary but lets in-flight tools
/// complete normally. Tools never check `is_interjected()`.
///
/// Implemented as `Arc<AtomicBool>` rather than `tokio_util`'s
/// `CancellationToken` so we don't pull in a new dep for the
/// trivial case. If we ever need `.cancelled().await` (notifier
/// semantics for futures that want to race against the signal),
/// upgrade to `tokio_util::sync::CancellationToken` in a later
/// phase.
#[derive(Debug, Clone, Default)]
pub struct AbortSignal {
    cancelled: Arc<AtomicBool>,
    interjected: Arc<AtomicBool>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            interjected: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Trigger hard cancellation. Idempotent — subsequent calls
    /// are no-ops. Tools poll `is_cancelled()` and bail out
    /// cleanly when true.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    /// Read the cancelled state. Tools call this from inside
    /// their `execute` loops.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    /// LOOP-4: Trigger graceful interjection. Idempotent. The
    /// loop checks this at turn boundaries and stops accepting
    /// new turns, but in-flight tools complete normally. Tools
    /// never check this flag — they only check `is_cancelled()`.
    pub fn interject(&self) {
        self.interjected.store(true, Ordering::SeqCst);
    }
    /// LOOP-4: Read the interjected state. The loop checks this
    /// at turn boundaries. Tools never call this.
    pub fn is_interjected(&self) -> bool {
        self.interjected.load(Ordering::SeqCst)
    }
}

/// Callback used by tools to stream partial execution updates.
///
/// Port of pi `AgentToolUpdateCallback<T>` (types.ts:358):
///   `(partialResult: AgentToolResult<T>) => void`
///
/// Pi's callback is synchronous; our Rust version is a boxed
/// `Fn` so async-context callers can capture senders without
/// extra ceremony. Tools call this between long-running steps
/// to surface progress (e.g. "scanned 1000/5000 files"); the
/// loop translates each invocation into a
/// `tool_execution_update` event downstream.
pub type LoopToolUpdate = Arc<dyn Fn(&LoopToolResult) + Send + Sync>;

/// A tool the agent loop can dispatch.
///
/// Port of pi `AgentTool<TParameters, TDetails>` extending
/// `Tool<TParameters>` (types.ts:361). Pi's generic parameters
/// (`TParameters` for the JSON Schema, `TDetails` for the typed
/// result payload) collapse to JSON `Value` here — Rust trait
/// objects can't carry generic type parameters per call, and the
/// phase-2 dispatcher needs a homogeneous tool registry. Tools
/// that want typed args/results convert in their `execute` impl.
///
/// Pi field mapping:
///   - `name: string`              → `name(&self) -> &str`
///   - `description: string`       → `description(&self) -> &str`
///   - `label: string`             → `label(&self) -> &str`
///   - `parameters: TSchema`       → `parameters(&self) -> &Value`
///   - `prepareArguments?`         → `prepare_arguments(&self, args)`
///   - `execute(id, params, ...)`  → `execute(&self, id, args, signal, on_update)`
///   - `executionMode?`            → `execution_mode(&self) -> Option<ToolExecutionMode>`
pub trait LoopTool: Send + Sync + std::fmt::Debug {
    /// Tool name as the LLM sees it. Pi field `name: string`.
    fn name(&self) -> &str;

    /// Human-readable description shown to the LLM in the tool
    /// list. Pi field `description: string`.
    fn description(&self) -> &str;

    /// UI-display label distinct from the LLM-facing name. Pi
    /// field `label: string` (types.ts:363).
    #[allow(dead_code)]
    fn label(&self) -> &str;

    /// JSON Schema of the tool's arguments. Pi field
    /// `parameters: TSchema` — typebox at the pi end, plain
    /// `serde_json::Value` here so the same trait object can
    /// front tools with wildly different arg shapes.
    fn parameters(&self) -> &Value;

    /// Flattened variant of `parameters` for deep/wide schemas.
    /// When `Some`, the LLM sees the flat schema (dot-notation
    /// keys) and the dispatch re-nests args before calling
    /// `execute`. Port of Reasonix `InternalTool.flatSchema`
    /// (tools.ts:37).
    ///
    /// Default: `None` (no flattening).
    fn flat_parameters(&self) -> Option<&Value> {
        None
    }

    /// Per-tool execution-mode override. `None` means "use the
    /// loop's default mode". Returning `Sequential` forces the
    /// whole batch sequential per pi's tool-execution semantics
    /// (agent-loop.ts:381 — `hasSequentialToolCall`).
    ///
    /// Pi field `executionMode?: ToolExecutionMode` (types.ts:383).
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Compatibility shim run BEFORE schema validation. Pi field
    /// `prepareArguments?(args: unknown): Static<TParameters>`
    /// (types.ts:368). Mutates raw provider arguments into a
    /// shape that matches the declared `parameters` schema.
    ///
    /// Returning the input unchanged is the no-op default.
    fn prepare_arguments(&self, args: Value) -> Value {
        args
    }

    /// Execute the tool call. Pi field
    /// `execute(toolCallId, params, signal?, onUpdate?)`
    /// (types.ts:370). Throws-on-failure semantics map to
    /// `Result::Err`; the dispatcher catches `Err` and emits an
    /// error tool result the same way pi does.
    ///
    /// Returns a `Pin<Box<dyn Future>>` rather than `async fn` so
    /// the trait is dyn-compatible without the `async_trait`
    /// macro. Matches rig's `ToolDyn` shape (which dirge already
    /// uses elsewhere).
    ///
    /// `signal`: cooperative cancellation flag — tools poll it.
    /// `on_update`: streaming-progress callback; tools that don't
    /// emit progress just never call it.
    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        args: Value,
        signal: AbortSignal,
        on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<LoopToolResult, String>> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AbortSignal::is_cancelled()` is false on construction; flips
    /// true after `cancel()`; clones share state.
    #[test]
    fn abort_signal_shared_state() {
        let sig = AbortSignal::new();
        assert!(!sig.is_cancelled());
        let clone = sig.clone();
        sig.cancel();
        assert!(clone.is_cancelled(), "clone must see the cancel");
        // Double-cancel is a no-op.
        clone.cancel();
        assert!(sig.is_cancelled());
    }

    /// `AbortSignal::default()` matches `::new()` — uncancelled.
    #[test]
    fn abort_signal_default_uncancelled() {
        let sig = AbortSignal::default();
        assert!(!sig.is_cancelled());
    }
}
