//! /toggle handler.

#[allow(unused_imports)]
use crate::sync_util::LockExt;

use crate::ui::slash::cmd::agent;
use crate::ui::slash::{SlashCtx, c_agent, c_error, c_result};

pub(crate) async fn cmd_toggle(ctx: &mut SlashCtx<'_>, parts: &[&str]) -> anyhow::Result<()> {
    if parts.len() < 2 {
        ctx.renderer
            .write_line("usage: /toggle <feature> [on|off]", c_agent())?;
        ctx.renderer.write_line("features:", c_agent())?;
        ctx.renderer.write_line(
            &format!(
                "  todo  {}",
                if *ctx.todo_tools_enabled { "on" } else { "off" }
            ),
            c_result(),
        )?;
    } else {
        let new_state = match parts.get(2).copied() {
            Some("on") => true,
            Some("off") => false,
            Some(other) => {
                ctx.renderer
                    .write_line(&format!("invalid: '{}', use on or off", other), c_error())?;
                return Ok(());
            }
            None => !*ctx.todo_tools_enabled,
        };
        if new_state == *ctx.todo_tools_enabled {
            ctx.renderer.write_line(
                &format!(
                    "todo tools already {}",
                    if new_state { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        } else {
            *ctx.todo_tools_enabled = new_state;
            agent::rebuild_agent(ctx).await;
            ctx.renderer.write_line(
                &format!(
                    "todo tools: {}",
                    if *ctx.todo_tools_enabled { "on" } else { "off" }
                ),
                c_agent(),
            )?;
        }
    }
    Ok(())
}
