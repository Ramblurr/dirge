use super::*;
use crate::agent::agent_loop::hooks::{
    AfterToolCallContext, AfterToolCallFn, GetSteeringMessagesFn, PrepareNextTurnFn,
    ShouldStopAfterTurnFn,
};
use crate::agent::agent_loop::message::{StreamEvent, UserMessage};
use crate::agent::agent_loop::result::AfterToolCallResult;
use crate::agent::agent_loop::stream::StreamFn;
use crate::agent::agent_loop::tool::{AbortSignal, LoopTool, LoopToolUpdate};
use crate::agent::agent_loop::types::{ConvertToLlmFn, LoopConfig, ToolExecutionMode, TurnUpdate};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Build a stream factory that returns canned assistant
/// messages in sequence. Mirrors pi's typical test mock —
/// `callIndex` increments per invocation; each call returns
/// the next canned response.
///
/// `responses` is a Vec; index N is returned on the (N+1)th
/// call. Past the end → final fallback message with
/// stopReason=Stop.
fn canned_factory(responses: Vec<AssistantMessage>) -> StreamFn {
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let responses = std::sync::Arc::new(responses);
    std::sync::Arc::new(move |_ctx, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = responses.get(n).cloned().unwrap_or_else(|| {
            AssistantMessage::new(
                vec![ContentBlock::Text {
                    text: "end".to_string(),
                }],
                StopReason::Stop,
            )
        });
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    })
}

fn identity_converter() -> ConvertToLlmFn {
    std::sync::Arc::new(|messages: &[Value]| {
        messages
            .iter()
            .filter(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                matches!(role, "user" | "assistant" | "tool" | "toolResult")
            })
            .cloned()
            .collect()
    })
}

fn build_config() -> LoopConfig {
    LoopConfig {
        convert_to_llm: identity_converter(),
        transform_context: None,
        get_api_key: None,
        api_key: None,
        tool_execution: ToolExecutionMode::Sequential,
        before_tool_call: None,
        after_tool_call: None,
        prepare_next_turn: None,
        should_stop_after_turn: None,
        get_steering_messages: None,
        get_followup_messages: None,
        reasoning: None,
        thinking_budgets: None,
        headers: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        request_timeout: None,
        provider_name: None,
        model_name: None,
        compact_model: None,
        storm_mutating_tools: None,
        storm_exempt_tools: None,
        repair_stats: std::sync::Arc::new(
            crate::agent::agent_loop::tool_input_repair::RepairStats::new(),
        ),
        tool_def_filter: None,
        dynamic_tool_search: false,
        escalation_stream_fn: None,
        escalation_provider_name: None,
        escalation_pending: std::sync::Arc::new(std::sync::Mutex::new(None)),
        escalation_max_per_session: 3,
        escalation_remaining: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(3)),
        file_touch_tracker: None,
        max_turns: None,
    }
}

fn empty_context() -> Context {
    Context {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    }
}

/// LOOP-9 integration: `run_compaction_pass` end-to-end. Feed
/// a long conversation, a mock summarizer, and assert that
/// (a) the older messages were dropped, (b) a SUMMARY_PREFIX
/// system message was inserted at the head, (c) the latest
/// user message is still in the tail, and (d) a
/// `ContextCompacted` event was emitted with a rotated session id.
#[tokio::test]
async fn run_compaction_pass_inserts_summary_and_rotates_session() {
    let mut ctx = empty_context();
    ctx.system_prompt = "you are an agent".into();
    // Pad with 25 turns so the compaction window has material.
    ctx.messages.push(serde_json::json!({
        "role": "system", "content": "you are an agent"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "initial task: fix the bug"
    }));
    for i in 0..20 {
        let role = if i % 2 == 0 { "assistant" } else { "user" };
        ctx.messages.push(serde_json::json!({
            "role": role,
            "content": format!("turn {i} with some content to fill bytes"),
        }));
    }
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "latest user request"
    }));
    let n_before = ctx.messages.len();

    // Mock summarizer: returns a valid Hermes-style summary
    // structure. We assert the prompt was built (non-empty).
    let prompt_seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let prompt_seen_inner = prompt_seen.clone();
    let summarize_fn: Option<crate::agent::compression::SummarizeFn> =
        Some(std::sync::Arc::new(move |prompt: String| {
            let store = prompt_seen_inner.clone();
            Box::pin(async move {
                *store.lock().unwrap() = prompt;
                Ok("## Active Task\nfix the bug\n\n\
                        ## Goal\nresolve the issue\n\n\
                        ## Completed Actions\n1. read the file\n\n\
                        ## Remaining Work\nrun tests"
                    .to_string())
            })
        }));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(8);
    super::run_compaction_pass(&mut ctx, &summarize_fn, 5, &None, &tx).await;
    drop(tx);

    // (a) older messages dropped.
    assert!(
        ctx.messages.len() < n_before,
        "expected compaction to shrink the message list: before={n_before} after={}",
        ctx.messages.len()
    );

    // (b) summary system message with SUMMARY_PREFIX is present.
    let summary_msg = ctx
        .messages
        .iter()
        .find(|m| {
            m.get("role").and_then(|v| v.as_str()) == Some("system")
                && m.get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.contains("CONTEXT COMPACTION"))
                    .unwrap_or(false)
        })
        .expect("compaction summary message should be present");
    let body = summary_msg["content"].as_str().unwrap();
    assert!(body.contains("## Active Task"));
    assert!(body.contains("fix the bug"));

    // (c) latest user message preserved.
    let last = ctx.messages.last().unwrap();
    assert_eq!(last["content"].as_str().unwrap(), "latest user request");

    // (d) ContextCompacted event emitted with rotated session id.
    let mut compacted_event_seen = false;
    while let Some(ev) = rx.recv().await {
        if let LoopEvent::ContextCompacted { new_session_id, .. } = ev {
            assert!(
                new_session_id.starts_with("compacted-"),
                "session id should rotate via compacted- prefix; got {new_session_id}"
            );
            compacted_event_seen = true;
        }
    }
    assert!(compacted_event_seen, "expected ContextCompacted event");

    // Sanity: the summarizer received a Hermes structured prompt
    // (built via build_summary_prompt).
    let received = prompt_seen.lock().unwrap().clone();
    assert!(received.contains("TURNS TO SUMMARIZE"));
    assert!(received.contains("## Active Task"));
}

/// LOOP-9: when no summarizer is wired, the compaction pass
/// still runs the cheap pruning and emits ContextCompacted, but
/// does NOT insert a structured summary system message.
#[tokio::test]
async fn run_compaction_pass_without_summarizer_prunes_only() {
    let mut ctx = empty_context();
    // One large tool result that should be pruned.
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "first"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "toolResult", "content": "x".repeat(2000), "toolName": "bash"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "user", "content": "tail"
    }));
    ctx.messages.push(serde_json::json!({
        "role": "assistant", "content": "tail asst"
    }));

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(4);
    // Use protect_tail = 2 so the large tool result is eligible
    // for pruning (it's at index 1, end = 4 - 2 = 2, so index
    // 1 is in-range).
    super::run_compaction_pass(&mut ctx, &None, 2, &None, &tx).await;
    drop(tx);

    // No SUMMARY_PREFIX message inserted.
    let has_summary = ctx.messages.iter().any(|m| {
        m.get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("CONTEXT COMPACTION"))
            .unwrap_or(false)
    });
    assert!(
        !has_summary,
        "no summary should be inserted without summarize_fn"
    );

    // The large tool result was pruned (replaced with a [bash] marker).
    let tool_msg = &ctx.messages[1];
    assert!(tool_msg["content"].as_str().unwrap().contains("[bash]"));

    // ContextCompacted still emitted.
    let mut compacted_event_seen = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, LoopEvent::ContextCompacted { .. }) {
            compacted_event_seen = true;
        }
    }
    assert!(compacted_event_seen);
}

/// Mock echo tool for run-loop tests. Records executed args
/// per call so test setups can detect terminate-flag flow.
#[derive(Debug)]
struct EchoTool {
    terminate: bool,
    executed: std::sync::Arc<Mutex<Vec<Value>>>,
}
impl EchoTool {
    fn new() -> Self {
        Self {
            terminate: false,
            executed: std::sync::Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn with_terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
}
impl LoopTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo tool"
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn parameters(&self) -> &Value {
        static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        _signal: AbortSignal,
        _on_update: LoopToolUpdate,
    ) -> Pin<Box<dyn Future<Output = Result<super::super::LoopToolResult, String>> + Send + 'a>>
    {
        let executed = self.executed.clone();
        let terminate = self.terminate;
        Box::pin(async move {
            executed.lock().unwrap().push(args.clone());
            Ok(super::super::LoopToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "ok"})],
                details: args,
                terminate: if terminate { Some(true) } else { None },
            })
        })
    }
}

fn user(text: &str) -> LoopMessage {
    LoopMessage::User(UserMessage {
        content: text.to_string(),
    })
}

fn text_response(text: &str) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        StopReason::Stop,
    )
}

fn tool_use_response(id: &str, name: &str, args: Value) -> AssistantMessage {
    AssistantMessage::new(
        vec![ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }],
        StopReason::ToolUse,
    )
}

/// Drain channel into a Vec.
async fn drain(rx: &mut mpsc::Receiver<LoopEvent>) -> Vec<LoopEvent> {
    let mut out = Vec::new();
    while let Some(e) = rx.recv().await {
        out.push(e);
    }
    out
}

/// Port of pi test "should emit events with AgentMessage types"
/// (agent-loop.test.ts:84). Full agent loop run — assistant
/// response, no tools.
#[tokio::test]
async fn test_emits_full_agent_loop_event_sequence() {
    let factory = canned_factory(vec![text_response("Hi there!")]);
    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("Hello")],
        empty_context(),
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    // Must contain all pi-required events.
    for required in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(kinds.contains(&required), "missing {required}: {kinds:?}");
    }
    // Return value: user + assistant message.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role(), "user");
    assert_eq!(messages[1].role(), "assistant");
}

/// Port of pi test "should handle tool calls and results"
/// (agent-loop.test.ts:239). Full-loop scope: assistant emits
/// tool call → loop dispatches → next assistant emits final
/// text.
#[tokio::test]
async fn test_full_loop_with_tool_then_final_text() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo.clone());

    let factory = canned_factory(vec![
        tool_use_response("call-1", "echo", serde_json::json!({"v": 1})),
        text_response("done"),
    ]);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Tool actually executed.
    assert_eq!(echo.executed.lock().unwrap().len(), 1);

    // Roles: user, assistant (tool use), toolResult, assistant (text).
    let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult", "assistant"]);

    // Stream of events should contain tool_execution_start +
    // tool_execution_end.
    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"tool_execution_start"));
    assert!(kinds.contains(&"tool_execution_end"));
}

/// Port of pi test "should use prepareNextTurn snapshot before
/// continuing" (agent-loop.test.ts:897). The hook returns a
/// snapshot mutating `context`; subsequent turn observes the
/// mutation.
#[tokio::test]
async fn test_prepare_next_turn_snapshot_applied() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.system_prompt = "first prompt".to_string();
    ctx.tools.push(echo.clone());

    // Track the system_prompt seen at each LLM call.
    let observed_prompts = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_clone = observed_prompts.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
        observed_clone.lock().unwrap().push(llm_ctx.system_prompt);
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    // Hook fires once: returns a new context with a different
    // system prompt.
    let fired = std::sync::Arc::new(AtomicUsize::new(0));
    let fired_clone = fired.clone();
    let hook: PrepareNextTurnFn = std::sync::Arc::new(move |ctx| {
        let fired = fired_clone.clone();
        Box::pin(async move {
            if fired.fetch_add(1, Ordering::SeqCst) > 0 {
                return None; // only on the first invocation
            }
            Some(TurnUpdate {
                context: Some(Context {
                    system_prompt: "second prompt".to_string(),
                    messages: ctx.context.messages.clone(),
                    tools: ctx.context.tools.clone(),
                }),
                ..Default::default()
            })
        })
    });

    let mut config = build_config();
    config.prepare_next_turn = Some(hook);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(128);
    let _ = run_agent_loop(
        vec![user("echo something")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    let observed = observed_prompts.lock().unwrap().clone();
    assert_eq!(observed.len(), 2, "expected 2 LLM calls");
    assert_eq!(observed[0], "first prompt");
    assert_eq!(
        observed[1], "second prompt",
        "second LLM call should see the mutated context"
    );
}

/// Port of pi test "should stop after the current turn when
/// shouldStopAfterTurn returns true" (agent-loop.test.ts:970).
#[tokio::test]
async fn test_should_stop_after_turn_stops_loop() {
    let factory = canned_factory(vec![
        text_response("turn one"),
        // Second response should NEVER be requested — hook
        // stops the loop after turn one.
        text_response("should not appear"),
    ]);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    // Wrap factory to count invocations.
    let factory_counted: StreamFn = std::sync::Arc::new(move |ctx, opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        factory(ctx, opts)
    });

    let hook: ShouldStopAfterTurnFn = std::sync::Arc::new(|_ctx| Box::pin(async move { true }));

    let mut config = build_config();
    config.should_stop_after_turn = Some(hook);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("hi")],
        empty_context(),
        config,
        AbortSignal::new(),
        &tx,
        &factory_counted,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    // Only one LLM call.
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    // Messages: user + one assistant.
    assert_eq!(messages.len(), 2);
    // Loop emitted agent_end.
    let kinds: Vec<_> = drain(&mut rx).await.iter().map(|e| e.kind()).collect();
    assert!(kinds.contains(&"agent_end"));
}

/// Port of pi test "should stop after a tool batch when every
/// tool result sets terminate=true" (agent-loop.test.ts:1067).
/// LOOP-LEVEL: only one LLM call (the tool dispatch terminates).
#[tokio::test]
async fn test_terminate_stops_loop_after_tool_batch() {
    let echo = std::sync::Arc::new(EchoTool::new().with_terminate());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: msg,
            usage: None,
        }]))
    });

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let messages = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
    // user + assistant(tool use) + toolResult — no second
    // assistant text turn.
    let roles: Vec<_> = messages.iter().map(|m| m.role()).collect();
    assert_eq!(roles, vec!["user", "assistant", "toolResult"]);
}

/// Port of pi test "should allow afterToolCall to mark a tool
/// batch as terminating" (agent-loop.test.ts:1184). LOOP-LEVEL.
#[tokio::test]
async fn test_after_tool_call_terminate_stops_loop() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = tool_use_response("call-1", "echo", serde_json::json!({"v": 1}));
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: msg,
            usage: None,
        }]))
    });

    let after: AfterToolCallFn = std::sync::Arc::new(|_ctx: AfterToolCallContext| {
        Box::pin(async move {
            Some(AfterToolCallResult {
                content: None,
                details: None,
                is_error: None,
                terminate: Some(true),
            })
        })
    });
    let mut config = build_config();
    config.after_tool_call = Some(after);

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("echo")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(llm_calls.load(Ordering::SeqCst), 1, "no second LLM call");
}

/// Port of pi test "should continue after parallel tool calls
/// when not all tool results terminate" (agent-loop.test.ts:1119).
/// LOOP-LEVEL: two LLM calls.
#[tokio::test]
async fn test_continue_when_not_all_terminate() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    let llm_calls = std::sync::Arc::new(AtomicUsize::new(0));
    let llm_calls_clone = llm_calls.clone();
    let factory: StreamFn = std::sync::Arc::new(move |_ctx, _opts| {
        let n = llm_calls_clone.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("echo")],
        ctx,
        build_config(),
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    assert_eq!(
        llm_calls.load(Ordering::SeqCst),
        2,
        "two LLM calls expected"
    );
}

/// Port of pi test "should inject queued messages after all
/// tool calls complete" (agent-loop.test.ts:547).
///
/// Setup: assistant emits a tool call. After tool dispatch
/// the loop polls `getSteeringMessages` which returns a user
/// message ONCE. That message is injected before the next
/// assistant call; the second LLM call sees it in its context.
#[tokio::test]
async fn test_steering_messages_injected_after_tool_calls() {
    let echo = std::sync::Arc::new(EchoTool::new());
    let mut ctx = empty_context();
    ctx.tools.push(echo);

    // Steering hook delivers once on the SECOND call (so
    // not on initial poll).
    let poll_count = std::sync::Arc::new(AtomicUsize::new(0));
    let poll_clone = poll_count.clone();
    let steering: GetSteeringMessagesFn = std::sync::Arc::new(move || {
        let poll = poll_clone.clone();
        Box::pin(async move {
            let n = poll.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                vec![user("interrupt")]
            } else {
                Vec::new()
            }
        })
    });

    // Inspector: record what each LLM call sees in its
    // converted message list.
    let saw_interrupt_on_second = std::sync::Arc::new(std::sync::Mutex::new(false));
    let saw_clone = saw_interrupt_on_second.clone();
    let call_counter = std::sync::Arc::new(AtomicUsize::new(0));

    let factory: StreamFn = std::sync::Arc::new(move |llm_ctx, _opts| {
        let n = call_counter.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            // Second call: check for "interrupt" in messages.
            let found = llm_ctx.messages.iter().any(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("user")
                    && m.get("content")
                        .and_then(|c| c.as_str())
                        .map(|s| s.contains("interrupt"))
                        == Some(true)
            });
            *saw_clone.lock().unwrap() = found;
        }
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({"v": 1}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            reason,
            message: msg,
            usage: None,
        }]))
    });

    let mut config = build_config();
    config.get_steering_messages = Some(steering);

    let (tx, mut rx) = mpsc::channel::<LoopEvent>(128);
    let messages = run_agent_loop(
        vec![user("start")],
        ctx,
        config,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;
    drop(tx);

    assert!(
        *saw_interrupt_on_second.lock().unwrap(),
        "second LLM call should see the injected interrupt"
    );

    // Returned messages include the injected interrupt.
    let user_contents: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            LoopMessage::User(u) => Some(u.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(user_contents, vec!["start", "interrupt"]);

    // The interrupt's message_start fires AFTER the tool
    // result's message_end. We verify by event ordering.
    let events = drain(&mut rx).await;
    let interrupt_idx = events.iter().position(|e| match e {
        LoopEvent::MessageStart {
            message: LoopMessage::User(u),
        } => u.content == "interrupt",
        _ => false,
    });
    let last_tool_result_end_idx = events.iter().rposition(|e| {
        matches!(
            e,
            LoopEvent::MessageEnd {
                message: LoopMessage::ToolResult(_)
            }
        )
    });
    assert!(
        interrupt_idx.unwrap() > last_tool_result_end_idx.unwrap(),
        "interrupt should appear AFTER the tool result message_end"
    );
}

// ============================================================
// Phase 6 — regression tests for hardening paths
// ============================================================

use crate::agent::agent_loop::result::LoopToolResult as PhaseSixToolResult;
use std::sync::Arc as PhaseSixArc;

/// Phase 6: a multi-turn run with a network error in turn 2
/// preserves the FULL history (user prompt, turn 1's
/// assistant + tool-result) across the retry. The retry
/// wrapper isn't directly invoked here (we use mock
/// StreamFn), but the LOOP's context.messages survival
/// across turn errors is the invariant.
///
/// We verify by counting context.messages entries the
/// second LLM call observes. The mock StreamFn captures
/// what each call sees.
#[tokio::test]
async fn loop_preserves_history_across_turns() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let observed_lens: PhaseSixArc<Mutex<Vec<usize>>> = PhaseSixArc::new(Mutex::new(Vec::new()));
    let observed_clone = observed_lens.clone();
    let counter = std::sync::Arc::new(AtomicUsize::new(0));

    // Inline echo tool — needed for the tool-result turn
    // that grows the history.
    #[derive(Debug)]
    struct LocalEcho;
    impl LoopTool for LocalEcho {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                Ok(PhaseSixToolResult {
                    content: vec![serde_json::json!({
                        "type": "text",
                        "text": "ok",
                    })],
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    let factory: StreamFn = std::sync::Arc::new(move |ctx: LlmContext, _opts| {
        observed_clone.lock().unwrap().push(ctx.messages.len());
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "echo", serde_json::json!({}))
        } else {
            text_response("done")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.tools.push(PhaseSixArc::new(LocalEcho));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let _ = run_agent_loop(
        vec![user("start")],
        ctx,
        cfg,
        AbortSignal::new(),
        &tx,
        &factory,
        None,
        None, // memory_provider — test default
    )
    .await;

    let lens = observed_lens.lock().unwrap().clone();
    assert_eq!(lens.len(), 2, "expected two LLM calls");
    // First call sees: just user prompt → 1 message.
    assert_eq!(lens[0], 1);
    // Second call sees: user prompt + assistant (tool_use) +
    // tool result → 3 messages. History preserved.
    assert_eq!(
        lens[1], 3,
        "second LLM call should see prior turn's history; got {} messages",
        lens[1],
    );
}

/// Phase 6: full signal-chain regression. Cancel the signal
/// mid-tool; tool aborts; loop's next LLM call's stream
/// observes the same signal and exits via Error path; loop
/// exits cleanly with no infinite-loop or hung tools.
#[tokio::test]
async fn full_signal_chain_exits_cleanly() {
    use crate::agent::agent_loop::stream::{LlmContext, StreamFn};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Mock tool that observes the signal during execution
    // (immediate cancel since the test cancels signal right
    // after spawn).
    #[derive(Debug)]
    struct CancellableTool;
    impl LoopTool for CancellableTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "Cancellable"
        }
        fn label(&self) -> &str {
            "Noop"
        }
        fn parameters(&self) -> &Value {
            static EMPTY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({"type": "object"}))
        }
        fn execute<'a>(
            &'a self,
            _id: &'a str,
            _args: Value,
            _signal: AbortSignal,
            _on_update: super::super::tool::LoopToolUpdate,
        ) -> Pin<Box<dyn Future<Output = Result<PhaseSixToolResult, String>> + Send + 'a>> {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(PhaseSixToolResult {
                    content: Vec::new(),
                    details: Value::Null,
                    terminate: None,
                })
            })
        }
    }

    // Factory that returns a tool_use response first,
    // then would return a text response on retry (but
    // shouldn't get there because signal is cancelled
    // before turn 2).
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let factory: StreamFn = std::sync::Arc::new(move |_ctx: LlmContext, _opts| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let msg = if n == 0 {
            tool_use_response("call-1", "noop", serde_json::json!({}))
        } else {
            text_response("should-not-reach")
        };
        let reason = msg.stop_reason;
        Box::pin(futures::stream::iter(vec![
            crate::agent::agent_loop::message::StreamEvent::Done {
                reason,
                message: msg,
                usage: None,
            },
        ]))
    });

    let mut ctx = empty_context();
    ctx.tools.push(PhaseSixArc::new(CancellableTool));
    let mut cfg = build_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;

    let (tx, _rx) = mpsc::channel::<LoopEvent>(64);
    let signal = AbortSignal::new();
    let signal_clone = signal.clone();

    // Spawn the loop in a task; cancel signal after a small
    // yield so the tool has started.
    let task = tokio::spawn(async move {
        run_agent_loop(
            vec![user("start")],
            ctx,
            cfg,
            signal_clone,
            &tx,
            &factory,
            None,
            None, // memory_provider — test default
        )
        .await
    });
    // Yield twice so the loop reaches the tool dispatch
    // before we cancel.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    signal.cancel();

    // Bound the test: loop must complete in <2s. Without
    // the tool-abort wrap, the 30s blocking tool would
    // exceed this. R3 ensures the next LLM call (if any)
    // also exits promptly via its pre-poll signal check.
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
    assert!(
        result.is_ok(),
        "loop should exit within 2s after signal cancel"
    );
}

// ── dirge-h5tv: build_augmented_focus + transcript helper ──

use crate::extras::memory_provider::MemoryProvider;
use std::sync::Arc;

#[derive(Default)]
struct PreCompressRecorder {
    seen: Mutex<Vec<String>>,
    return_value: Mutex<String>,
}
impl MemoryProvider for PreCompressRecorder {
    fn name(&self) -> &str {
        "pre-compress-recorder"
    }
    fn view(&self, _: &str) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn add(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn replace(&self, _: &str, _: &str, _: &str) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn remove(&self, _: &str, _: &str) -> Result<serde_json::Value, String> {
        Ok(serde_json::Value::Null)
    }
    fn on_pre_compress(&self, transcript: &str) -> String {
        self.seen.lock().unwrap().push(transcript.to_string());
        self.return_value.lock().unwrap().clone()
    }
}

fn make_middle() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"role": "user", "content": "what is rust?"}),
        serde_json::json!({"role": "assistant", "content": "a systems language"}),
    ]
}

#[test]
fn build_augmented_focus_returns_none_with_no_inputs() {
    let result = super::build_augmented_focus(None, None, &make_middle());
    assert!(
        result.is_none(),
        "no focus + no provider must yield None instructions"
    );
}

#[test]
fn build_augmented_focus_preserves_focus_when_no_provider() {
    let result = super::build_augmented_focus(Some("error handling"), None, &make_middle());
    assert_eq!(result.as_deref(), Some("error handling"));
}

#[test]
fn build_augmented_focus_folds_provider_insights_into_focus() {
    let provider = Arc::new(PreCompressRecorder::default());
    *provider.return_value.lock().unwrap() = "user prefers async/await over threads".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result =
        super::build_augmented_focus(Some("retry logic"), Some(&provider_dyn), &make_middle());

    let out = result.expect("focus + insights produces Some");
    assert!(out.contains("retry logic"), "user focus must survive");
    assert!(
        out.contains("user prefers async/await over threads"),
        "provider insight must be folded in: {out}"
    );
    assert!(
        out.contains("Provider insights:"),
        "insights must be labelled so the summarizer can attribute them"
    );

    // Provider received the transcript built from the middle slice.
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "hook fires exactly once");
    assert!(
        seen[0].contains("user: what is rust?")
            && seen[0].contains("assistant: a systems language"),
        "transcript must contain both messages: {:?}",
        seen[0]
    );
}

#[test]
fn build_augmented_focus_yields_insights_alone_when_no_focus() {
    let provider = Arc::new(PreCompressRecorder::default());
    *provider.return_value.lock().unwrap() = "remember the build flags".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result = super::build_augmented_focus(None, Some(&provider_dyn), &make_middle());

    let out = result.expect("insights alone produce Some");
    assert!(out.starts_with("Provider insights:"));
    assert!(out.contains("remember the build flags"));
}

#[test]
fn build_augmented_focus_treats_empty_provider_output_as_none() {
    let provider = Arc::new(PreCompressRecorder::default());
    // Empty string return from on_pre_compress — provider has
    // nothing to contribute this turn.
    *provider.return_value.lock().unwrap() = "".into();
    let provider_dyn: Arc<dyn MemoryProvider> = provider.clone();

    let result = super::build_augmented_focus(None, Some(&provider_dyn), &make_middle());
    assert!(
        result.is_none(),
        "empty provider output + no focus must yield None"
    );

    // But the hook still fired (so it can do internal bookkeeping
    // even if its return is empty).
    assert_eq!(provider.seen.lock().unwrap().len(), 1);
}

#[test]
fn transcript_from_value_slice_renders_role_prefixes() {
    let messages = vec![
        serde_json::json!({"role": "user", "content": "hello"}),
        serde_json::json!({"role": "assistant", "content": "hi"}),
        serde_json::json!({"role": "system", "content": ""}), // empty — skipped
    ];
    let t = super::transcript_from_value_slice(&messages);
    assert!(t.contains("user: hello"));
    assert!(t.contains("assistant: hi"));
    assert!(
        !t.contains("system: "),
        "empty content must be skipped: {t:?}"
    );
}

// =====================================================================
// dirge-ngic — scavenge must inspect both Thinking AND Text blocks.
// Reasonix combines both at `loop.ts:910-913` →
// `repair/index.ts:71`. Previously dirge merged only Thinking, so
// any DSML invoke that streamed as visible content (the common
// case on Anthropic cache hits) was lost.
// =====================================================================

/// dirge-ngic: a DSML invoke that lives only in `ContentBlock::Text`
/// (no Thinking block at all) must be picked up by the scavenger.
/// Proves the run.rs source builder includes Text — without the
/// fix this orphan call goes unrecovered, the model loop stalls
/// waiting for a tool result that never dispatches.
#[test]
fn scavenge_source_recovers_dsml_invoke_from_text_only() {
    let dsml = "<|DSML|invoke name=\"read_file\"><|DSML|parameter name=\"path\" string=\"true\">/tmp/x</|DSML|parameter></|DSML|invoke>";
    let blocks = vec![ContentBlock::Text {
        text: dsml.to_string(),
    }];

    let source = super::build_scavenge_source(&blocks);
    assert!(
        source.contains("DSML"),
        "scavenge source must include Text block content: {source:?}",
    );

    let allowed: std::collections::HashSet<String> =
        ["read_file".to_string()].into_iter().collect();
    let result =
        crate::agent::agent_loop::scavenge::scavenge_tool_calls(Some(&source), &allowed, 4);
    assert_eq!(
        result.calls.len(),
        1,
        "orphan DSML in Text must be recovered: calls={:?}",
        result.calls
    );
    assert_eq!(result.calls[0].name, "read_file");
}

/// dirge-ngic: mixed Thinking + Text content — both contribute to
/// the scavenge corpus. Order is preserved (Thinking first as it
/// streams first), separated by `\n` so DSML on a line boundary
/// doesn't merge with surrounding chatter.
#[test]
fn scavenge_source_concatenates_thinking_and_text() {
    let blocks = vec![
        ContentBlock::Thinking {
            text: "Plan: call list_dir.".to_string(),
        },
        ContentBlock::Text {
            text: "Acting now.".to_string(),
        },
    ];
    let source = super::build_scavenge_source(&blocks);
    assert_eq!(source, "Plan: call list_dir.\nActing now.");
}

/// dirge-ngic: tool-call and other non-text blocks contribute
/// nothing to the scavenge corpus — only Thinking and Text.
#[test]
fn scavenge_source_skips_non_text_blocks() {
    let blocks = vec![
        ContentBlock::Text {
            text: "visible".to_string(),
        },
        ContentBlock::ToolCall {
            id: "call_1".to_string(),
            name: "noop".to_string(),
            arguments: serde_json::json!({}),
        },
    ];
    let source = super::build_scavenge_source(&blocks);
    assert_eq!(source, "visible");
}

// =====================================================================
// dirge-7bwx — truncation repair must run BEFORE storm so two
// streams whose raw args differ but heal to the same form dedupe
// under the storm filter. Reasonix order: `repair/index.ts:88-109`
// (truncation) then `:113-121` (storm).
// =====================================================================

/// dirge-7bwx: two ToolCalls with different truncated arg strings
/// that repair to the same canonical form must, after
/// `apply_truncation_repair`, present identical parsed arguments.
/// Pre-fix these survived storm because their pre-repair raw
/// strings hashed differently and only got repaired at dispatch
/// time, after the de-dupe window had closed.
#[test]
fn truncation_repair_canonicalizes_divergent_streams_before_storm() {
    use crate::agent::agent_loop::tool_input_repair::{RepairKind, RepairStats};
    use crate::agent::agent_loop::tools::ToolCall;

    // Same logical call, different truncation points.
    let call_a_raw = r#"{"path": "/tmp/x""#; // unterminated object
    let call_b_raw = r#"{"path": "/tmp/x"}"#; // already complete
    // Quick sanity: distinct strings → distinct pre-repair sigs.
    assert_ne!(call_a_raw, call_b_raw);

    let mut tool_calls = vec![
        ToolCall {
            id: "call_a".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::Value::String(call_a_raw.to_string()),
        },
        ToolCall {
            id: "call_b".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::Value::String(call_b_raw.to_string()),
        },
    ];

    let stats = RepairStats::new();
    super::apply_truncation_repair(&mut tool_calls, &stats);

    // Truncated A repaired; B was already valid JSON-as-string but
    // parsed-and-replaced.
    assert_eq!(tool_calls[0].arguments, tool_calls[1].arguments);
    assert_eq!(tool_calls[0].arguments["path"], "/tmp/x");
    assert!(
        stats.snapshot().truncation_fixed >= 1,
        "at least the truncated call must record TruncationFixed",
    );
}

/// dirge-7bwx: hard-fallback (closer can't rebalance) does NOT
/// replace arguments. Original `Value::String(raw)` is preserved
/// so `validate_and_repair` downstream surfaces a real validation
/// error rather than silently dispatching a fabricated value —
/// matches Reasonix's invariant at `repair/index.ts:93-102`.
#[test]
fn truncation_repair_preserves_raw_on_hard_fallback() {
    use crate::agent::agent_loop::tool_input_repair::RepairStats;
    use crate::agent::agent_loop::tools::ToolCall;

    let unsalvageable = "}}}garbage no opening".to_string();
    let mut tool_calls = vec![ToolCall {
        id: "call_garbage".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::Value::String(unsalvageable.clone()),
    }];

    let stats = RepairStats::new();
    super::apply_truncation_repair(&mut tool_calls, &stats);

    // Either preserved as the same Value::String, OR if the
    // closer happened to find a structured interpretation, it
    // must NOT be the empty/fabricated `{}` that masks a real
    // error. We test the strict case where fallback fires.
    if let serde_json::Value::String(after) = &tool_calls[0].arguments {
        assert_eq!(
            after, &unsalvageable,
            "hard fallback must not mutate the raw string",
        );
    }
    // Empty object is the canonical fabricated value Reasonix
    // refuses to emit; assert we never silently substitute it.
    assert_ne!(
        tool_calls[0].arguments,
        serde_json::json!({}),
        "hard fallback must not silently fabricate an empty object",
    );
}

/// dirge-7bwx: structurally valid args (real `Value::Object`)
/// pass through untouched — only `Value::String` triggers the
/// repair pass.
#[test]
fn truncation_repair_leaves_already_parsed_args_alone() {
    use crate::agent::agent_loop::tool_input_repair::{RepairKind, RepairStats};
    use crate::agent::agent_loop::tools::ToolCall;

    let already_parsed = serde_json::json!({ "path": "/tmp/y" });
    let mut tool_calls = vec![ToolCall {
        id: "call_ok".to_string(),
        name: "read_file".to_string(),
        arguments: already_parsed.clone(),
    }];

    let stats = RepairStats::new();
    super::apply_truncation_repair(&mut tool_calls, &stats);

    assert_eq!(tool_calls[0].arguments, already_parsed);
    assert_eq!(
        stats.snapshot().truncation_fixed,
        0,
        "no repair should be recorded for already-parsed args",
    );
}
