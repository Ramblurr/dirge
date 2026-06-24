//! Non-blocking `!cmd` shell execution (dirge-x9a3).
//!
//! A `!command` typed at the prompt runs a shell command (bounded by a 120s
//! cap). Awaiting it inline in the event loop froze rendering, input, and
//! Ctrl+C for the whole run — a `!npm install` or `!cargo build` hung the UI.
//! This module runs it on a spawned task; the `shell_phase` arm renders the
//! output when it lands. A `Visible` command then feeds its output to the agent
//! as a new turn (that continuation runs in the arm); an `Invisible` command
//! just prints.

use crate::sandbox::Sandbox;

/// Whether the command's output is fed to the agent as a new turn (`Visible`)
/// or merely printed (`Invisible`).
pub(crate) enum ShellKind {
    Visible,
    Invisible,
}

/// Handle to the spawned `!cmd` task: the result channel the loop drains, the
/// task (so Ctrl+C can `abort()` it), and the kind + command text the arm needs
/// to render and (for `Visible`) build the agent turn.
pub(crate) struct ShellPhaseHandle {
    pub rx: tokio::sync::mpsc::Receiver<Result<String, String>>,
    pub task: tokio::task::JoinHandle<()>,
    pub kind: ShellKind,
    pub cmd: String,
}

/// Spawn `cmd` off-thread. `sandbox` is cloned in (cheap) and moved to the task,
/// which sends the captured output (or a stringified error) back over a
/// capacity-1 channel.
pub(crate) fn spawn(cmd: String, kind: ShellKind, sandbox: Sandbox) -> ShellPhaseHandle {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, String>>(1);
    let cmd_run = cmd.clone();
    let task = tokio::spawn(async move {
        let result = crate::ui::shell_exec::run_shell_command(&cmd_run, &sandbox)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(result).await;
    });
    ShellPhaseHandle {
        rx,
        task,
        kind,
        cmd,
    }
}
