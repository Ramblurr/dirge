use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use rmcp::ServiceError;
use rmcp::model::{CallToolRequestParams, JsonObject, RawContent};
use tokio::sync::Mutex;

use crate::agent::tools::check_perm;
use crate::extras::mcp::client::{SharedPeer, fresh_peer_for};
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
    /// Shared, swappable peer ref — see `crate::extras::mcp::client::SharedPeer`.
    /// Updated by either the manager's manual reconnect or this
    /// tool's own auto-reconnect path on transport failure
    /// (audit dirge-dvi).
    pub peer: SharedPeer,
    /// Server config retained so a transport-class failure can
    /// trigger a self-reconnect without going through the manager.
    /// `None` for tools constructed by callers that don't supply
    /// the config (e.g. tests); auto-reconnect is skipped in that
    /// case and a clear error surfaces instead.
    pub config: Option<Arc<McpServerConfig>>,
    /// Per-server lock + generation counter. Multiple in-flight
    /// tool calls failing concurrently all wait on this; the
    /// generation lets the first reconnect mark the swap done so
    /// later callers re-read the peer without redundant
    /// reconnects. Cloned across all McpTools from the same
    /// server.
    pub reconnect_lock: Arc<Mutex<u64>>,
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
}

/// Decide whether a [`ServiceError`] looks like the transport died
/// (worth a reconnect) versus an application-level error from the
/// MCP server's tool implementation (surface to the LLM as-is).
/// `TransportSend` and `TransportClosed` are the canonical transport
/// failure variants; `UnexpectedResponse` and `Timeout` are also
/// good candidates since both typically indicate the server is
/// wedged. `McpError` is the actual tool-returned-an-error case —
/// reconnecting wouldn't help.
fn is_transport_failure(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportSend(_)
            | ServiceError::TransportClosed
            | ServiceError::UnexpectedResponse
            | ServiceError::Timeout { .. }
    )
}

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
        let peer_ref = self.peer.clone();
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
            // clearly broken on the server side. The error message
            // names the server + tool so the user can identify
            // which MCP server is misbehaving.
            const MCP_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

            // Auto-reconnect path (audit dirge-dvi): try once; on
            // transport-class failure, swap the shared peer and
            // retry once. Tool-application errors (`ServiceError::
            // McpError`) bypass the retry — the server is alive,
            // the tool just refused.
            let result = match try_call_with_reconnect(
                &server_name,
                &peer_ref,
                config.as_deref(),
                &reconnect_lock,
                params.clone(),
                MCP_CALL_TIMEOUT,
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
/// shared peer for a freshly-reconnected one and retry exactly
/// once. Tool-level errors (server returned an error response)
/// surface verbatim without retry — reconnecting wouldn't help.
///
/// The reconnect_lock + gen counter serializes concurrent callers
/// failing against the same dead transport: the first reconnects,
/// later callers see the bumped gen and skip the redundant work.
/// Config is required for the reconnect path; without it (caller
/// didn't supply one), the transport error surfaces immediately.
async fn try_call_with_reconnect(
    server_name: &str,
    peer_ref: &SharedPeer,
    config: Option<&McpServerConfig>,
    reconnect_lock: &Arc<Mutex<u64>>,
    params: CallToolRequestParams,
    timeout: std::time::Duration,
) -> Result<rmcp::model::CallToolResult, String> {
    // Snapshot the generation BEFORE the first call so we can
    // detect after-the-fact reconnects below.
    let gen_before = *reconnect_lock.lock().await;

    let first = call_once(server_name, peer_ref, params.clone(), timeout).await;
    let err = match first {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };

    // Non-transport error → surface as-is (server is alive but the
    // tool refused).
    let Some(svc_err) = err.as_service_error() else {
        return Err(err.message);
    };
    if !is_transport_failure(svc_err) {
        return Err(err.message);
    }

    // Transport failure. Without config we can't reconnect — and
    // without a retry, the user just sees the original error.
    // That's still better than the pre-fix behaviour where every
    // subsequent call would also fail until they restarted dirge.
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
            match fresh_peer_for(server_name, cfg).await {
                Ok((new_peer, _new_running_service)) => {
                    *peer_ref.write().await = new_peer;
                    *gen_guard += 1;
                    tracing::info!(
                        target: "dirge::mcp",
                        server = %server_name,
                        "MCP server reconnected after transport failure",
                    );
                    // NB: we drop `_new_running_service` after this
                    // scope. That looks like it'd kill the
                    // child immediately, but rmcp's RunningService
                    // keeps the spawned process alive only while
                    // SOMEONE holds the service. We need to retain
                    // it for the lifetime of the connection —
                    // otherwise the new peer points at a process
                    // we just terminated. Leak it intentionally so
                    // the connection stays up; the manager-side
                    // reconnect path replaces the manager's
                    // RunningService cleanly, but the tool-side
                    // auto-reconnect path doesn't have access to
                    // the manager. This leaks 1 RunningService per
                    // auto-reconnect — bounded by reconnect
                    // frequency. Acceptable for a P3 follow-up;
                    // a tighter design routes reconnects through
                    // the manager via a channel.
                    std::mem::forget(_new_running_service);
                }
                Err(e) => {
                    return Err(format!(
                        "{}\n(auto-reconnect to '{}' also failed: {})",
                        err.message, server_name, e,
                    ));
                }
            }
        }
        // else: another caller already reconnected; just retry with
        // the (newer) peer.
    }

    // Second attempt with the fresh peer.
    match call_once(server_name, peer_ref, params, timeout).await {
        Ok(r) => Ok(r),
        Err(e) => Err(format!(
            "{}\n(after auto-reconnect retry; original failure: transport-class)",
            e.message,
        )),
    }
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
    peer_ref: &SharedPeer,
    params: CallToolRequestParams,
    timeout: std::time::Duration,
) -> Result<rmcp::model::CallToolResult, CallErr> {
    let tool_name = params.name.to_string();
    // Take a short read-lock so the peer can be swapped between
    // calls but the call itself doesn't hold the lock.
    let peer = peer_ref.read().await.clone();
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

    /// Classification matrix for `is_transport_failure`. Locks in
    /// the audit dirge-dvi contract: only transport-class errors
    /// trigger auto-reconnect; tool-application errors surface
    /// directly to the agent.
    #[test]
    fn is_transport_failure_classifies_correctly() {
        // Transport-class → reconnect candidate
        assert!(is_transport_failure(&ServiceError::TransportClosed));
        assert!(is_transport_failure(&ServiceError::UnexpectedResponse));
        assert!(is_transport_failure(&ServiceError::Timeout {
            timeout: std::time::Duration::from_secs(1),
        }));

        // Tool-application error → NOT a reconnect candidate
        let mcp_err = rmcp::ErrorData::new(
            rmcp::model::ErrorCode::INTERNAL_ERROR,
            "the tool refused",
            None,
        );
        assert!(!is_transport_failure(&ServiceError::McpError(mcp_err)));

        // Cancelled → not a reconnect trigger (caller-driven, not transport)
        assert!(!is_transport_failure(&ServiceError::Cancelled {
            reason: Some("user".into()),
        }));
    }
}
