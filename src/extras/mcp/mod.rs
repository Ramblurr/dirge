pub mod client;
pub mod config;
pub mod tool;

use std::collections::HashMap;

use tool::McpTool;

use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub struct McpClientManager {
    pub handles: Vec<client::McpClientHandle>,
    /// Original configs retained so a disconnected server can be
    /// reconnected later via [`reconnect`]. Without this, a
    /// transport that dies mid-session was dead for the rest of the
    /// session — there was nowhere to look up the original
    /// command/args/env to respawn (audit H15). Auto-retry-on-tool-
    /// failure is deferred (requires sharing a swappable Peer across
    /// already-handed-out McpTool instances); this enables manual
    /// recovery in the meantime.
    configs: HashMap<String, config::McpServerConfig>,
}

impl McpClientManager {
    pub async fn connect_all(configs: &HashMap<String, config::McpServerConfig>) -> Self {
        let mut handles = Vec::new();
        for (name, cfg) in configs {
            match client::McpClientHandle::connect(name.clone(), cfg).await {
                Ok(handle) => {
                    tracing::info!("Connected to MCP server '{}'", name);
                    handles.push(handle);
                }
                Err(e) => {
                    // ALSO emit to stderr so users running without
                    // RUST_LOG / --verbose see that an MCP server
                    // failed to register. Without this, configured
                    // tools just silently never appear and the user
                    // has no idea why.
                    tracing::warn!("Failed to connect to MCP server '{}': {e}", name);
                    eprintln!(
                        "warning: MCP server '{}' failed to connect: {}; its tools won't be available this session",
                        name, e,
                    );
                }
            }
        }
        Self {
            handles,
            configs: configs.clone(),
        }
    }

    /// Reconnect a single MCP server by name using its original
    /// config. Updates the shared peer ref in place so existing
    /// McpTool clones from that server pick up the new transport
    /// transparently — no need to rebuild the tool registry.
    /// Returns Err if the server isn't in the manager's config map
    /// or the fresh connect attempt fails.
    ///
    /// Wired by `/mcp reconnect <name>` (UI slash) for the manual
    /// case. McpTool also calls this implicitly on transport-class
    /// failures (audit dirge-dvi auto-reconnect).
    #[allow(dead_code)]
    pub async fn reconnect(&mut self, name: &str) -> anyhow::Result<()> {
        let cfg = self.configs.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!("no config for MCP server '{name}' — was it registered at startup?")
        })?;
        // Spawn the new connection BEFORE dropping the old handle so
        // a connection failure leaves the old (dead) handle in place
        // rather than orphaning the slot entirely.
        let new_handle = client::McpClientHandle::connect(name.to_string(), &cfg)
            .await
            .map_err(|e| anyhow::anyhow!("reconnect to '{name}' failed: {e}"))?;
        let new_peer = new_handle.shared_peer().read().await.clone();

        // Find the existing handle (if any) and update its shared
        // peer in place so previously-handed-out McpTool clones see
        // the new transport. Then replace the RunningService so the
        // old child process is dropped.
        if let Some(existing) = self.handles.iter().find(|h| h.server_name == name) {
            existing.replace_peer(new_peer).await;
        }
        self.handles.retain(|h| h.server_name != name);
        self.handles.push(new_handle);
        Ok(())
    }

    pub async fn collect_tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<McpTool> {
        let mut all_tools = Vec::new();
        for handle in &self.handles {
            let peer = handle.shared_peer();
            let server_name = handle.server_name.clone();
            let cfg = self
                .configs
                .get(&server_name)
                .cloned()
                .map(std::sync::Arc::new);
            // Per-server reconnect lock — serializes self-reconnect
            // attempts when multiple tool calls fail concurrently.
            // Cloned across all McpTools for the same server.
            let reconnect_lock = std::sync::Arc::new(tokio::sync::Mutex::new(0u64));
            match handle.list_tools().await {
                Ok(tools) => {
                    for definition in tools {
                        all_tools.push(McpTool {
                            server_name: server_name.clone(),
                            definition,
                            peer: peer.clone(),
                            config: cfg.clone(),
                            reconnect_lock: reconnect_lock.clone(),
                            permission: permission.clone(),
                            ask_tx: ask_tx.clone(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to list tools from MCP server '{}': {e}",
                        server_name,
                    );
                    eprintln!(
                        "warning: MCP server '{}' connected but list_tools failed: {}; \
                         its tools won't be available this session",
                        server_name, e,
                    );
                }
            }
        }
        all_tools
    }

    pub async fn shutdown(self) {
        for handle in self.handles {
            let name = handle.server_name.clone();
            drop(handle);
            tracing::debug!("Disconnected from MCP server '{}'", name);
        }
    }
}
