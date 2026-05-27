use compact_str::CompactString;

/// Structured classification of tool output for richer downstream
/// rendering. Most tools return plain text and use `Text`; tools
/// that surface file references (`read`, `find_files`,
/// `list_dir`) can opt into `File` so consumers (ACP, future UI
/// features) can render file refs as resource links rather than
/// blobs of text.
///
/// The classification is currently coarse — assigned by the
/// runner based on tool NAME rather than via per-tool plumbing —
/// since that's enough to drive opencode/ACP-style file-link
/// surfaces without touching every tool's `type Output = String`
/// contract. A future refactor could thread the variant through
/// the rig `Tool` trait for finer control.
#[derive(Debug, Clone, Default)]
pub enum ToolContent {
    /// Plain text output — the default for every tool that
    /// returns prose, JSON, command output, diffs, etc.
    #[default]
    Text,
    /// Tool surfaced one or more file paths (read returned the
    /// content of a specific file; find_files returned a listing).
    /// Consumers can render as a clickable resource link instead
    /// of a text blob.
    File,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Token(CompactString),
    Reasoning(CompactString),
    ToolCall {
        /// Provider call id (rig's `ToolCall.id`). Empty for older
        /// rig versions or providers that don't emit one; the UI
        /// uses it to pair this call with the corresponding
        /// `ToolResult` event for structured persistence (Phase 3).
        id: CompactString,
        name: CompactString,
        args: serde_json::Value,
    },
    /// Fired immediately AFTER `ToolCall` — marks the transition
    /// from "LLM has emitted this call" to "dispatch is imminent".
    /// Semantically: between this event and the matching
    /// `ToolResult`, the tool is *running*. Consumers use it to:
    ///   - Show per-tool spinners / status badges
    ///   - Emit ACP `ToolCallStatus::InProgress` updates (the
    ///     ACP protocol distinguishes pending / in_progress /
    ///     completed; without this dirge skipped the in_progress
    ///     transition)
    ///   - Plugin observability hooks that need a "started" tick
    ///     distinct from "LLM decided to call"
    ///
    /// The id matches the corresponding `ToolCall.id` so consumers
    /// can pair them. UI consumers that already track in-flight
    /// state via "saw ToolCall, no matching ToolResult" can ignore
    /// this event safely — it's purely additive.
    ///
    /// `name` is intentionally omitted — consumers correlate by
    /// `id` against the immediately-prior `ToolCall` which already
    /// carries the name. Keeping the variant lean (one field)
    /// keeps the per-event allocation cheap; the runner emits
    /// many of these per turn.
    ToolStarted {
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        id: CompactString,
    },
    ToolResult {
        /// Matching call id from the `ToolCall` event. Empty if the
        /// provider didn't emit one — the UI falls back to
        /// positional pairing (this result belongs to the most-
        /// recent unanswered ToolCall in the same turn).
        id: CompactString,
        output: CompactString,
        #[cfg_attr(not(feature = "acp"), allow(dead_code))]
        kind: ToolContent,
    },
    Error(CompactString),
    /// The streaming run failed with a context-length error. Audit
    /// H17: the UI used to render this as a hard `Error` and stop;
    /// users had to manually `/compress` then re-issue. Now the
    /// runner emits `ContextOverflow` carrying the prompt it was
    /// trying to send so the UI can auto-compact the session and
    /// respawn the run with the same prompt against the compacted
    /// history.
    ContextOverflow {
        prompt: CompactString,
        error: CompactString,
    },
    /// Context was compacted mid-run — old tool results pruned,
    /// session rotated. The UI persists the split via session DB.
    ContextCompacted {
        new_session_id: CompactString,
        tokens_before: u64,
        tokens_after: u64,
    },
    Done {
        response: CompactString,
        tokens: u64,
        cost: f64,
    },
    /// Marks the start of one turn within an agent run. A "turn" is one
    /// LLM call + any tool calls it dispatched + the tool results
    /// returning. A pure-text response has exactly one turn (TurnStart 0
    /// → TurnEnd 0 → Done). A run with tool calls has multiple turns,
    /// with turn boundaries straddling tool-result/next-assistant
    /// content. Plugin hook authors (P3) consume these to bracket
    /// per-turn observability.
    TurnStart {
        index: u32,
    },
    /// Marks the end of one turn. Fires immediately before the next
    /// turn's TurnStart, or just before `Done` for the final turn.
    /// Empty runs (stream ended without any assistant content) emit
    /// neither TurnStart nor TurnEnd.
    TurnEnd {
        index: u32,
    },
    /// Plugin-emitted custom message reaching the UI mid-stream.
    /// Carries the raw JSON payload the plugin queued via
    /// `harness/add-custom-message`. The UI looks up a registered
    /// renderer (see `PluginManager::list_message_renderers`) by
    /// the payload's `type` field; without one it falls back to a
    /// default formatter. Port of pi's `LoopMessage::Custom` →
    /// `registerMessageRenderer` lookup (extensions/types.ts:1171).
    CustomMessage {
        payload: serde_json::Value,
    },
    /// The runner observed an interjection request at a tool-result boundary
    /// and stopped the stream cleanly. Whatever assistant text had streamed
    /// so far is captured in `partial_response`. The UI commits it as an
    /// assistant message and then drains its interjection queue as the next
    /// user turn.
    ///
    /// Constructed by the bridge when the LLM stream is cancelled via the
    /// interject signal (rig_stream error message "stream aborted by
    /// cancellation signal" is recognized as a graceful stop, not a hard
    /// error).
    Interjected {
        partial_response: CompactString,
        tokens: u64,
    },
    /// User message injected mid-run via the steering queue.
    /// The UI renders this as a user chat message so the user sees
    /// their interjected guidance appear in the log when the agent
    /// processes it at a turn boundary.
    UserMessage {
        content: CompactString,
    },
    /// The retry layer is about to re-attempt a stream request after
    /// a transient error. `attempt` is 1-indexed (the Nth retry).
    /// PROV-2: consumers should surface a temporary banner so the
    /// user isn't staring at silence during backoff.
    RetryNotice {
        attempt: u32,
        delay_ms: u64,
        error: CompactString,
    },
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    Paste(String),
    /// Terminal was resized. Carries no payload — the renderer queries
    /// `crossterm::terminal::size()` directly; the variant is just a kick
    /// to repaint at the new dimensions.
    Resize,
    /// Mouse wheel scrolled up — scroll the output pane up by one line.
    /// Mouse capture is on (see `TerminalGuard::new`) so the wheel reaches
    /// the app instead of being absorbed by the terminal, which under the
    /// alt screen would push the TUI off-view.
    ScrollUp,
    /// Mouse wheel scrolled down — scroll the output pane down by one line.
    ScrollDown,
    /// Left mouse button pressed at terminal cell `(row, col)` — starts
    /// an app-level drag selection. Consumed by `ui::selection::handle`
    /// before any UI-state-specific consumer sees it.
    MouseDown {
        row: u16,
        col: u16,
    },
    /// Left mouse button dragged to terminal cell `(row, col)` — extends
    /// the active selection.
    MouseDrag {
        row: u16,
        col: u16,
    },
    /// Left mouse button released at terminal cell `(row, col)` —
    /// finalizes the selection, copies it to the clipboard, and clears
    /// the highlight.
    MouseUp {
        row: u16,
        col: u16,
    },
}
