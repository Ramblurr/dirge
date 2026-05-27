//! Compaction summarization.
//!
//! Serializes conversation history into a prompt for the summarizer
//! model and invokes the model with retry logic. Extracted from
//! `provider/mod.rs`.

use crate::session::{MessageRole, SessionMessage, ToolCallState};

use rig::streaming::StreamingChat;

/// Serialize the full conversation prefix for compaction summarization.
/// Returns a formatted string with all messages including tool calls
/// (args + results), truncated per-tool at 2KB for memory safety.
pub(crate) fn serialize_conversation(messages: &[SessionMessage]) -> String {
    let mut result = String::new();
    for msg in messages {
        let role_tag = match msg.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
        };
        result.push_str(&format!("[{}]: {}\n", role_tag, msg.content));
        for tc in &msg.tool_calls {
            let args_str = serde_json::to_string(&tc.args).unwrap_or_else(|_| "{}".to_string());
            result.push_str(&format!("[Tool: {}({})]\n", tc.name, args_str));
            match &tc.state {
                ToolCallState::Completed { result: out } => {
                    const PER_TOOL_CAP: usize = 2048;
                    if out.len() > PER_TOOL_CAP {
                        let trimmed: String = out.chars().take(PER_TOOL_CAP).collect();
                        result.push_str(&format!(
                            "[Result: {} ... (truncated, {} bytes total)]\n",
                            trimmed,
                            out.len()
                        ));
                    } else {
                        result.push_str(&format!("[Result: {}]\n", out));
                    }
                }
                ToolCallState::Interrupted => {
                    result.push_str("[Result: <interrupted>]\n");
                }
                ToolCallState::Failed { error } => {
                    result.push_str(&format!("[Result: <failed: {}>]\n", error));
                }
            }
        }
        result.push('\n');
    }
    result
}

/// Call the summarizer model with the full conversation prefix.
/// The summarizer is invoked by `/compress`, often exactly when the
/// user's context is about to overflow. Uses a retry loop with the
/// same `RecoveryPolicy` shape as the main agent.
///
/// PROV-9: bound the prompt size before dispatch. `/compress` is
/// typically invoked when the conversation already exceeds the
/// model's context window — handing the same un-bounded blob to
/// the summarizer guarantees a ContextLength failure. Cap at
/// roughly the summarizer's input budget (≈ 50% of a 64k window).
/// We can't know the exact window for every provider here, so use
/// a conservative absolute cap and let the per-message truncation
/// handle the rest; the head-and-tail strategy preserves the most
/// recent turns (where the recent context lives) plus the earliest
/// turns (which often set up the task).
pub(crate) async fn summarize_with_model(
    model: super::AnyModel,
    mut prompt: String,
) -> anyhow::Result<String> {
    const SUMMARIZER_PROMPT_BUDGET_BYTES: usize = 128 * 1024; // ~32k tokens
    if prompt.len() > SUMMARIZER_PROMPT_BUDGET_BYTES {
        prompt = head_tail_truncate(&prompt, SUMMARIZER_PROMPT_BUDGET_BYTES);
    }
    match model {
        super::AnyModel::OpenRouter(m) => run_summarizer(m, prompt).await,
        super::AnyModel::OpenAI(m) => run_summarizer(m, prompt).await,
        super::AnyModel::Anthropic(m) => run_summarizer(m, prompt).await,
        super::AnyModel::Gemini(m) => run_summarizer(m, prompt).await,
        super::AnyModel::DeepSeek(m) => run_summarizer(m, prompt).await,
        super::AnyModel::Glm(m) => run_summarizer(m, prompt).await,
        super::AnyModel::Ollama(m) => run_summarizer(m, prompt).await,
        super::AnyModel::Custom(m) => run_summarizer(m, prompt).await,
    }
}

/// Trim a prompt to `budget` bytes by keeping a head + tail slice
/// with a placeholder noting the drop. Splits on `\n` so we don't
/// land mid-message. Used by the summarizer when the conversation
/// blob would overflow the summarizer's own context window.
pub(crate) fn head_tail_truncate(prompt: &str, budget: usize) -> String {
    if prompt.len() <= budget {
        return prompt.to_string();
    }
    // 40% head, 60% tail — recent context tends to matter more.
    let head_budget = budget * 4 / 10;
    let tail_budget = budget - head_budget - 128; // leave room for the marker

    // Find a newline-aligned head boundary at or before head_budget.
    let head_end = prompt[..head_budget.min(prompt.len())]
        .rfind('\n')
        .unwrap_or(head_budget.min(prompt.len()));
    // Walk back to a UTF-8 char boundary.
    let mut head_end = head_end;
    while head_end > 0 && !prompt.is_char_boundary(head_end) {
        head_end -= 1;
    }

    let tail_start_target = prompt.len().saturating_sub(tail_budget);
    let tail_start = prompt[tail_start_target..]
        .find('\n')
        .map(|i| tail_start_target + i + 1)
        .unwrap_or(tail_start_target);
    let mut tail_start = tail_start;
    while tail_start < prompt.len() && !prompt.is_char_boundary(tail_start) {
        tail_start += 1;
    }

    if tail_start <= head_end {
        // The two halves overlap — prompt is already short enough
        // after newline rounding. Fall through to verbatim.
        return prompt.to_string();
    }
    let dropped = tail_start - head_end;
    format!(
        "{}\n\n[... {} bytes truncated by summarizer-prompt budget ...]\n\n{}",
        &prompt[..head_end],
        dropped,
        &prompt[tail_start..],
    )
}

async fn run_summarizer<M>(model: M, prompt: String) -> anyhow::Result<String>
where
    M: rig::completion::CompletionModel + Clone + 'static,
    M::StreamingResponse: Send + Sync + Unpin + Clone + 'static,
{
    use crate::agent::recovery::{self, RecoveryPolicy};
    let policy = RecoveryPolicy::default();
    let mut attempts: usize = 0;
    loop {
        let agent = rig::agent::AgentBuilder::new(model.clone())
            .preamble("You are a conversation summarizer.")
            .build();

        let mut stream = agent
            .stream_chat(prompt.clone(), Vec::<rig::completion::Message>::new())
            .multi_turn(1)
            .await;

        let mut response = String::new();
        let mut error: Option<String> = None;
        use futures::StreamExt;
        while let Some(item) = stream.next().await {
            match item {
                Ok(rig::agent::MultiTurnStreamItem::StreamAssistantItem(
                    rig::streaming::StreamedAssistantContent::Text(text),
                )) => response.push_str(&text.text),
                Ok(rig::agent::MultiTurnStreamItem::FinalResponse(res)) => {
                    response = res.response().to_string();
                    break;
                }
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
                _ => {}
            }
        }

        if let Some(msg) = error {
            let kind = recovery::classify_error(&msg);
            if policy.should_retry(attempts, kind) {
                let delay = policy.backoff_duration_for_msg(attempts, &msg);
                tracing::info!(
                    "summarizer retry {}/{} after {:?} ({:?}): {}",
                    attempts + 1,
                    policy.max_retries(),
                    delay,
                    kind,
                    msg
                );
                tokio::time::sleep(delay).await;
                attempts += 1;
                continue;
            }
            return Err(anyhow::anyhow!("Compression failed: {}", msg));
        }

        if response.is_empty() {
            anyhow::bail!("Compression returned empty response");
        }

        return Ok(response);
    }
}

#[cfg(test)]
mod tests {
    use super::head_tail_truncate;

    #[test]
    fn head_tail_truncate_short_prompt_passes_through() {
        let s = "line 1\nline 2\nline 3";
        assert_eq!(head_tail_truncate(s, 1024), s);
    }

    #[test]
    fn head_tail_truncate_keeps_head_and_tail() {
        let mut s = String::new();
        for i in 0..2000 {
            s.push_str(&format!("line {}\n", i));
        }
        let out = head_tail_truncate(&s, 4096);
        assert!(out.len() < s.len(), "output should be shorter");
        assert!(out.starts_with("line 0\n"));
        assert!(out.contains("truncated by summarizer-prompt budget"));
        assert!(out.ends_with("line 1999\n"));
    }
}
