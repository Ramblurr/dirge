//! /agent command dispatch and shared helpers.

pub(crate) mod clear;
pub(crate) mod list;
pub(crate) mod switch;

use crate::ui::slash::{SlashCtx, rebuild_agent_parts};

/// Rebuild the agent from the current session model and context.
/// Shared by agent activation/deactivation and /regen-prompts.
pub(crate) async fn rebuild_agent(ctx: &mut SlashCtx<'_>) {
    rebuild_agent_parts(
        ctx.agent,
        ctx.client,
        ctx.session,
        ctx.cli,
        ctx.cfg,
        ctx.context,
        ctx.permission,
        ctx.ask_tx,
        ctx.question_tx,
        ctx.plan_tx,
        ctx.bg_store,
        ctx.sandbox,
        #[cfg(feature = "mcp")]
        ctx.mcp_manager,
        #[cfg(feature = "semantic")]
        ctx.semantic_manager,
        #[cfg(feature = "lsp")]
        ctx.lsp_manager,
    )
    .await;
}

pub(crate) async fn cmd_agent(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 || parts[0] == "/agents" {
        return list::cmd_agent_list(ctx, parts).await;
    }
    let arg = parts[1].trim();
    if matches!(arg, "off" | "none" | "default") {
        return clear::cmd_agent_clear(ctx).await;
    }
    switch::cmd_agent_switch(ctx, arg).await
}
