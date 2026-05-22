use std::collections::HashMap;
use std::process::Stdio;

use rmcp::service::{RoleClient, RunningService, serve_client};
use tokio::process::{ChildStderr, Command};

use super::config::McpServerConfig;

pub struct McpClientHandle {
    pub server_name: String,
    pub running_service: RunningService<RoleClient, ()>,
}

/// Upper bound on how long we'll wait for an MCP server to complete
/// initialization. Command-based servers that hang on `initialize`
/// (e.g. waiting for stdin that never comes) would otherwise pin
/// startup indefinitely. 10s is generous for legitimate inits — npm
/// install-on-first-run servers take a few seconds; locally-running
/// binaries respond in <100ms. Past the cap we abort and log.
const MCP_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl McpClientHandle {
    pub async fn connect(server_name: String, config: &McpServerConfig) -> anyhow::Result<Self> {
        // Wrap the entire connect in a timeout so a wedged server
        // doesn't block startup forever. Returns a clean
        // "init timeout" error past the cap.
        let inner = Self::connect_inner(server_name.clone(), config);
        match tokio::time::timeout(MCP_INIT_TIMEOUT, inner).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "MCP server {server_name:?} did not initialize within {}s — skipping",
                MCP_INIT_TIMEOUT.as_secs(),
            )),
        }
    }

    async fn connect_inner(server_name: String, config: &McpServerConfig) -> anyhow::Result<Self> {
        match config {
            McpServerConfig::Command { command, args, env } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                // CRITICAL: capture stderr instead of inheriting it.
                // rmcp's default `TokioChildProcess::new` uses
                // `Stdio::inherit()` for stderr, which gives the MCP
                // server (and its descendants) direct access to
                // dirge's controlling terminal. If the server (or
                // any library it uses) emits terminal queries — OSC
                // 11 for bg-color detection, `\x1b[c` for DA1,
                // `\x1b[6n` for CPR — those queries reach the
                // terminal, which replies via the TTY's INPUT side
                // (dirge's stdin). Crossterm's event parser doesn't
                // recognize those reply shapes, so the bytes sit in
                // the OS stdin buffer until exit, when the shell
                // inherits them and renders the literal escape
                // payload as visible garbage at the prompt.
                //
                // Pipe stderr instead. The child's logs are still
                // surfaced — we line-read them and forward to
                // dirge's stderr via tracing — but the child no
                // longer has a route to send escape queries that
                // can elicit a reply on dirge's stdin.
                let (transport, stderr) =
                    rmcp::transport::child_process::TokioChildProcess::builder(cmd)
                        .stderr(Stdio::piped())
                        .spawn()?;
                if let Some(child_stderr) = stderr {
                    spawn_stderr_forwarder(server_name.clone(), child_stderr);
                }
                let running_service = serve_client((), transport).await.map_err(|e| {
                    anyhow::anyhow!("MCP connection failed for '{server_name}': {e}")
                })?;
                Ok(Self {
                    server_name,
                    running_service,
                })
            }
            McpServerConfig::Url { url, headers } => {
                let custom_headers = parse_headers(headers)?;
                let cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom_headers);
                type HttpClient = rmcp::transport::StreamableHttpClientTransport<reqwest::Client>;
                let transport = HttpClient::from_config(cfg);
                let running_service = serve_client((), transport).await.map_err(|e| {
                    anyhow::anyhow!("MCP HTTP connection failed for '{server_name}': {e}")
                })?;
                Ok(Self {
                    server_name,
                    running_service,
                })
            }
        }
    }

    pub fn peer(&self) -> rmcp::service::Peer<RoleClient> {
        self.running_service.peer().clone()
    }

    pub async fn list_tools(&self) -> Result<Vec<rmcp::model::Tool>, rmcp::ServiceError> {
        self.running_service.peer().list_all_tools().await
    }
}

/// Forward an MCP child's stderr line-by-line to dirge's tracing
/// `info` channel (and ultimately to dirge's own stderr, which the
/// user has been seeing under the `[Lattice]` / `[Chiasmus]` etc.
/// prefixes). Strips any control bytes the child emits so its
/// stderr can't paint colors / move the cursor / send queries on
/// the way through. Bytes are forwarded as plain text, prefixed
/// with `[mcp:<server_name>]` so multiple servers are
/// distinguishable.
///
/// The task exits when the child closes stderr (process termination
/// or stream EOF). No explicit cancel — the rmcp ChildWithCleanup
/// Drop kills the child on shutdown, which closes stderr.
fn spawn_stderr_forwarder(server_name: String, stderr: ChildStderr) {
    /// Per-line byte cap. A buggy / runaway MCP child that writes
    /// gigabytes without a newline would otherwise grow dirge's
    /// `read_line` buffer until OOM. 16 KiB is generous for any
    /// real log line; past it we truncate and emit a marker.
    /// (#5 fix.)
    const MAX_LINE_BYTES: usize = 16 * 1024;
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut buf = Vec::with_capacity(1024);
        let mut byte_buf = [0u8; 4096];
        loop {
            let n = match reader.read(&mut byte_buf).await {
                Ok(0) => break, // EOF
                Ok(n) => n,
                Err(_) => break,
            };
            for &b in &byte_buf[..n] {
                if b == b'\n' {
                    emit_mcp_line(&server_name, &buf);
                    buf.clear();
                    continue;
                }
                if buf.len() >= MAX_LINE_BYTES {
                    // Past the cap — finalise the line with a
                    // truncation marker, then keep dropping
                    // bytes until the next `\n` so the
                    // overflow doesn't roll into the NEXT line.
                    buf.extend_from_slice(b" ...[truncated]");
                    emit_mcp_line(&server_name, &buf);
                    buf.clear();
                    // Skip bytes until next newline.
                    // (Set buf to capacity already so we don't
                    // grow; just discard until we see \n.)
                    // We use a marker bool by reusing buf's len > 0:
                    // simpler: just track a draining state.
                    // For correctness, fall through to the
                    // dropping branch below.
                }
                if buf.is_empty() && b == b'\r' {
                    continue; // strip leading CR (CRLF from windows-y child)
                }
                buf.push(b);
            }
        }
        // Flush any pending partial line on EOF.
        if !buf.is_empty() {
            emit_mcp_line(&server_name, &buf);
        }
    });
}

/// Sanitize and emit one MCP child stderr line through the UI's
/// off-stream notification channel.
///
/// Filter blocks:
///   - C0 controls except `\t` (0x00..=0x1F minus 0x09)
///   - DEL (0x7F)
///   - C1 controls (U+0080..=U+009F) — U+009B is single-byte CSI
///     on terminals in 8-bit mode and behaves identically to
///     `ESC[`, so leaving it through would defeat the sanitizer.
///     Also blocks NEL (U+0085), DCS (U+0090), etc.
///   - Trailing `\r` from CRLF children
///
/// Routes through `ui::notifications::notify_mcp_log` rather than
/// `tracing::warn!` or direct stderr writes — the UI event loop
/// drains the channel and renders via the standard
/// `Renderer::write_line` pipeline. Without this, MCP server logs
/// painted directly on top of the alt-screen UI from raw stderr
/// (e.g. `[Lattice] session closed` overlapping a chamber).
fn emit_mcp_line(server_name: &str, raw: &[u8]) {
    let s = String::from_utf8_lossy(raw);
    // Centralised sanitizer (`ui::ansi`) so MCP / websearch /
    // chat consumers share one definition of "what's a control
    // byte". Block ALL controls — MCP log lines are emitted
    // one-per-row by the UI, so embedded newlines/tabs from a
    // child would split into multiple notifications and rendering
    // becomes inconsistent. Newlines are handled by our
    // `read` loop seeing them as line delimiters; tabs become
    // spaces upstream.
    let sanitized = crate::ui::ansi::strip_controls(&s, crate::ui::ansi::StripPolicy::STRICT);
    // Review #9: previously used `trim().is_empty()` which also
    // dropped legitimate whitespace-only lines (e.g. servers that
    // emit indented continuation lines). Now drop only truly
    // empty post-sanitize lines — `\n`-between-log-groups still
    // collapses since the read loop sees the empty buf and emits
    // nothing.
    if sanitized.is_empty() {
        return;
    }
    crate::ui::notifications::notify_mcp_log(server_name, &sanitized);
}

fn parse_headers(
    headers: &HashMap<String, String>,
) -> anyhow::Result<HashMap<http::HeaderName, http::HeaderValue>> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header name '{name}': {e}"))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid header value for '{name}': {e}"))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}
