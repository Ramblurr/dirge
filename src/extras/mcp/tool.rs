use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use rmcp::ServiceError;
use rmcp::model::{CallToolRequestParams, JsonObject, RawContent};
use tokio::sync::Mutex;

use crate::agent::tools::check_perm;
use crate::extras::mcp::client::{SharedConnection, raw_connect};
use crate::extras::mcp::config::McpServerConfig;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

#[derive(Debug)]
pub struct McpToolError(pub String);

impl fmt::Display for McpToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for McpToolError {}

pub struct McpTool {
    pub server_name: String,
    pub definition: rmcp::model::Tool,
    /// Shared connection — peer + running_service co-owned with the
    /// manager and every other McpTool from this server. M-R1 review
    /// fix: previously each tool held a bare `Peer<RoleClient>` clone
    /// plus a separately leaked `RunningService`; the new shape keeps
    /// the running_service alive THROUGH the swap so reconnects
    /// don't leak the spawned child process.
    pub connection: Arc<SharedConnection>,
    /// Server config retained so a transport-class failure can
    /// trigger a self-reconnect without going through the manager.
    /// `None` for tools constructed by callers that don't supply
    /// the config (e.g. tests); auto-reconnect is skipped in that
    /// case and a clear error surfaces instead.
    pub config: Option<Arc<McpServerConfig>>,
    /// Per-server lock + generation counter. Multiple in-flight tool
    /// calls failing concurrently all wait on this; the gen lets the
    /// first reconnect mark the swap done so later callers re-read
    /// the peer without redundant reconnects. M-R2 review fix:
    /// constructed once per server at manager startup, not per
    /// `collect_tools` call, so the gen counter is canonical for
    /// the entire process lifetime.
    pub reconnect_lock: Arc<Mutex<u64>>,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

/// Classify a [`ServiceError`] as transport-class (worth reconnecting)
/// versus everything else (surface as-is). M-R5 review tightening:
/// narrowed from the original aggressive set. Only the two unambiguous
/// transport-failure variants reconnect:
///
/// - `TransportSend` — the underlying writer failed.
/// - `TransportClosed` — the receiver task on our side observed EOF.
///
/// `UnexpectedResponse` (protocol mismatch — server is alive but
/// buggy), `Timeout` (a slow tool legitimately running long), and
/// `Cancelled` (user-driven abort) intentionally fall through to the
/// surface-as-is path. Reconnecting on those would mask real bugs or
/// tear down healthy connections mid-run.
fn is_transport_failure(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportSend(_) | ServiceError::TransportClosed
    )
}

/// MCP tool permission trust model — read before assuming an MCP
/// tool obeys the same rules as a built-in:
///
/// All MCP tool calls route through `check_perm` with the umbrella
/// tool name `"mcp_tool"` and a perm key shaped
/// `mcp_tool:<server>:<name>`. They do NOT alias to dirge built-ins
/// — an MCP server exporting `edit_file` / `write` / `bash` is
/// gated by `mcp_tool` rules, NOT by the user's `edit:` / `write:` /
/// `bash:` rules.
///
/// Concretely, if the user configures:
///
///   "permission": {
///     "edit":     { "/etc/**": "deny" },
///     "mcp_tool": { "*":       "allow" }
///   }
///
/// …a built-in `edit` of `/etc/passwd` is denied, but an MCP-exported
/// `edit_file` call against `/etc/passwd` runs unprompted. To gate
/// MCP-exported edits, pin the qualified form:
///
///   "permission": {
///     "mcp_tool": {
///       "mcp_tool:fs:edit_file": "ask"
///     }
///   }
///
/// Prompt frontmatter `deny_tools` IS cross-checked against the
/// concrete MCP tool name (PERM-7 — handled inside
/// `PermissionChecker::check` plus the explicit `any_prompt_denied`
/// probe below), so plan-mode `deny_tools: [edit]` does block an
/// MCP-exported `edit`. Built-in tool *rule tables* don't alias.
impl ToolDyn for McpTool {
    fn name(&self) -> String {
        self.definition.name.to_string()
    }

    fn definition(&self, _prompt: String) -> WasmBoxedFuture<'_, ToolDefinition> {
        let name = self.definition.name.to_string();
        let description = self
            .definition
            .description
            .clone()
            .unwrap_or(Cow::from(""))
            .to_string();
        // MCP servers that don't ship an `inputSchema` would
        // serialize as `null`, which violates rig's expectation of
        // an object. Substitute an empty object so the tool stays
        // usable (the LLM just won't have a hint that args are
        // expected, but it can still call the tool with no params).
        let parameters = serde_json::to_value(&self.definition.input_schema)
            .ok()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| serde_json::json!({}));
        Box::pin(async move {
            ToolDefinition {
                name,
                description,
                parameters,
            }
        })
    }

    fn call(&self, args: String) -> WasmBoxedFuture<'_, Result<String, ToolError>> {
        let server_name = self.server_name.clone();
        let tool_name = self.definition.name.to_string();
        let connection = Arc::clone(&self.connection);
        let config = self.config.clone();
        let reconnect_lock = self.reconnect_lock.clone();
        let permission = self.permission.clone();
        let ask_tx = self.ask_tx.clone();

        Box::pin(async move {
            // Adversarial-review finding #1: MCP tools pass the
            // umbrella name `"mcp_tool"` to `check_perm`, which
            // means a prompt's `deny_tools: [edit]` would NOT match
            // an MCP server's `edit` tool — the literal string
            // comparison inside `is_prompt_denied` never sees the
            // concrete name. Probe explicitly for the concrete
            // name, the qualified `mcp_tool:server:name` form, and
            // the umbrella `mcp_tool`; any match denies before the
            // call leaves dirge.
            if let Some(perm) = permission.as_ref() {
                let qualified = format!("mcp_tool:{}:{}", server_name, tool_name);
                let denied = {
                    let guard = perm.lock().unwrap_or_else(|e| e.into_inner());
                    guard.any_prompt_denied(&[tool_name.as_str(), qualified.as_str(), "mcp_tool"])
                };
                if denied {
                    return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                        "MCP tool {}::{} is denied by the active prompt's `deny_tools` frontmatter. Switch with `/prompt <other>` to use it.",
                        server_name, tool_name,
                    )))));
                }
            }
            let perm_key = format!("mcp_tool:{server_name}:{tool_name}");
            check_perm(&permission, &ask_tx, "mcp_tool", &perm_key)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(McpToolError(e.to_string()))))?;

            // Malformed JSON used to silently default to `None` via
            // `unwrap_or_default()` — the MCP server got an empty
            // argument set and the agent saw a confusing "missing
            // required field" error from the server instead of a
            // dirge-side parse error. Surface the parse failure
            // distinctly so the agent can fix its tool call.
            //
            // Empty / whitespace-only args is treated as the explicit
            // no-arguments case (matches rig's default tool-call
            // shape when the LLM omits the arguments object).
            let trimmed = args.trim();
            let arguments: Option<JsonObject> = if trimmed.is_empty() {
                None
            } else {
                match serde_json::from_str::<JsonObject>(trimmed) {
                    Ok(obj) => Some(obj),
                    Err(e) => {
                        return Err(ToolError::ToolCallError(Box::new(McpToolError(format!(
                            "MCP tool {}::{}: malformed JSON arguments ({e}). Got: {trimmed:.200}",
                            server_name, tool_name,
                        )))));
                    }
                }
            };
            let params = arguments
                .map(|a| CallToolRequestParams::new(tool_name.clone()).with_arguments(a))
                .unwrap_or_else(|| CallToolRequestParams::new(tool_name.clone()));

            // MCP tool calls go over JSON-RPC to a spawned server
            // process. If the server hangs (deadlock, infinite
            // loop, lost stdin pipe), the await never resolves and
            // the agent turn stalls indefinitely. Cap at 120s to
            // match `bash`'s default timeout — anything longer is
            // clearly broken on the server side.
            //
            // The cap is a TOTAL budget for the whole try-reconnect-
            // retry cycle (M-R3 review fix), not per-attempt. Worst
            // case the user waits 120s for everything; previously the
            // budget was 240s = 2 × 120s.
            const MCP_CALL_BUDGET: Duration = Duration::from_secs(120);
            let started = Instant::now();

            let result = match try_call_with_reconnect(
                &server_name,
                &connection,
                config.as_deref(),
                &reconnect_lock,
                params,
                started,
                MCP_CALL_BUDGET,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Err(ToolError::ToolCallError(Box::new(McpToolError(e))));
                }
            };

            if result.is_error.unwrap_or(false) {
                let error_msg = result
                    .content
                    .iter()
                    .filter_map(|c| match &c.raw {
                        RawContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let msg = if error_msg.is_empty() {
                    "MCP tool returned an error".to_string()
                } else {
                    error_msg
                };
                return Err(ToolError::ToolCallError(Box::new(McpToolError(msg))));
            }

            // Cap aggregate MCP result at 256 KiB before it
            // reaches LLM context. A misbehaving MCP server
            // returning a 200 KB+ blob would otherwise flood
            // every subsequent turn until compaction. The cap
            // matches the bash output cap below; tools wanting
            // larger payloads should chunk or return resource
            // URIs.
            const MCP_RESULT_CAP_BYTES: usize = 256 * 1024;
            let mut content = String::new();
            let mut truncated = false;
            for item in result.content {
                if truncated {
                    break;
                }
                let chunk: String = match item.raw {
                    RawContent::Text(t) => t.text,
                    RawContent::Image(img) => {
                        format!("data:{};base64,{}", img.mime_type, img.data)
                    }
                    RawContent::Resource(r) => match r.resource {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                        rmcp::model::ResourceContents::BlobResourceContents { blob, .. } => blob,
                    },
                    _ => continue,
                };
                let remaining = MCP_RESULT_CAP_BYTES.saturating_sub(content.len());
                if chunk.len() <= remaining {
                    content.push_str(&chunk);
                } else {
                    // Find a UTF-8 char boundary at or below
                    // `remaining` so we don't slice through a
                    // multi-byte codepoint.
                    let mut cut = remaining;
                    while cut > 0 && !chunk.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    content.push_str(&chunk[..cut]);
                    truncated = true;
                }
            }
            if truncated {
                content.push_str(&format!(
                    "\n…[MCP result truncated at {} bytes — {}::{} returned more]",
                    MCP_RESULT_CAP_BYTES, server_name, tool_name,
                ));
            }
            Ok(content)
        })
    }
}

/// Try `peer.call_tool` once; on transport-class failure, swap the
/// shared connection for a freshly-reconnected one and retry exactly
/// once. Tool-level errors (server returned an error response) and
/// non-transport ServiceErrors surface verbatim — reconnecting
/// wouldn't help.
///
/// `started` + `total_budget` define the deadline for the WHOLE
/// try-reconnect-retry cycle (M-R3 fix). Each `call_once` invocation
/// receives whatever budget remains, so the worst-case latency
/// matches the prior single-attempt timeout.
///
/// The reconnect_lock + gen counter serializes concurrent callers
/// failing against the same dead transport: the first reconnects,
/// later callers see the bumped gen and skip the redundant work.
/// Config is required for the reconnect path; without it the
/// transport error surfaces immediately.
async fn try_call_with_reconnect(
    server_name: &str,
    connection: &Arc<SharedConnection>,
    config: Option<&McpServerConfig>,
    reconnect_lock: &Arc<Mutex<u64>>,
    params: CallToolRequestParams,
    started: Instant,
    total_budget: Duration,
) -> Result<rmcp::model::CallToolResult, String> {
    // Snapshot the generation BEFORE the first call so we can detect
    // after-the-fact reconnects below.
    let gen_before = *reconnect_lock.lock().await;

    let remaining = remaining_budget(started, total_budget);
    let first = call_once(server_name, connection, params.clone(), remaining).await;
    let err = match first {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };

    // Non-transport error → surface as-is.
    let Some(svc_err) = err.as_service_error() else {
        return Err(err.message);
    };
    if !is_transport_failure(svc_err) {
        return Err(err.message);
    }

    // Transport failure. Without config we can't reconnect.
    let Some(cfg) = config else {
        return Err(format!(
            "{}\n(auto-reconnect unavailable — no config retained for server '{}')",
            err.message, server_name,
        ));
    };

    // Lock and reconnect (or skip if another caller beat us).
    {
        let mut gen_guard = reconnect_lock.lock().await;
        if *gen_guard == gen_before {
            tracing::warn!(
                target: "dirge::mcp",
                server = %server_name,
                "transport failure detected — attempting auto-reconnect",
            );
            // Bound the reconnect at the remaining budget so a wedged
            // server doesn't burn the whole thing without leaving any
            // for the retry call.
            let reconnect_budget = remaining_budget(started, total_budget);
            let reconnect_result =
                tokio::time::timeout(reconnect_budget, raw_connect(server_name, cfg)).await;
            match reconnect_result {
                Ok(Ok((new_peer, new_rs))) => {
                    connection.replace(new_peer, new_rs).await;
                    *gen_guard += 1;
                    tracing::info!(
                        target: "dirge::mcp",
                        server = %server_name,
                        "MCP server reconnected after transport failure",
                    );
                }
                Ok(Err(e)) => {
                    return Err(format!(
                        "{}\n(auto-reconnect to '{}' also failed: {})",
                        err.message, server_name, e,
                    ));
                }
                Err(_) => {
                    return Err(format!(
                        "{}\n(auto-reconnect to '{}' timed out within the {}s budget)",
                        err.message,
                        server_name,
                        total_budget.as_secs(),
                    ));
                }
            }
        }
        // else: another caller already reconnected; just retry with
        // the (newer) peer.
    }

    // Second attempt with the fresh peer.
    let remaining = remaining_budget(started, total_budget);
    if remaining.is_zero() {
        return Err(format!(
            "MCP tool {}::{} budget ({}s) exhausted before retry",
            server_name,
            params.name,
            total_budget.as_secs(),
        ));
    }
    match call_once(server_name, connection, params, remaining).await {
        Ok(r) => Ok(r),
        Err(e) => Err(format!(
            "{}\n(reconnected but the retry also failed)",
            e.message,
        )),
    }
}

/// Time left in the budget. Returns `Duration::ZERO` (NOT a negative)
/// when the deadline has passed; `tokio::time::timeout(ZERO, _)` then
/// fires immediately and surfaces the budget-exhausted state.
fn remaining_budget(started: Instant, total: Duration) -> Duration {
    total.saturating_sub(started.elapsed())
}

/// Tagged error for `try_call_with_reconnect` — distinguishes
/// transport failures (worth retrying) from tool-level errors
/// (surface as-is).
struct CallErr {
    message: String,
    service_error: Option<ServiceError>,
}

impl CallErr {
    fn as_service_error(&self) -> Option<&ServiceError> {
        self.service_error.as_ref()
    }
}

async fn call_once(
    server_name: &str,
    connection: &Arc<SharedConnection>,
    params: CallToolRequestParams,
    timeout: Duration,
) -> Result<rmcp::model::CallToolResult, CallErr> {
    let tool_name = params.name.to_string();
    // Snapshot the current peer. Held briefly across the read-lock;
    // the actual call doesn't hold the lock so another caller can
    // swap the peer (manager-side or tool-side reconnect) without
    // blocking on us.
    let peer = connection.current_peer().await;
    match tokio::time::timeout(timeout, peer.call_tool(params)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(svc_err)) => {
            let msg = format!("MCP tool error ({server_name}::{tool_name}): {svc_err}");
            Err(CallErr {
                message: msg,
                service_error: Some(svc_err),
            })
        }
        Err(_) => Err(CallErr {
            message: format!(
                "MCP tool {server_name}::{tool_name} timed out after {}s",
                timeout.as_secs(),
            ),
            service_error: Some(ServiceError::Timeout { timeout }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classification matrix for `is_transport_failure`. M-R5 review
    /// tightening: ONLY the two unambiguous transport-failure
    /// variants reconnect. `UnexpectedResponse`, `Timeout`,
    /// `McpError`, `Cancelled` surface as-is — previously
    /// `UnexpectedResponse`+`Timeout` reconnected too, which would
    /// tear down healthy connections on a slow tool or a buggy
    /// server reply.
    #[test]
    fn is_transport_failure_classifies_correctly() {
        // Transport-class → reconnect
        assert!(is_transport_failure(&ServiceError::TransportClosed));

        // Non-transport → surface as-is.
        assert!(!is_transport_failure(&ServiceError::UnexpectedResponse));
        assert!(!is_transport_failure(&ServiceError::Timeout {
            timeout: Duration::from_secs(1),
        }));
        let mcp_err = rmcp::ErrorData::new(
            rmcp::model::ErrorCode::INTERNAL_ERROR,
            "the tool refused",
            None,
        );
        assert!(!is_transport_failure(&ServiceError::McpError(mcp_err)));
        assert!(!is_transport_failure(&ServiceError::Cancelled {
            reason: Some("user".into()),
        }));
    }

    /// `remaining_budget` decays as time passes and saturates at
    /// zero past the deadline (no negative durations / underflow).
    #[test]
    fn remaining_budget_decays_and_saturates() {
        let now = Instant::now();
        let total = Duration::from_millis(100);
        // Fresh start — full budget available.
        let r1 = remaining_budget(now, total);
        assert!(r1 > Duration::from_millis(90));
        // Past the deadline — saturates to ZERO, not negative.
        std::thread::sleep(Duration::from_millis(110));
        let r2 = remaining_budget(now, total);
        assert_eq!(r2, Duration::ZERO);
    }
}
