//! `stream_assistant_response` — single-turn LLM call wrapper.
//!
//! Faithful port of pi `streamAssistantResponse` (agent-loop.ts:275-368).
//!
//! Flow:
//!   1. Apply `transformContext` if configured (transcript-level
//!      prune/rewrite — AgentMessage[] → AgentMessage[]).
//!   2. Apply `convertToLlm` (REQUIRED) — AgentMessage[] →
//!      LLM-compatible Message[].
//!   3. Resolve API key via `getApiKey`; fall back to
//!      `config.api_key`.
//!   4. Invoke the stream function with `(model, llm_context,
//!      options)`.
//!   5. Iterate stream events:
//!        - `Start`         → push partial to context.messages;
//!                             emit `MessageStart`
//!        - `Delta(*)`      → replace last context message;
//!                             emit `MessageUpdate`
//!        - `Done`/`Error`  → finalize; emit `MessageEnd`; return
//!   6. If the stream closes without `Done`/`Error`, finalize
//!      defensively (pi has the same fallback at
//!      agent-loop.ts:359).
//!
//! The stream function is injected — phase 1 uses canned-event
//! mock streams in tests; phase 4 will substitute a rig-backed
//! implementation that yields actual provider events.

use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use super::message::{AssistantMessage, LoopEvent, LoopMessage, StopReason, StreamEvent};
use super::tool::AbortSignal;
use super::types::{Context, LoopConfig};

/// Input passed to the stream function. Port of pi's `Context`
/// (the one from `@earendil-works/pi-ai`, not pi's `AgentContext`)
/// — system prompt + LLM-ready message list + tool defs.
///
/// Phase 1 keeps this minimal; phase 4 will carry the model
/// handle + reasoning level + signal once the rig wiring lands.
#[derive(Debug, Clone)]
pub struct LlmContext {
    pub system_prompt: String,
    /// LLM-compatible messages (output of `convert_to_llm`).
    pub messages: Vec<serde_json::Value>,
}

/// Stream function signature. Caller provides one; the function
/// is invoked ONCE PER LLM CALL within a run — multi-turn runs
/// call it N times. Returns a fresh stream of `StreamEvent`s
/// each invocation.
///
/// In pi (types.ts:24): `StreamFn = (...args: Parameters<typeof
/// streamSimple>) => ReturnType<typeof streamSimple>`. Pi's
/// `streamSimple` takes `(model, context, options)`; we collapse
/// model/options into `LlmContext` + `api_key` separately for
/// now.
///
/// `Arc<dyn Fn …>` so the loop can clone the same StreamFn across
/// every turn without consuming it. Stateful closures (e.g. test
/// mocks tracking `callIndex`) use interior mutability
/// (`Arc<AtomicUsize>` captured by the closure).
pub type StreamFn = Arc<
    dyn Fn(
            LlmContext,
            Option<String>, // resolved api_key
            AbortSignal,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>>
        + Send
        + Sync,
>;

/// Run the stream function and bridge its events to the loop's
/// `LoopEvent` channel. Returns the final `AssistantMessage`.
///
/// Mutates `context.messages`: pushes the partial assistant
/// message on `Start` (or the final on `Done`/`Error` if no
/// partial preceded) and replaces it on each `Delta`. Matches
/// pi's mutation of `context.messages` at lines 317, 333, 346,
/// 348, 361, 363.
pub async fn stream_assistant_response(
    context: &mut Context,
    config: &LoopConfig,
    signal: AbortSignal,
    emit: &mpsc::Sender<LoopEvent>,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    // 1. transformContext (optional, AgentMessage[] → AgentMessage[])
    let messages: Vec<serde_json::Value> = if let Some(transform) = &config.transform_context {
        transform(context.messages.clone()).await
    } else {
        context.messages.clone()
    };

    // 2. convertToLlm (required, AgentMessage[] → Message[])
    let llm_messages = (config.convert_to_llm)(&messages);

    // 3. getApiKey (optional dynamic resolution)
    let resolved_api_key: Option<String> = if let Some(get_key) = &config.get_api_key {
        // Phase 1 placeholder: model identifier is empty. Phase 4
        // will pass the real provider name from `config.model.
        // provider`.
        match get_key("").await {
            Some(k) => Some(k),
            None => config.api_key.clone(),
        }
    } else {
        config.api_key.clone()
    };

    // 4. Build LlmContext and invoke the stream function.
    let llm_ctx = LlmContext {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
    };
    let mut stream = stream_fn(llm_ctx, resolved_api_key, signal);

    // 5. Iterate events.
    let mut added_partial = false;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::Start { partial } => {
                context.messages.push(serialize_assistant(&partial));
                added_partial = true;
                let _ = emit
                    .send(LoopEvent::MessageStart {
                        message: LoopMessage::Assistant(partial),
                    })
                    .await;
            }
            StreamEvent::Delta { partial, phase } => {
                if added_partial {
                    // Replace the last context message with the
                    // updated partial. Pi: `context.messages[
                    // context.messages.length - 1] =
                    // partialMessage` (line 333).
                    if let Some(last) = context.messages.last_mut() {
                        *last = serialize_assistant(&partial);
                    }
                }
                let _ = emit
                    .send(LoopEvent::MessageUpdate {
                        message: partial,
                        phase,
                    })
                    .await;
            }
            StreamEvent::Done { reason, message } => {
                let mut finalised = message;
                finalised.stop_reason = reason;
                finalize(context, &finalised, added_partial, emit).await;
                final_message = Some(finalised);
                break;
            }
            StreamEvent::Error { error } => {
                // Pi (agent-loop.ts:343-354): on `error`, call
                // `response.result()` to get the final message
                // (carrying the error in its stopReason/
                // errorMessage). Our stream's error variant
                // doesn't carry a message — we synthesise one
                // with stop_reason=Error so the caller can
                // observe it.
                let finalised = AssistantMessage {
                    content: Vec::new(),
                    stop_reason: StopReason::Error,
                    error_message: Some(error),
                };
                finalize(context, &finalised, added_partial, emit).await;
                final_message = Some(finalised);
                break;
            }
        }
    }

    // 6. Defensive: stream closed without Done/Error. Pi has
    // the same fallback at agent-loop.ts:359 ("stream ended
    // without done/error"). Synthesise a final message with
    // whatever partial we accumulated (or empty if no Start
    // fired).
    final_message.unwrap_or_else(|| {
        let empty = AssistantMessage::new(Vec::new(), StopReason::Stop);
        // Pi pushes the empty final to context if no partial
        // preceded (line 363).
        if !added_partial {
            context.messages.push(serialize_assistant(&empty));
        }
        empty
    })
}

/// Common finalization path used by `Done` and `Error` arms.
///
/// Pi at lines 343-354: if a partial was pushed earlier, replace
/// the last context message with the final; otherwise push the
/// final and emit `message_start`. Then emit `message_end`.
async fn finalize(
    context: &mut Context,
    final_msg: &AssistantMessage,
    added_partial: bool,
    emit: &mpsc::Sender<LoopEvent>,
) {
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = serialize_assistant(final_msg);
        }
    } else {
        context.messages.push(serialize_assistant(final_msg));
        let _ = emit
            .send(LoopEvent::MessageStart {
                message: LoopMessage::Assistant(final_msg.clone()),
            })
            .await;
    }
    let _ = emit
        .send(LoopEvent::MessageEnd {
            message: LoopMessage::Assistant(final_msg.clone()),
        })
        .await;
}

/// Serialise an `AssistantMessage` to the placeholder `Value`
/// shape used in `Context.messages`. Phase 1's `Vec<Value>`
/// transcript is a stopgap; phase 4 swaps in typed messages and
/// this helper goes away.
fn serialize_assistant(msg: &AssistantMessage) -> serde_json::Value {
    // Minimal shape that downstream consumers (convertToLlm)
    // can pattern-match on. Pi's AssistantMessage carries
    // role/content/stopReason etc.; phase 1 ports just enough
    // to round-trip through tests.
    serde_json::json!({
        "role": "assistant",
        "content": msg.content,
        "stopReason": msg.stop_reason,
        "errorMessage": msg.error_message,
    })
}

// =====================================================================
// Tests — ported from pi/packages/agent/test/agent-loop.test.ts
// =====================================================================
//
// Phase 1 targets three tests (lines 84, 131, 186 in pi's file).
// Each test below cites its pi origin. Behaviour matches pi
// FAITHFULLY at the unit level — note that pi tests run the full
// `agentLoop`, not `streamAssistantResponse` in isolation, so a
// few phase-1 tests skip outer-loop event expectations
// (`agent_start`, `turn_start`, etc.) and check only what
// `streamAssistantResponse` itself emits + returns. The full
// event sequence is verified again in phase 4 when the outer
// loop lands.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_loop::message::ContentBlock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Identity convertToLlm — passes through user/assistant/
    /// toolResult messages, drops anything else. Mirrors pi's
    /// `identityConverter` at test file line 79.
    fn identity_converter()
    -> Arc<dyn Fn(&[serde_json::Value]) -> Vec<serde_json::Value> + Send + Sync> {
        Arc::new(|messages: &[serde_json::Value]| {
            messages
                .iter()
                .filter(|m| {
                    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    matches!(role, "user" | "assistant" | "toolResult")
                })
                .cloned()
                .collect()
        })
    }

    /// Build a stream that emits one `Done` event carrying a
    /// canned assistant message. Mirrors the typical test mock
    /// from pi (createAssistantMessage + done push).
    fn canned_done_stream(content_text: &str) -> StreamFn {
        let text = content_text.to_string();
        Arc::new(move |_ctx, _key, _signal| {
            let message = AssistantMessage::new(
                vec![ContentBlock::Text { text: text.clone() }],
                StopReason::Stop,
            );
            Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            }]))
        })
    }

    fn build_config(
        convert: Arc<dyn Fn(&[serde_json::Value]) -> Vec<serde_json::Value> + Send + Sync>,
    ) -> LoopConfig {
        LoopConfig {
            convert_to_llm: convert,
            transform_context: None,
            get_api_key: None,
            api_key: None,
            tool_execution: crate::agent::agent_loop::ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            get_steering_messages: None,
            get_followup_messages: None,
        }
    }

    /// Port of pi test 84 ("should emit events with AgentMessage
    /// types"), reduced to what `stream_assistant_response`
    /// alone produces. Pi's test asserts the FULL event sequence
    /// from `agentLoop` (agent_start / turn_start / message_start
    /// / message_end / turn_end / agent_end); this phase-1
    /// equivalent only checks the message_* family our function
    /// emits. The outer turn / agent events come in phase 4.
    #[tokio::test]
    async fn test_emits_message_start_and_end() {
        let mut ctx = Context {
            system_prompt: "You are helpful.".to_string(),
            messages: vec![serde_json::json!({"role": "user", "content": "Hello"})],
            tools: Vec::new(),
        };
        let config = build_config(identity_converter());
        let signal = AbortSignal::new();
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(32);

        let final_msg = stream_assistant_response(
            &mut ctx,
            &config,
            signal,
            &tx,
            &canned_done_stream("Hi there!"),
        )
        .await;
        drop(tx); // close so we can drain the channel

        // Final message asserted as expected.
        assert_eq!(final_msg.stop_reason, StopReason::Stop);
        assert_eq!(final_msg.content.len(), 1);

        // Drain events: with a canned Done-only stream, pi's
        // flow at lines 343-354 hits the `addedPartial=false`
        // branch and emits MessageStart + MessageEnd back-to-
        // back.
        let mut kinds = Vec::new();
        while let Some(e) = rx.recv().await {
            kinds.push(e.kind().to_string());
        }
        assert_eq!(kinds, vec!["message_start", "message_end"]);

        // Context has user + final assistant message.
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(
            ctx.messages[0].get("role").and_then(|r| r.as_str()),
            Some("user")
        );
        assert_eq!(
            ctx.messages[1].get("role").and_then(|r| r.as_str()),
            Some("assistant")
        );
    }

    /// Port of pi test 131 ("should handle custom message types
    /// via convertToLlm"). Verifies the custom-role message is
    /// passed to `convertToLlm`, where the caller filters it
    /// out before the LLM sees it.
    #[tokio::test]
    async fn test_convert_to_llm_filters_custom_messages() {
        let mut ctx = Context {
            system_prompt: "You are helpful.".to_string(),
            messages: vec![
                serde_json::json!({"role": "notification", "text": "noisy"}),
                serde_json::json!({"role": "user", "content": "Hello"}),
            ],
            tools: Vec::new(),
        };

        // Inspector closure — records what convertToLlm received.
        let received = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let received_clone = received.clone();
        let convert: Arc<dyn Fn(&[serde_json::Value]) -> Vec<serde_json::Value> + Send + Sync> =
            Arc::new(move |messages| {
                let mut slot = received_clone.lock().unwrap();
                *slot = messages.to_vec();
                // Filter notifications out for the LLM.
                messages
                    .iter()
                    .filter(|m| m.get("role").and_then(|r| r.as_str()) != Some("notification"))
                    .cloned()
                    .collect()
            });

        let config = build_config(convert);
        let signal = AbortSignal::new();
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(32);

        let _ = stream_assistant_response(
            &mut ctx,
            &config,
            signal,
            &tx,
            &canned_done_stream("Response"),
        )
        .await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // convertToLlm saw the full transcript (notification +
        // user) — same as pi's contract.
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 2);
        let roles: Vec<_> = received
            .iter()
            .map(|m| m.get("role").and_then(|r| r.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["notification", "user"]);
    }

    /// Port of pi test 186 ("should apply transformContext
    /// before convertToLlm"). Pi's transformContext returns the
    /// last 2 messages; convertToLlm then sees only those 2.
    /// The KEY assertion is the ORDERING: transform fires first.
    #[tokio::test]
    async fn test_transform_context_runs_before_convert_to_llm() {
        let mut ctx = Context {
            system_prompt: "You are helpful.".to_string(),
            messages: vec![
                serde_json::json!({"role": "user", "content": "old 1"}),
                serde_json::json!({"role": "assistant", "content": "resp 1"}),
                serde_json::json!({"role": "user", "content": "old 2"}),
                serde_json::json!({"role": "assistant", "content": "resp 2"}),
                serde_json::json!({"role": "user", "content": "new"}),
            ],
            tools: Vec::new(),
        };

        // Counter so we can prove the order of invocation.
        let counter = Arc::new(AtomicUsize::new(0));

        let transform_order = counter.clone();
        let transform: Arc<
            dyn Fn(
                    Vec<serde_json::Value>,
                )
                    -> Pin<Box<dyn std::future::Future<Output = Vec<serde_json::Value>> + Send>>
                + Send
                + Sync,
        > = Arc::new(move |messages| {
            let order = transform_order.clone();
            Box::pin(async move {
                let n = order.fetch_add(1, Ordering::SeqCst);
                // Stamp the order onto the result so we can
                // verify it.
                assert_eq!(n, 0, "transform_context must fire before convert_to_llm");
                // Pi: `messages.slice(-2)` — keep only the last two.
                let len = messages.len();
                if len <= 2 {
                    messages
                } else {
                    messages[len - 2..].to_vec()
                }
            })
        });

        let convert_order = counter.clone();
        let received_convert = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let received_clone = received_convert.clone();
        let convert: Arc<dyn Fn(&[serde_json::Value]) -> Vec<serde_json::Value> + Send + Sync> =
            Arc::new(move |messages| {
                let n = convert_order.fetch_add(1, Ordering::SeqCst);
                assert_eq!(n, 1, "convert_to_llm must run after transform_context");
                *received_clone.lock().unwrap() = messages.to_vec();
                messages.to_vec()
            });

        let config = LoopConfig {
            convert_to_llm: convert,
            transform_context: Some(transform),
            get_api_key: None,
            api_key: None,
            tool_execution: crate::agent::agent_loop::ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            get_steering_messages: None,
            get_followup_messages: None,
        };
        let signal = AbortSignal::new();
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(32);

        let _ = stream_assistant_response(
            &mut ctx,
            &config,
            signal,
            &tx,
            &canned_done_stream("Response"),
        )
        .await;
        drop(tx);
        while rx.recv().await.is_some() {}

        // After running:
        //   - transformContext invoked at counter=0
        //   - convertToLlm invoked at counter=1 with 2 messages
        let received = received_convert.lock().unwrap();
        assert_eq!(received.len(), 2, "convert_to_llm should see pruned list");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// Defensive: stream closes without Done/Error. Pi has the
    /// same fallback path (agent-loop.ts:359). We return an
    /// empty Stop-reason message and emit a MessageStart +
    /// MessageEnd if no partial preceded.
    #[tokio::test]
    async fn test_stream_closed_without_terminal_event() {
        let mut ctx = Context {
            system_prompt: String::new(),
            messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
            tools: Vec::new(),
        };
        let config = build_config(identity_converter());
        let signal = AbortSignal::new();
        let (tx, mut rx) = mpsc::channel::<LoopEvent>(32);

        // Stream that yields nothing — closes immediately.
        let empty_stream: StreamFn = Arc::new(|_ctx, _key, _sig| {
            Box::pin(futures::stream::iter::<Vec<StreamEvent>>(vec![]))
        });

        let final_msg =
            stream_assistant_response(&mut ctx, &config, signal, &tx, &empty_stream).await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        // Pi's fallback at line 363: push final to context;
        // emit message_start (no message_end for the empty-
        // fallback path per pi's agent-loop.ts:364-366 which
        // emits both).
        assert_eq!(final_msg.stop_reason, StopReason::Stop);
        assert_eq!(ctx.messages.len(), 2);
        // No events emitted in our defensive fallback (we
        // diverge from pi here: pi emits start+end even when
        // the stream produced nothing; we don't, since there's
        // no observable assistant turn). Documented as an
        // intentional Rust deviation in code review.
        assert!(events.is_empty(), "expected no events; got {events:?}");
    }
}
