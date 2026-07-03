//! /retry handler.

use crate::session::MessageRole;
use crate::ui::events::render_session;
use crate::ui::slash::{SlashCtx, c_agent};

pub(crate) async fn cmd_retry(ctx: &mut SlashCtx<'_>) -> anyhow::Result<()> {
    let last_user = ctx
        .session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::User)
        .cloned();
    match last_user {
        Some(msg) => {
            let mut guard = ctx.session.messages.len();
            while let Some(last) = ctx.session.messages.last() {
                let was_user = last.role == MessageRole::User;
                ctx.session.pop_last_message();
                if was_user {
                    break;
                }
                guard = guard.saturating_sub(1);
                if guard == 0 {
                    break;
                }
            }
            // set_text (not raw buffer/cursor pokes) so the paste-table,
            // kill-ring, and history-draft resets fire — otherwise stale paste
            // placeholders from the prior draft linger after /retry (dirge-nyr7,
            // matches how /fork loads its text).
            ctx.input.set_text(&msg.content);
            render_session(ctx.renderer, ctx.session, ctx.cli, ctx.cfg, ctx.context)?;
            ctx.renderer
                .write_line("edit last message and press Enter to retry", c_agent())?;
        }
        None => {
            ctx.renderer
                .write_line("no previous message to retry", c_agent())?;
        }
    }
    Ok(())
}
