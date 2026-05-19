use compact_str::CompactString;
use futures::StreamExt;
use rig::agent::{Agent, MultiTurnStreamItem};
use rig::completion::{CompletionModel, Message};
use rig::message::ToolResultContent;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::recovery::{self, ErrorKind, RecoveryPolicy};
use crate::agent::tools::ToolCache;
use crate::event::AgentEvent;
use crate::session::{MessageRole, Session};

pub struct AgentRunner {
    pub event_rx: mpsc::Receiver<AgentEvent>,
    /// Handle to the spawned tokio task. The UI calls `abort()` on interrupt
    /// so in-flight LLM calls and tool execution actually stop, rather than
    /// running to completion in the background and emitting permission
    /// prompts after the user thought they cancelled.
    pub task: JoinHandle<()>,
}

pub fn convert_history(session: &Session) -> Vec<Message> {
    let (summary, first_kept) = session.compacted_context();
    let mut messages = Vec::new();

    if let Some(summary) = summary {
        messages.push(Message::system(format!(
            "[Previous conversation summary]\n{}",
            summary
        )));
    }

    for msg in &session.messages[first_kept..] {
        match msg.role {
            MessageRole::User => messages.push(Message::user(msg.content.to_string())),
            MessageRole::Assistant => messages.push(Message::assistant(msg.content.to_string())),
            MessageRole::System => messages.push(Message::system(msg.content.to_string())),
        }
    }

    messages
}

/// Outcome of a streaming pass — used by the retry loop to decide whether
/// it's safe to re-issue the request. We never buffer events themselves;
/// they're sent to the UI as they arrive so the user sees progress in real
/// time.
#[derive(Default)]
struct StreamOutcome {
    had_tool_calls: bool,
    error: Option<String>,
}

async fn run_stream<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    history: Vec<Message>,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> StreamOutcome
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut outcome = StreamOutcome::default();
    let mut stream = agent.stream_chat(prompt.to_string(), history).await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                let _ = event_tx
                    .send(AgentEvent::Token(CompactString::from(text.text)))
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                let _ = event_tx
                    .send(AgentEvent::Reasoning(CompactString::new(r.display_text())))
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                outcome.had_tool_calls = true;
                let _ = event_tx
                    .send(AgentEvent::ToolCall {
                        name: CompactString::from(tool_call.function.name),
                        args: tool_call.function.arguments,
                    })
                    .await;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                outcome.had_tool_calls = true;
                let mut output = String::new();
                for c in tool_result.content.iter() {
                    if let ToolResultContent::Text(t) = c {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&t.text);
                    }
                }
                let _ = event_tx
                    .send(AgentEvent::ToolResult {
                        output: CompactString::from(output),
                    })
                    .await;
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let response_text = res.response();
                let estimated_tokens = Session::estimate_tokens(response_text);
                let _ = event_tx
                    .send(AgentEvent::Done {
                        response: CompactString::from(response_text),
                        tokens: estimated_tokens,
                        cost: 0.0,
                    })
                    .await;
                return outcome;
            }
            Err(e) => {
                outcome.error = Some(e.to_string());
                return outcome;
            }
            _ => {}
        }
    }
    outcome
}

pub fn spawn_agent<M, P>(
    agent: Agent<M, P>,
    prompt: String,
    history: Vec<Message>,
    cache: ToolCache,
) -> AgentRunner
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    cache.clear();
    let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);

    let task = tokio::spawn(async move {
        let policy = RecoveryPolicy::default();
        let mut attempts = 0;

        loop {
            let outcome = run_stream(&agent, &prompt, history.clone(), &event_tx).await;

            let msg = match outcome.error {
                None => break,
                Some(m) => m,
            };

            let kind = recovery::classify_error(&msg);

            // Auth and unknown errors surface immediately
            if kind == ErrorKind::Auth || kind == ErrorKind::Other {
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(msg)))
                    .await;
                break;
            }

            // Context-length errors: not retryable without compaction
            // Surface a helpful error hinting at /compress
            if kind == ErrorKind::ContextLength {
                let hint = format!(
                    "{} — try /compress to compact the conversation, then retry",
                    msg
                );
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(hint)))
                    .await;
                break;
            }

            // If any tool calls were dispatched, their side effects already
            // executed. Retrying would re-run them. Surface the error
            // without retrying — events already streamed live, so the user
            // sees what got done.
            if outcome.had_tool_calls {
                let err = format!("{} (tool side effects already applied, not retrying)", msg);
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(err)))
                    .await;
                break;
            }

            if !policy.should_retry(attempts, kind) {
                let retry_msg = format!("{} (retries exhausted)", msg);
                let _ = event_tx
                    .send(AgentEvent::Error(CompactString::new(retry_msg)))
                    .await;
                break;
            }

            // Emit retry notification as reasoning
            let retry_msg = format!(
                "retrying ({kind:?} error, attempt {attempt}/{max})...",
                kind = kind,
                attempt = attempts + 1,
                max = policy.max_retries(),
            );
            let _ = event_tx
                .send(AgentEvent::Reasoning(CompactString::new(retry_msg)))
                .await;

            let delay = policy.backoff_duration(attempts);
            tokio::time::sleep(delay).await;
            attempts += 1;
        }
    });

    AgentRunner { event_rx, task }
}

pub async fn run_print<M, P>(
    agent: &Agent<M, P>,
    prompt: &str,
    max_turns: usize,
) -> anyhow::Result<String>
where
    M: CompletionModel + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
    P: rig::agent::PromptHook<M> + 'static,
{
    let mut stream = agent
        .stream_chat(prompt.to_string(), Vec::<Message>::new())
        .multi_turn(max_turns)
        .await;

    let mut full_response = String::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                full_response.push_str(&text.text);
                print!("{}", text.text);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                r,
            ))) => {
                eprint!("{}", r.display_text());
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    println!();
    Ok(full_response)
}
