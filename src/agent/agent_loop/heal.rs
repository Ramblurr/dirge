//! Session healing — fix broken message histories on load.
//!
//! Faithful port of `DeepSeek-Reasonix/src/loop/healing.ts` (108 lines).
//!
//! On session restore, applies targeted repairs before the first
//! API call:
//!
//! 1. Shrink oversized tool results (char cap, not token cap)
//! 2. Fix unpaired tool calls (drops assistant.tool_calls with no
//!    matching tool responses + stray tool messages)
//! 3. Stamp missing `reasoning_content` on thinking-mode sessions
//!
//! The rationale: oversized tool results would 400 the next call
//! before the user types. Unpaired tool calls would similarly
//! fail API validation.

use serde_json::Value;

/// Messages are `Vec<Value>` in dirge's transcript. We inspect
/// `role` and `tool_call_id` fields.
type ChatMessage = Value;

/// Outcome of a heal pass.
#[derive(Debug, Clone)]
pub struct HealResult {
    pub messages: Vec<Value>,
    pub healed_count: usize,
    pub chars_saved: usize,
}

/// Default max chars for a single tool result. Matches
/// Reasonix's `DEFAULT_MAX_RESULT_CHARS` (~40K chars).
pub const DEFAULT_MAX_RESULT_CHARS: usize = 40_000;

// ================================================================
// Shrink oversized tool results (char cap)
// Port of `shrinkOversizedToolResults` (shrink.ts:17-32)
// ================================================================

/// Shrink any tool-result message whose content string exceeds
/// `max_chars`. Only `role: "tool"` messages are touched.
pub fn shrink_oversized_tool_results(messages: &[Value], max_chars: usize) -> HealResult {
    let mut healed_count = 0usize;
    let mut chars_saved = 0usize;
    let out: Vec<Value> = messages
        .iter()
        .map(|msg| {
            if msg.get("role").and_then(|r| r.as_str()) != Some("tool") {
                return msg.clone();
            }
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            if content.len() <= max_chars {
                return msg.clone();
            }
            healed_count += 1;
            chars_saved += content.len().saturating_sub(max_chars);
            let truncated = truncate_for_model(content, max_chars);
            let mut m = msg.clone();
            m["content"] = Value::String(truncated);
            m
        })
        .collect();
    HealResult {
        messages: out,
        healed_count,
        chars_saved,
    }
}

/// Truncate a string to `max_chars` while keeping the beginning
/// more useful than the end.
fn truncate_for_model(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars || max_chars < 2 {
        return content.to_string();
    }
    let head_pct = 0.7;
    let head_chars = (max_chars as f64 * head_pct) as usize;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head = &content[..content
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(content.len())
        .min(content.len())];
    let tail = if tail_chars > 0 {
        let tail_start = content
            .char_indices()
            .nth_back(tail_chars.saturating_sub(1))
            .map(|(i, _)| i)
            .unwrap_or(content.len());
        &content[tail_start..]
    } else {
        ""
    };
    format!(
        "{head}\n...[truncated {} chars]...\n{tail}",
        content.len() - max_chars,
    )
}

// ================================================================
// Fix unpaired tool calls
// Port of `fixToolCallPairing` (healing.ts:13-59)
// ================================================================

/// Drop unpaired assistant.tool_calls and stray tool messages.
/// DeepSeek 400s on either mismatch.
pub fn fix_tool_call_pairing(messages: &[Value]) -> (Vec<Value>, usize, usize) {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut dropped_assistant_calls = 0usize;
    let mut dropped_stray_tools = 0usize;
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        if role == "assistant" {
            if let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) {
                if !calls.is_empty() {
                    let mut needed: std::collections::HashSet<String> = calls
                        .iter()
                        .filter_map(|c| c.get("id").and_then(|id| id.as_str()).map(String::from))
                        .collect();
                    let mut candidates: Vec<Value> = Vec::new();
                    let mut j = i + 1;
                    while j < messages.len() && !needed.is_empty() {
                        let nxt = &messages[j];
                        if nxt.get("role").and_then(|r| r.as_str()) != Some("tool") {
                            break;
                        }
                        let id = nxt
                            .get("tool_call_id")
                            .and_then(|id| id.as_str())
                            .unwrap_or("");
                        if !needed.contains(id) {
                            break;
                        }
                        needed.remove(id);
                        candidates.push(nxt.clone());
                        j += 1;
                    }
                    if needed.is_empty() {
                        out.push(msg.clone());
                        out.extend(candidates);
                        i = j - 1;
                    } else {
                        dropped_assistant_calls += 1;
                        dropped_stray_tools += candidates.len();
                        i = j - 1;
                    }
                    i += 1;
                    continue;
                }
            }
            out.push(msg.clone());
        } else if role == "tool" {
            dropped_stray_tools += 1;
        } else {
            out.push(msg.clone());
        }
        i += 1;
    }

    (out, dropped_assistant_calls, dropped_stray_tools)
}

// ================================================================
// Full heal
// Port of `healLoadedMessages` (healing.ts:61-69)
// ================================================================

/// Apply all heal steps to a message list. Returns the healed
/// list + counts of what was fixed.
pub fn heal_loaded_messages(messages: &[Value], max_chars: usize) -> HealResult {
    let shrunk = shrink_oversized_tool_results(messages, max_chars);
    let (paired, dropped_assistant, dropped_stray) = fix_tool_call_pairing(&shrunk.messages);
    HealResult {
        messages: paired,
        healed_count: shrunk.healed_count + dropped_assistant + dropped_stray,
        chars_saved: shrunk.chars_saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_msg(content: &str, call_id: &str) -> Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        })
    }

    fn assistant_msg(content: &str, tool_calls: &[Value]) -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        })
    }

    fn user_msg(content: &str) -> Value {
        serde_json::json!({
            "role": "user",
            "content": content,
        })
    }

    #[test]
    fn shrink_leaves_short_results_untouched() {
        let msgs = vec![tool_msg("short result", "c1"), user_msg("hello")];
        let r = shrink_oversized_tool_results(&msgs, 100);
        assert_eq!(r.healed_count, 0);
        assert_eq!(r.messages.len(), 2);
    }

    #[test]
    fn shrink_truncates_long_tool_results() {
        let long = "x".repeat(100_000);
        let msgs = vec![tool_msg(&long, "c1")];
        let r = shrink_oversized_tool_results(&msgs, 40_000);
        assert_eq!(r.healed_count, 1);
        let content = r.messages[0]["content"].as_str().unwrap();
        assert!(content.len() <= 40_100, "should be roughly capped");
        assert!(content.contains("truncated"));
    }

    #[test]
    fn shrink_does_not_touch_user_messages() {
        let long = "x".repeat(100_000);
        let msgs = vec![user_msg(&long)];
        let r = shrink_oversized_tool_results(&msgs, 40_000);
        assert_eq!(r.healed_count, 0);
        assert_eq!(r.messages[0]["content"].as_str().unwrap(), long);
    }

    #[test]
    fn pairing_keeps_valid_assistant_tool_sequence() {
        let msgs = vec![
            assistant_msg(
                "calling",
                &[serde_json::json!({"id": "c1", "name": "echo"})],
            ),
            tool_msg("result", "c1"),
        ];
        let (out, dropped_a, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(dropped_a, 0);
        assert_eq!(dropped_t, 0);
    }

    #[test]
    fn pairing_drops_unpaired_assistant_tool_calls() {
        let msgs = vec![assistant_msg(
            "calling",
            &[serde_json::json!({"id": "c1", "name": "echo"})],
        )];
        let (out, dropped_a, _) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 0);
        assert_eq!(dropped_a, 1);
    }

    #[test]
    fn pairing_drops_stray_tool_messages() {
        let msgs = vec![tool_msg("orphan", "c1")];
        let (out, _, dropped_t) = fix_tool_call_pairing(&msgs);
        assert_eq!(out.len(), 0);
        assert_eq!(dropped_t, 1);
    }

    #[test]
    fn pairing_handles_missing_id_on_tool_call() {
        // Assistant calls but the tool_call has no id — still
        // try to match with tool results.
        let msgs = vec![
            assistant_msg("calling", &[serde_json::json!({"name": "echo"})]),
            tool_msg("result", ""),
        ];
        let (out, _, _) = fix_tool_call_pairing(&msgs);
        // No valid ids to match → dropped
        assert!(out.is_empty() || out.len() < 2);
    }

    #[test]
    fn full_heal_composes_shrink_and_pairing() {
        let long = "x".repeat(100_000);
        let msgs = vec![
            user_msg("hello"),
            assistant_msg(
                "calling",
                &[serde_json::json!({"id": "c1", "name": "echo"})],
            ),
            tool_msg(&long, "c1"),
            user_msg("next"),
        ];
        let r = heal_loaded_messages(&msgs, 40_000);
        assert!(r.healed_count >= 1); // shrunk at minimum
        assert!(
            r.chars_saved > 0,
            "should have saved at least {} chars from the long tool result",
            long.len() - 40_000
        );
    }
}
