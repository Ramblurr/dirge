//! Scavenge tool calls from reasoning content.
//!
//! Faithful port of `DeepSeek-Reasonix/src/repair/scavenge.ts` (201 lines).
//!
//! DeepSeek R1 sometimes emits tool-call JSON inside reasoning_content
//! and forgets to include it in the structured `tool_calls` field.
//! This module recovers those calls from the reasoning text.
//!
//! Three patterns are recognized:
//!
//! 1. DSML invoke blocks: `<｜DSML｜invoke name="tool_name">...</>`
//! 2. Raw JSON objects matching:
//!    - `{name, arguments}` (simplest form)
//!    - `{type: "function", function: {name, arguments}}` (OpenAI-style)
//!    - `{tool_name, tool_args}` (R1 free-form variant)
//!
//! Only tools whose name appears in the allowed set are returned.
//! A max-calls cap defends against runaway extraction.
//! Inputs over 100KB are skipped (defense against regex O(n²)).

use crate::agent::agent_loop::tools::ToolCall;

/// Maximum input size before we skip scavenging.
/// Port of `MAX_SCAVENGE_INPUT` (scavenge.ts:18).
const MAX_SCAVENGE_INPUT: usize = 100 * 1024;

/// Result of a scavenge pass.
#[derive(Debug, Clone)]
pub struct ScavengeResult {
    pub calls: Vec<ToolCall>,
    pub notes: Vec<String>,
}

/// Scan reasoning content for tool calls the model forgot to emit.
/// Port of `scavengeToolCalls` (scavenge.ts:20-65).
pub fn scavenge_tool_calls(
    reasoning_content: Option<&str>,
    allowed_names: &std::collections::HashSet<String>,
    max_calls: usize,
) -> ScavengeResult {
    let content = match reasoning_content {
        Some(c) if !c.is_empty() => c,
        _ => {
            return ScavengeResult {
                calls: vec![],
                notes: vec![],
            };
        }
    };

    if content.len() > MAX_SCAVENGE_INPUT {
        return ScavengeResult {
            calls: vec![],
            notes: vec![format!(
                "scavenge skipped: reasoning_content too large ({} chars)",
                content.len()
            )],
        };
    }

    let max = if max_calls == 0 { 4 } else { max_calls };
    let mut notes: Vec<String> = Vec::new();
    let mut out: Vec<ToolCall> = Vec::new();

    // Pattern A: DSML invoke blocks.
    for invoke in iterate_dsml_invokes(content) {
        if out.len() >= max {
            break;
        }
        if !allowed_names.contains(&invoke.name) {
            continue;
        }
        out.push(ToolCall {
            id: String::new(),
            name: invoke.name.clone(),
            arguments: invoke.args,
        });
        notes.push(format!("scavenged DSML call: {}", invoke.name));
    }

    // Pattern B: raw JSON objects.
    let non_dsml = strip_dsml_blocks(content);
    for candidate in iterate_json_objects(&non_dsml) {
        if out.len() >= max {
            break;
        }
        if let Some(call) = coerce_to_tool_call(&candidate, allowed_names) {
            notes.push(format!("scavenged call: {}", call.name));
            out.push(call);
        }
    }

    ScavengeResult { calls: out, notes }
}

// ---- internal helpers ----

struct DsmlInvoke {
    name: String,
    args: serde_json::Value,
}

/// Strip DSML blocks so the raw-JSON scanner doesn't re-scavenge
/// parameter payloads. Port of `stripDsmlBlocks` (scavenge.ts:73-78).
fn strip_dsml_blocks(text: &str) -> String {
    use regex::Regex;
    // Match the full-width pipe (U+FF5C) or ASCII pipe.
    let re_func =
        Regex::new(r"<[｜|]DSML[｜|]function_calls>[\s\S]*?</?[｜|]DSML[｜|]function_calls>")
            .expect("DSML function_calls regex must compile");
    let re_invoke = Regex::new(r"<[｜|]DSML[｜|]invoke\s+[^>]*>[\s\S]*?</[｜|]DSML[｜|]invoke>")
        .expect("DSML invoke regex must compile");

    let out = re_func.replace_all(text, "");
    re_invoke.replace_all(&out, "").to_string()
}

/// Yield every DSML invoke block found in text.
/// Port of `iterateDsmlInvokes` (scavenge.ts:80-90).
fn iterate_dsml_invokes(text: &str) -> Vec<DsmlInvoke> {
    use regex::Regex;
    let re =
        Regex::new(r#"<[｜|]DSML[｜|]invoke\s+name="([^"]+)">([\s\S]*?)</[｜|]DSML[｜|]invoke>"#)
            .expect("DSML invoke regex must compile");

    let mut out = Vec::new();
    for caps in re.captures_iter(text) {
        let name = match caps.get(1) {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };
        let body = match caps.get(2) {
            Some(m) => m.as_str(),
            None => continue,
        };
        out.push(DsmlInvoke {
            name,
            args: parse_dsml_parameters(body),
        });
    }
    out
}

/// Parse DSML parameter blocks into a JSON Value.
/// Port of `parseDsmlParameters` (scavenge.ts:92-113).
/// Falls back to literal text when `string="false"` JSON parse fails.
fn parse_dsml_parameters(body: &str) -> serde_json::Value {
    use regex::Regex;
    let re = Regex::new(
        r#"<[｜|]DSML[｜|]parameter\s+name="([^"]+)"(?:\s+string="(true|false)")?\s*>([\s\S]*?)</[｜|]DSML[｜|]parameter>"#
    ).expect("DSML parameter regex must compile");

    let mut map = serde_json::Map::new();
    for caps in re.captures_iter(body) {
        let key = match caps.get(1) {
            Some(m) => m.as_str().to_string(),
            None => continue,
        };
        if key.is_empty() {
            continue;
        }
        let string_flag = caps.get(2).map(|m| m.as_str());
        let raw = caps
            .get(3)
            .map(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if string_flag == Some("false") {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) => {
                    map.insert(key, v);
                    continue;
                }
                Err(_) => {
                    // Fall through — keep as literal string.
                }
            }
        }
        map.insert(key, serde_json::Value::String(raw));
    }
    serde_json::Value::Object(map)
}

/// Yield every top-level JSON object substring in text.
/// Port of `iterateJsonObjects` (scavenge.ts:116-148).
fn iterate_json_objects(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escaped = false;

        for j in i..chars.len() {
            let c = chars[j];
            if escaped {
                escaped = false;
                continue;
            }
            if in_string {
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '"' {
                    in_string = false;
                }
                continue;
            }
            if c == '"' {
                in_string = true;
            } else if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    let candidate: String = chars[i..=j].iter().collect();
                    out.push(candidate);
                    i = j;
                    break;
                }
            }
        }
        i += 1;
    }
    out
}

/// Try to coerce a JSON string into a ToolCall.
/// Port of `coerceToToolCall` (scavenge.ts:150-201).
#[allow(clippy::collapsible_if)]
fn coerce_to_tool_call(
    candidate_json: &str,
    allowed_names: &std::collections::HashSet<String>,
) -> Option<ToolCall> {
    let parsed: serde_json::Value = serde_json::from_str(candidate_json).ok()?;
    let obj = parsed.as_object()?;

    // Pattern 1: { name, arguments }
    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        if allowed_names.contains(name) {
            let args = obj
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let args_val = if args.is_string() {
                serde_json::from_str::<serde_json::Value>(args.as_str().unwrap_or("{}"))
                    .unwrap_or(serde_json::json!({}))
            } else {
                args
            };
            return Some(ToolCall {
                id: String::new(),
                name: name.to_string(),
                arguments: args_val,
            });
        }
    }

    // Pattern 2: OpenAI-style { type: "function", function: { name, arguments } }
    if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
        if let Some(func) = obj.get("function").and_then(|v| v.as_object()) {
            if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                if allowed_names.contains(name) {
                    let args = func
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    let args_val = if args.is_string() {
                        serde_json::from_str::<serde_json::Value>(args.as_str().unwrap_or("{}"))
                            .unwrap_or(serde_json::json!({}))
                    } else {
                        args
                    };
                    return Some(ToolCall {
                        id: String::new(),
                        name: name.to_string(),
                        arguments: args_val,
                    });
                }
            }
        }
    }

    // Pattern 3: { tool_name, tool_args } (R1 free-form variant)
    if let Some(name) = obj.get("tool_name").and_then(|v| v.as_str()) {
        if allowed_names.contains(name) {
            let args = obj
                .get("tool_args")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            return Some(ToolCall {
                id: String::new(),
                name: name.to_string(),
                arguments: args,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn allowed() -> HashSet<String> {
        ["get_weather", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn dsml_allowed() -> HashSet<String> {
        ["filesystem_edit_file", "search"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn returns_nothing_for_empty_reasoning() {
        let r = scavenge_tool_calls(None, &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn returns_nothing_for_null_reasoning() {
        let r = scavenge_tool_calls(Some(""), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn extracts_pattern_1_name_arguments() {
        let reasoning =
            r#"thinking... I should call {"name": "get_weather", "arguments": {"city": "SF"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "get_weather");
        assert_eq!(r.calls[0].arguments["city"], "SF");
    }

    #[test]
    fn extracts_openai_style_envelope() {
        let reasoning = r#"plan: {"type":"function","function":{"name":"search","arguments":"{\"q\":\"ts\"}"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "search");
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn extracts_tool_name_tool_args_variant() {
        let reasoning = r#"decide: {"tool_name": "search", "tool_args": {"q": "deepseek"}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].name, "search");
        assert_eq!(r.calls[0].arguments["q"], "deepseek");
    }

    #[test]
    fn ignores_tools_not_in_allowed_set() {
        let reasoning = r#"{"name": "rm_rf_slash", "arguments": {}}"#;
        let r = scavenge_tool_calls(Some(reasoning), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn respects_max_calls() {
        let reasoning: String = (0..6)
            .map(|_| r#"{"name": "search", "arguments": {"q": "x"}}"#)
            .collect::<Vec<_>>()
            .join(" then ");
        let r = scavenge_tool_calls(Some(&reasoning), &allowed(), 2);
        assert_eq!(r.calls.len(), 2);
    }

    #[test]
    fn extracts_dsml_invoke_block_with_params() {
        let input = [
            "Let me make the edit.",
            "",
            "<｜DSML｜function_calls> <｜DSML｜invoke name=\"filesystem_edit_file\">",
            "  <｜DSML｜parameter name=\"path\" string=\"true\">F:/x.html</｜DSML｜parameter>",
            "  <｜DSML｜parameter name=\"edits\" string=\"false\">[{\"oldText\":\"a\",\"newText\":\"b\"}]</｜DSML｜parameter>",
            "</｜DSML｜invoke> </｜DSML｜function_calls>",
        ].join("\n");
        let r = scavenge_tool_calls(Some(&input), &dsml_allowed(), 4);
        assert_eq!(r.calls.len(), 1);
        let call = &r.calls[0];
        assert_eq!(call.name, "filesystem_edit_file");
        assert_eq!(call.arguments["path"], "F:/x.html");
        assert_eq!(
            call.arguments["edits"],
            serde_json::json!([{"oldText": "a", "newText": "b"}])
        );
        assert!(r.notes.iter().any(|n| n.contains("DSML")));
    }

    #[test]
    fn accepts_ascii_pipe_dsml_variant() {
        let dsml_search: HashSet<String> = ["search"].iter().map(|s| s.to_string()).collect();
        let input = "<|DSML|invoke name=\"search\"><|DSML|parameter name=\"q\" string=\"true\">ts</|DSML|parameter></|DSML|invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_search, 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "ts");
    }

    #[test]
    fn dsml_call_with_unknown_tool_is_skipped() {
        let input = "<｜DSML｜invoke name=\"rm_rf_slash\"><｜DSML｜parameter name=\"x\" string=\"true\">y</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &allowed(), 4);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn dsml_string_false_malformed_json_falls_back_to_literal() {
        let dsml_search: HashSet<String> = ["search"].iter().map(|s| s.to_string()).collect();
        let input = "<｜DSML｜invoke name=\"search\"><｜DSML｜parameter name=\"q\" string=\"false\">not valid json</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_search, 4);
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].arguments["q"], "not valid json");
    }

    #[test]
    fn does_not_double_count_json_inside_dsml_block() {
        // Inner JSON is a param value — should not become a separate call
        let input = "<｜DSML｜invoke name=\"filesystem_edit_file\"><｜DSML｜parameter name=\"edits\" string=\"false\">{\"name\": \"filesystem_edit_file\", \"arguments\": {}}</｜DSML｜parameter></｜DSML｜invoke>";
        let r = scavenge_tool_calls(Some(input), &dsml_allowed(), 4);
        assert_eq!(
            r.calls.len(),
            1,
            "should be exactly one call — DSML wrapper, not inner JSON"
        );
    }

    #[test]
    fn skips_large_inputs() {
        let large = "x".repeat(MAX_SCAVENGE_INPUT + 1);
        let r = scavenge_tool_calls(Some(&large), &allowed(), 4);
        assert!(r.calls.is_empty());
        assert!(r.notes.iter().any(|n| n.contains("too large")));
    }
}
