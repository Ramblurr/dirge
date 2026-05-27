use rig::completion::ToolDefinition;
use rig::tool::Tool;
use tokio::process::Command;
use tokio::time::Duration;

use crate::agent::tools::cache::ToolCache;
use crate::agent::tools::{AskSender, BashArgs, PermCheck, Scope, ToolError, enforce};

use crate::sandbox::Sandbox;
#[cfg(feature = "semantic-bash")]
use crate::semantic::adapters::bash;

/// Captured output with stdout + stderr lines preserved in arrival
/// order. Replaces tokio's `Output` (which collects each stream as
/// a separate blob, losing time ordering between them — F12).
#[derive(Debug)]
pub(crate) struct InterleavedOutput {
    /// Lines in the order they arrived from EITHER pipe.
    pub merged: String,
    pub exit_code: i32,
}

/// On Unix, SIGKILL the bash process group on drop. Used to clean up
/// grandchildren when the agent task is aborted (Ctrl+C) — tokio's
/// `kill_on_drop` only signals the immediate child, leaving descendants
/// orphaned. Disarmed via [`PgKillGuard::disarm`] on graceful paths
/// (successful completion, timeout — which already calls killpg itself)
/// so we don't double-signal.
#[cfg(unix)]
struct PgKillGuard {
    pid: u32,
    armed: bool,
}

#[cfg(unix)]
impl PgKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PgKillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: killpg with negative pid sends to the process
        // group. SIGKILL is the same on every POSIX platform;
        // libc::pid_t is i32 on every platform dirge supports. The
        // pid was set by us via `process_group(0)` so we know this
        // group exists and is bash + descendants.
        unsafe {
            let _ = libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

/// Spawn `cmd` into its own process group and wait for it,
/// capped at `secs`. On timeout, send SIGKILL to the process
/// group so the whole subprocess tree dies — not just bash. On
/// Windows we fall back to tokio's `kill_on_drop` which signals
/// the direct child only (Windows job objects would be cleaner
/// but require extra deps). F6 + F12 fix.
async fn run_with_timeout(cmd: Command, secs: u64) -> Result<InterleavedOutput, ToolError> {
    use std::process::Stdio;
    let mut cmd = cmd;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop(true)` ensures the immediate child gets a
    // signal when the tokio future is dropped — necessary for
    // ANY platform's timeout to actually clean up the bash process.
    cmd.kill_on_drop(true);

    #[cfg(unix)]
    {
        // process_group(0) makes the spawned child the leader of a
        // new process group with pgid = pid. Then `killpg(-pid)`
        // reaches every descendant. (tokio's `Command` exposes this
        // natively without needing the std `CommandExt` trait.)
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Msg(format!("failed to spawn: {}", e)))?;
    let pid = child.id();

    // Drop guard: on Unix, `kill_on_drop(true)` SIGKILLs the immediate
    // bash child when the future is dropped (e.g. user Ctrl+C aborts
    // the agent task) but leaves bash's *descendants* running as
    // grandchildren of pid 1. The timeout branch below already
    // handles this by calling `killpg(-pid, SIGKILL)`; the same is
    // needed for any other drop path. Holding a `PgKillGuard` for
    // the lifetime of the future does that.
    #[cfg(unix)]
    let _pgguard = pid.map(PgKillGuard::new);

    // F12: drain stdout + stderr concurrently into a single buffer
    // so the order of lines reflects actual arrival time. The prior
    // implementation (`wait_with_output`) buffered each stream
    // separately and concatenated stdout + stderr at the end, which
    // mis-ordered every command that wrote to both interleaved
    // (e.g. `make`, `npm install`, `cargo build`).
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain = async move {
        use tokio::io::AsyncBufReadExt;
        let mut merged = String::new();
        let mut so = stdout.map(tokio::io::BufReader::new);
        let mut se = stderr.map(tokio::io::BufReader::new);
        loop {
            // Decide presence BEFORE constructing futures — the
            // `if` guards on select! borrow `so` and `se`, which
            // would conflict with the futures' mutable borrows.
            let has_so = so.is_some();
            let has_se = se.is_some();
            if !has_so && !has_se {
                break;
            }
            let mut so_buf = String::new();
            let mut se_buf = String::new();
            // Build futures lazily; each is "noop" if its reader
            // is None. We funnel both into `Result<usize>` so the
            // select! arms have matching types.
            let so_fut = async {
                match so.as_mut() {
                    Some(r) => r.read_line(&mut so_buf).await.map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            let se_fut = async {
                match se.as_mut() {
                    Some(r) => r.read_line(&mut se_buf).await.map(Some),
                    None => Ok::<_, std::io::Error>(None),
                }
            };
            tokio::select! {
                biased;
                r = so_fut, if has_so => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { so = None; }
                    Ok(Some(_)) => merged.push_str(&so_buf),
                },
                r = se_fut, if has_se => match r {
                    Ok(Some(0)) | Ok(None) | Err(_) => { se = None; }
                    Ok(Some(_)) => merged.push_str(&se_buf),
                },
            }
        }
        merged
    };

    let wait = async {
        let merged = drain.await;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((merged, status))
    };

    let outcome = tokio::time::timeout(Duration::from_secs(secs), wait).await;
    match outcome {
        Ok(Ok((merged, status))) => {
            // Graceful completion — process group is already gone.
            // Disarm the guard so its Drop doesn't issue a useless
            // SIGKILL against a reaped pgid (worst case: signal
            // races into a PID re-used by the OS).
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
            }
            Ok(InterleavedOutput {
                merged,
                exit_code: status.code().unwrap_or(-1),
            })
        }
        Ok(Err(e)) => Err(ToolError::Msg(format!("wait failed: {}", e))),
        Err(_) => {
            // Timeout path already issues the killpg below; disarm
            // the drop guard so we don't double-signal.
            #[cfg(unix)]
            {
                let mut g = _pgguard;
                if let Some(ref mut gg) = g {
                    gg.disarm();
                }
                if let Some(pid) = pid {
                    // SAFETY: killpg with negative pid sends to the
                    // process group. SIGKILL is the same on every
                    // POSIX platform; libc::pid_t is i32 on every
                    // platform dirge supports.
                    unsafe {
                        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
            }
            let _ = pid;
            Err(ToolError::Msg(format!("Command timed out after {}s", secs)))
        }
    }
}

pub struct BashTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    pub sandbox: Sandbox,
    cache: Option<ToolCache>,
}

impl BashTool {
    #[allow(dead_code)]
    pub fn new(permission: Option<PermCheck>, ask_tx: Option<AskSender>, sandbox: Sandbox) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: None,
        }
    }

    pub fn with_cache(
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
        sandbox: Sandbox,
        cache: ToolCache,
    ) -> Self {
        BashTool {
            permission,
            ask_tx,
            sandbox,
            cache: Some(cache),
        }
    }
}

impl Tool for BashTool {
    const NAME: &'static str = "bash";

    type Error = ToolError;
    type Args = BashArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a bash command in the current working directory. Returns stdout and stderr.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (optional)" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: BashArgs) -> Result<String, ToolError> {
        // Strip control characters from the command string before
        // it reaches bash. The LLM can embed raw escape sequences
        // and C0 controls in tool arguments; a bare BEL or ESC in
        // a `bash -c` argument would be interpreted by the shell
        // (or passed through to child processes that write to
        // /dev/tty, bypassing our pipe capture). Keep \n (multi-
        // line scripts via `-c`) and \t (indentation).
        let command =
            crate::ui::ansi::strip_escapes(&args.command, crate::ui::ansi::StripPolicy::KEEP_BOTH);
        check_bash_segments(&self.permission, &self.ask_tx, &command).await?;

        // F6: spawn into its own process group so a timeout can
        // SIGKILL the entire subprocess tree, not just the
        // immediate `bash` child. Before this, `pi` would spawn
        // `npm install`, the 120s timeout fired, the future was
        // dropped (taking the tokio `Child` with it), but bash's
        // children — and theirs — kept running orphaned under PID 1.
        // pi (`bash.ts:76-81`) does this via `detached: true` +
        // `killProcessTree(pid)`.
        let secs = args.timeout.unwrap_or(120);
        if secs == 0 {
            return Err(ToolError::Msg("timeout must be > 0".to_string()));
        }
        let output = run_with_timeout(self.sandbox.wrap_command(&command), secs).await?;

        // F12: `merged` already contains stdout + stderr in arrival
        // order. Previously we concatenated stdout then stderr,
        // mis-ordering interleaved output.
        let mut result = output.merged;
        // Cap raw bash output before it enters LLM context. The
        // UI's `render_tool_output` already truncates the display,
        // but the full string was being persisted to
        // `ToolCallState::Completed` → fed back to the LLM on the
        // next turn. `cat /dev/urandom | head -c 10M` would have
        // shoved millions of tokens at the model. Apply the cap
        // here at the source. 256 KiB ≈ 65k tokens worst-case,
        // already well above any sensible single-command output.
        const BASH_OUTPUT_CAP_BYTES: usize = 256 * 1024;
        if result.len() > BASH_OUTPUT_CAP_BYTES {
            // Slice at UTF-8 char boundary.
            let mut cut = BASH_OUTPUT_CAP_BYTES;
            while cut > 0 && !result.is_char_boundary(cut) {
                cut -= 1;
            }
            let dropped = result.len() - cut;
            result.truncate(cut);
            result.push_str(&format!(
                "\n…[bash output truncated: dropped {} bytes ({} KiB total); pipe through head/grep to keep the LLM context lean]",
                dropped,
                (cut + dropped) / 1024,
            ));
        }
        if output.exit_code != 0 {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&format!("Exit code: {}", output.exit_code));
        }
        // Bash may have mutated the filesystem; conservatively invalidate the
        // per-turn read/grep/list cache.
        if let Some(ref cache) = self.cache {
            cache.clear();
        }
        Ok(result)
    }
}

async fn check_bash_segments(
    permission: &Option<PermCheck>,
    ask_tx: &Option<AskSender>,
    command: &str,
) -> Result<(), ToolError> {
    // M3 (dirge-6ab): every bash permission decision routes through
    // the `enforce` chokepoint. Each compound-statement segment is
    // checked independently so `git diff && rm -rf /` gets BOTH `git`
    // AND `rm` checked against the user's bash rules — not just the
    // leading command. Redirect targets (`> file`) additionally route
    // through the `write` tool rules so write/edit path rules apply,
    // closing the C4 audit gap where targets hit bash rules with
    // path-string inputs and fell through to default Allow.
    #[cfg(feature = "semantic-bash")]
    {
        let (segments, complex) = bash::parse_bash_segments_full(command)
            .unwrap_or_else(|_| (vec![command.to_string()], false));

        if complex {
            // Subshell / command substitution / process substitution /
            // arithmetic expansion: tree-sitter declined to split.
            // Force a prompt on the WHOLE command so the user
            // confirms the unfamiliar shape. Maki does the same
            // (maki-agent/src/permissions.rs:441-455).
            //
            // PERM-6: even when complex, still extract redirect
            // targets and mutation paths so that `echo "$(rm /etc/passwd)"`
            // doesn't slip through when the outer `echo` is allowed.
            for target in bash::extract_redirect_targets(command) {
                enforce(permission, ask_tx, "write", Scope::PathResolve(&target)).await?;
            }
            for path in bash::extract_mutation_paths(command) {
                enforce(permission, ask_tx, "write", Scope::PathResolve(&path)).await?;
            }
            enforce(permission, ask_tx, "bash", Scope::Raw(command)).await?;
            return Ok(());
        }

        for segment in &segments {
            enforce(permission, ask_tx, "bash", Scope::Raw(segment)).await?;
        }

        // M3 fix to the C4 redirect-target gap: route targets through
        // the `write` tool name (not `bash`), since redirection
        // semantically writes files. Previously this was routed as
        // `check_perm_path(tool="bash", path=&target)`, looking the
        // target up in BASH rules — command-style globs that don't
        // match path strings, falling through to default Allow. With
        // `tool="write"` the user's write rules govern: deny lists
        // (`/etc/**`, `~/.ssh/**`, `~/.aws/credentials`) now fire on
        // bash redirects too. Falsely-prompting `< file` (read-side)
        // is acceptable; the `extract_redirect_targets` walker
        // intentionally skips heredoc / herestring and only emits
        // write-side targets (`>`, `>>`, `&>`, `1>`, `2>`).
        for target in bash::extract_redirect_targets(command) {
            enforce(permission, ask_tx, "write", Scope::PathResolve(&target)).await?;
        }
        // F1 (dirge-dvy): route positional path arguments to
        // file-mutating commands (rm / cp / mv / mkdir / rmdir /
        // touch / chmod / chown / ln / tee / dd) through the write
        // rules too. Without this, a permissive bash rule like
        // `rm *: allow` silently allowed `rm /etc/passwd` because
        // the path-side write deny never saw the argument. Ported
        // from opencode shell.ts:30-51 (`FILES` set) + :191-221
        // (`pathArgs` filter logic). The extractor skips flags
        // (`-r`, `--recursive`), chmod permission specs (`+x`),
        // and chmod/chown's first positional arg (mode / owner).
        for path in bash::extract_mutation_paths(command) {
            enforce(permission, ask_tx, "write", Scope::PathResolve(&path)).await?;
        }
        Ok(())
    }
    #[cfg(not(feature = "semantic-bash"))]
    {
        // Best-effort coarse split when tree-sitter isn't compiled in.
        // Without it, a command like `safe_cmd && rm -rf /` would be
        // checked as a single string against the bash rules and might
        // squeak through if `safe_cmd && rm` doesn't match any deny.
        // Split on the unambiguous compound separators (`&&`, `;`,
        // `||`) so each segment is checked individually.
        //
        // F10: the splitter now respects shell quoting. The naive
        // `command.split(";")` split inside quoted strings, so
        // `echo "; rm -rf /"` produced segments `echo "` and
        // `rm -rf /"` — the second matched the bash rule for `rm`
        // and could trigger a deny that the user thought was safe.
        // The fixed splitter walks character-by-character and only
        // emits a boundary when not inside `'…'`, `"…"`, or after
        // a backslash escape.
        let segments = quote_aware_split(command);

        // Flag command substitution / subshell constructs / ANSI-C
        // quoting that need a full parser. Surface as one
        // whole-command check so the user sees the unfamiliar form
        // before any segment runs.
        //
        // `$'...'` ANSI-C quoting was missing from the original
        // check, leaving a small bypass: `echo $'hi\nrm -rf /; ls'`
        // (with embedded literal newlines via `\n`) treated the body
        // as one quoted token, so `quote_aware_split` didn't see the
        // `;` as a separator. Adding `$'` to the substitution list
        // makes the whole command get checked as a single string —
        // the rules can still match the safe form, but the LLM
        // doesn't get a free pass on obscure quoting.
        let has_substitution = command.contains("$(")
            || command.contains('`')
            || command.contains("<(")
            || command.contains(">(")
            || command.contains("$'")
            // PERM-5: heredocs (`<<` / `<<-`). The heredoc body
            // is invisible to `quote_aware_split` — the redirect
            // operator is one token but the body starting on the
            // next line can contain any command. Treat as complex.
            || command.contains("<<");
        if has_substitution {
            enforce(permission, ask_tx, "bash", Scope::Raw(command)).await?;
            return Ok(());
        }
        for segment in &segments {
            enforce(permission, ask_tx, "bash", Scope::Raw(segment)).await?;
        }
        Ok(())
    }
}

/// Split a shell command on `;`, `&&`, `||` separators that appear
/// OUTSIDE single quotes, double quotes, or backslash escapes.
/// Used only on the no-`semantic-bash` build path — the
/// tree-sitter path delegates to the real bash grammar in
/// `semantic::adapters::bash` and doesn't need this.
///
/// Edge cases:
/// - `echo "; rm"` → one segment (the `;` is quoted).
/// - `echo 'a&&b'` → one segment.
/// - `echo \; ls` → one segment (the `;` is escaped).
/// - `cmd1; cmd2 && cmd3` → three segments, trimmed.
/// - Empty / whitespace-only segments dropped.
#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn quote_aware_split(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_backslash = false;

    while i < bytes.len() {
        let b = bytes[i];

        if prev_backslash {
            prev_backslash = false;
            i += 1;
            continue;
        }

        if b == b'\\' && !in_single {
            // Inside single quotes, backslash is literal; otherwise it
            // escapes the next byte.
            prev_backslash = true;
            i += 1;
            continue;
        }

        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }

        if !in_single && !in_double {
            // Check for `&&` and `||` (2-byte) BEFORE single-byte `;`/`|`/`&`.
            if i + 1 < bytes.len()
                && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
            {
                push_segment(command, start, i, &mut segments);
                i += 2;
                start = i;
                continue;
            }
            if b == b';' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // Pipe `|` (single-byte) — must be checked AFTER `||`
            // above. Without this, a command like `safe_cmd | rm
            // -rf /` was treated as one segment and only `safe_cmd`'s
            // permission rule applied; the destructive RHS rode in
            // unchecked. The semantic-bash tree-sitter path correctly
            // splits pipelines; this fallback didn't.
            if b == b'|' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
            // B3-6 (audit fix): background `&` (single-byte) — must
            // be checked AFTER `&&` above. Without this,
            // `safe_cmd & rm -rf /` rode through with only the LHS
            // matching a permission rule; the backgrounded LHS plus
            // unchecked RHS would both execute.
            if b == b'&' {
                push_segment(command, start, i, &mut segments);
                i += 1;
                start = i;
                continue;
            }
        }

        i += 1;
    }

    push_segment(command, start, bytes.len(), &mut segments);
    segments
}

#[cfg_attr(feature = "semantic-bash", allow(dead_code))]
fn push_segment<'a>(command: &'a str, start: usize, end: usize, out: &mut Vec<&'a str>) {
    if end <= start {
        return;
    }
    let s = command[start..end].trim();
    if !s.is_empty() {
        out.push(s);
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    /// F6: a timed-out `sleep 9999` (or any long-running command)
    /// must actually be killed when the timeout fires. Before this
    /// fix, dropping the tokio future left the bash child running
    /// orphaned. The test runs `sleep 5` with a 1-second timeout
    /// and asserts: (a) we return the timeout error within ~1.5s,
    /// (b) the time to return is much less than the requested
    /// sleep duration — proving the process was actually killed
    /// rather than us racing to read its output.
    #[tokio::test]
    async fn run_with_timeout_kills_orphaned_child() {
        let start = std::time::Instant::now();
        let cmd = {
            let mut c = Command::new("bash");
            c.arg("-c").arg("sleep 5");
            c
        };
        let result = run_with_timeout(cmd, 1).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout error, got {:?}", result);
        let msg = format!("{:?}", result);
        assert!(
            msg.contains("timed out"),
            "expected 'timed out' in error: {msg}",
        );
        // The timeout fires at 1s; we allow up to 2s slack for
        // CI variance. The KEY assertion is we return well before
        // the 5s sleep would have completed naturally.
        assert!(
            elapsed < Duration::from_secs(3),
            "took too long to return: {:?}",
            elapsed,
        );
    }

    /// F6: a command that completes under the timeout returns
    /// normally — no false-positive kill.
    #[tokio::test]
    async fn run_with_timeout_returns_output_on_success() {
        let cmd = {
            let mut c = Command::new("bash");
            c.arg("-c").arg("echo hi");
            c
        };
        let out = run_with_timeout(cmd, 5).await.expect("should succeed");
        assert_eq!(out.merged.trim(), "hi");
    }

    /// F12: stdout + stderr interleave in true arrival order, not
    /// stdout-then-stderr. Use a script that pings stderr between
    /// stdout writes; the merged output must keep the order.
    #[tokio::test]
    async fn run_with_timeout_interleaves_stdout_stderr() {
        let cmd = {
            let mut c = Command::new("bash");
            c.arg("-c")
                // Print to alternating streams with small delays so
                // the kernel actually buffers them in order. Without
                // the delay, both lines might land in the same
                // select! poll and ordering becomes about poll bias.
                .arg(
                    "echo OUT-A; \
                     sleep 0.05; \
                     echo ERR-1 >&2; \
                     sleep 0.05; \
                     echo OUT-B; \
                     sleep 0.05; \
                     echo ERR-2 >&2",
                );
            c
        };
        let out = run_with_timeout(cmd, 5).await.expect("should succeed");
        let lines: Vec<&str> = out.merged.lines().collect();
        // Pre-F12 we'd see [OUT-A, OUT-B, ERR-1, ERR-2] because
        // stdout was concatenated before stderr. Post-F12 each line
        // appears in arrival order.
        assert_eq!(
            lines,
            vec!["OUT-A", "ERR-1", "OUT-B", "ERR-2"],
            "stdout/stderr should interleave by arrival",
        );
    }

    /// F10: a `;` inside double quotes is part of the string, not a
    /// segment boundary. Before this, the naive splitter produced
    /// two segments, the second being `rm -rf /"`, which could
    /// match a bash deny rule for `rm`.
    #[test]
    fn quote_aware_split_keeps_semi_in_double_quotes() {
        let segments = quote_aware_split(r#"echo "; rm -rf /""#);
        assert_eq!(segments.len(), 1);
        assert!(segments[0].contains("rm -rf /"));
    }

    /// `&&` inside single quotes is literal too.
    #[test]
    fn quote_aware_split_keeps_compound_in_single_quotes() {
        let segments = quote_aware_split("echo 'a && b'");
        assert_eq!(segments.len(), 1);
    }

    /// Escaped `;` is literal — `echo \; ls` is ONE command in bash.
    #[test]
    fn quote_aware_split_respects_backslash_escape() {
        let segments = quote_aware_split(r"echo \; ls");
        assert_eq!(segments.len(), 1, "got: {:?}", segments);
    }

    /// Real compounds still split correctly into segments.
    #[test]
    fn quote_aware_split_splits_unquoted_compounds() {
        let segments = quote_aware_split("cmd1 && cmd2; cmd3 || cmd4");
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0], "cmd1");
        assert_eq!(segments[1], "cmd2");
        assert_eq!(segments[2], "cmd3");
        assert_eq!(segments[3], "cmd4");
    }

    /// B3-6: background `&` is a segment separator. Distinct from
    /// `&&`, which is handled by the earlier 2-byte branch.
    #[test]
    fn quote_aware_split_splits_background_ampersand() {
        let segments = quote_aware_split("safe_cmd & rm -rf /tmp/x");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], "safe_cmd");
        assert_eq!(segments[1], "rm -rf /tmp/x");
    }

    #[test]
    fn quote_aware_split_keeps_logical_and_separate_from_background() {
        // `&&` still binds as a 2-byte compound — must NOT be split
        // as two `&` separators.
        let segments = quote_aware_split("a && b & c");
        assert_eq!(segments, vec!["a", "b", "c"]);
    }

    /// Regression: bare `|` pipes must split into segments. Before
    /// this, a command like `safe_cmd | rm -rf /` was treated as
    /// one unit and only `safe_cmd`'s permission rule applied.
    #[test]
    fn quote_aware_split_splits_on_bare_pipe() {
        let segments = quote_aware_split("safe_cmd | rm -rf /tmp/x");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].trim(), "safe_cmd");
        assert_eq!(segments[1].trim(), "rm -rf /tmp/x");
    }

    /// `||` must NOT also match the single-`|` arm (already covered
    /// by the existing `||` test, but pin the interaction here too).
    #[test]
    fn quote_aware_split_or_and_pipe_distinct() {
        let segments = quote_aware_split("a || b | c");
        assert_eq!(segments.len(), 3, "got {segments:?}");
        assert_eq!(segments[0].trim(), "a");
        assert_eq!(segments[1].trim(), "b");
        assert_eq!(segments[2].trim(), "c");
    }

    /// Empty / whitespace-only segments dropped.
    #[test]
    fn quote_aware_split_drops_empty_segments() {
        let segments = quote_aware_split(";; cmd ;");
        assert_eq!(segments, vec!["cmd"]);
    }

    /// Mixed: quoted compound + unquoted compound.
    #[test]
    fn quote_aware_split_mixed_quoted_and_unquoted() {
        let segments = quote_aware_split(r#"echo "a; b" ; ls"#);
        assert_eq!(segments.len(), 2);
        assert!(segments[0].contains("a; b"));
        assert_eq!(segments[1], "ls");
    }

    // M3 (dirge-6ab) — segment-level bash gating regression tests.
    // These pin the "every command in a compound gets checked
    // separately" invariant the user asked about
    // ("agent runs `git diff && rm -rf /`, what happens?").

    /// `git diff && rm -rf /` must be denied — the second segment
    /// hits the default `rm -rf /**` deny rule even though the
    /// first segment is allowlisted. Pre-this-test, the path was
    /// covered by the parser test in `semantic::adapters::bash`,
    /// but nothing end-to-end pinned that `check_bash_segments`
    /// actually walks the segments through the perm checker.
    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn compound_command_denies_dangerous_segment() {
        use crate::permission::{PermissionConfig, SecurityMode, checker::PermissionChecker};

        let config = PermissionConfig::default();
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        let result = check_bash_segments(&Some(perm), &None, "git diff && rm -rf /").await;
        assert!(
            result.is_err(),
            "compound: rm segment must hit deny rule even after safe git segment; got {result:?}",
        );
        let msg = format!("{:?}", result);
        assert!(
            msg.contains("denied") || msg.contains("Denied"),
            "expected 'denied' in error: {msg}",
        );
    }

    /// Output redirect targets route through the `write` tool rules
    /// (M3 fix to the C4 audit). Pre-fix: `tool="bash"` lookup with a
    /// path string, no matching command pattern, fell through to
    /// default Allow — `echo hi > /etc/passwd` ran without prompting.
    /// Post-fix: routes through write rules.
    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn redirect_target_routes_through_write_rules() {
        use crate::permission::{
            Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
        };
        use std::collections::HashMap;

        // Configure write to deny everywhere; without an explicit
        // rule the M2/M4-pre default is still Allow, so we set an
        // explicit deny to make the test robust against the
        // default-flip.
        let mut write_rules = HashMap::new();
        write_rules.insert("/etc/**".to_string(), Action::Deny);
        let config = PermissionConfig {
            write: Some(ToolPerm::Granular(write_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        let result = check_bash_segments(&Some(perm), &None, "echo hi > /etc/passwd").await;
        assert!(
            result.is_err(),
            "redirect to /etc/passwd should be denied by write rules; got {result:?}",
        );
    }

    /// Sibling check: a redirect target inside the working directory
    /// (non-external) passes the write-rules check. Without this, a
    /// regression that over-broadly denied all redirects could pass
    /// the negative case above and ship.
    ///
    /// Uses an in-cwd path because the catch-all at
    /// `permission/checker.rs:434` upgrades unmatched-Allow to Ask
    /// for EXTERNAL paths — so `/tmp/x` (external to the test's cwd
    /// of the dirge repo) would test the external-path catch-all,
    /// not the write-rules-allow path we want to exercise here.
    /// M3 is intentionally tightening external bash-redirects to
    /// prompt; this test pins the in-cwd happy path.
    // F1 (dirge-dvy) — bash arg-side path checks. Pin that
    // file-mutating commands route their positional path args
    // through the write rules, independent of the bash command-
    // pattern check.

    /// `rm /etc/passwd` is denied via write rules even when the
    /// user's bash config is otherwise permissive. Pre-F1: the
    /// path-side check never ran for arguments (only redirect
    /// targets), so a `bash: { "rm *": "allow" }` rule silently
    /// allowed `rm /etc/passwd`. Post-F1: the path arg routes
    /// through `enforce(write, /etc/passwd)` and the user's
    /// write deny rule fires.
    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn rm_arg_path_routes_through_write_rules() {
        use crate::permission::{
            Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
        };
        use std::collections::HashMap;

        // Permissive bash: allow `rm *`. Restrictive write: deny
        // `/etc/**`. Without F1, the bash allow would let
        // `rm /etc/passwd` through.
        let mut bash_rules = HashMap::new();
        bash_rules.insert("rm *".to_string(), Action::Allow);
        let mut write_rules = HashMap::new();
        write_rules.insert("/etc/**".to_string(), Action::Deny);
        let config = PermissionConfig {
            bash: Some(ToolPerm::Granular(bash_rules)),
            write: Some(ToolPerm::Granular(write_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        let result = check_bash_segments(&Some(perm), &None, "rm /etc/passwd").await;
        assert!(
            result.is_err(),
            "rm /etc/passwd must hit write deny rule even when bash rule allows; got {result:?}",
        );
    }

    /// chmod's FIRST arg (the mode spec like `777` or `u+x`) is
    /// NOT treated as a path. Only subsequent positional args go
    /// through the write check.
    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn chmod_skips_mode_spec_routes_paths() {
        use crate::permission::{
            Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
        };
        use std::collections::HashMap;

        let mut bash_rules = HashMap::new();
        bash_rules.insert("chmod *".to_string(), Action::Allow);
        let mut write_rules = HashMap::new();
        write_rules.insert("/etc/**".to_string(), Action::Deny);
        let config = PermissionConfig {
            bash: Some(ToolPerm::Granular(bash_rules)),
            write: Some(ToolPerm::Granular(write_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        // `777` is the mode spec; it must NOT be treated as a
        // path arg (would resolve to /cwd/777, false-positive).
        // `/etc/passwd` IS a path → should hit write deny.
        let result = check_bash_segments(&Some(perm), &None, "chmod 777 /etc/passwd").await;
        assert!(
            result.is_err(),
            "chmod 777 /etc/passwd: mode skipped, path arg gated; got {result:?}",
        );
    }

    /// Flags (`-r`, `--recursive`) are correctly skipped when
    /// extracting path args.
    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn flags_skipped_when_extracting_paths() {
        use crate::permission::{
            Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
        };
        use std::collections::HashMap;

        let mut bash_rules = HashMap::new();
        bash_rules.insert("rm *".to_string(), Action::Allow);
        let mut write_rules = HashMap::new();
        write_rules.insert("/etc/**".to_string(), Action::Deny);
        let config = PermissionConfig {
            bash: Some(ToolPerm::Granular(bash_rules)),
            write: Some(ToolPerm::Granular(write_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        // `-rf` is a flag; `/etc/passwd` is the path. Flag is
        // skipped, path hits deny.
        let result = check_bash_segments(&Some(perm), &None, "rm -rf /etc/passwd").await;
        assert!(
            result.is_err(),
            "rm -rf /etc/passwd: flag skipped, path arg gated; got {result:?}",
        );
    }

    #[cfg(feature = "semantic-bash")]
    #[tokio::test]
    async fn redirect_target_allowed_when_write_permits() {
        use crate::permission::{
            Action, PermissionConfig, SecurityMode, ToolPerm, checker::PermissionChecker,
        };
        use std::collections::HashMap;

        // M4 (dirge-ojn): `write` no longer defaults to Allow; it
        // falls to the new global Ask. F2 (dirge-jlj): write
        // additionally consults `edit` rules. Install allow rules
        // for BOTH so the combined check passes — matches the
        // user-facing semantic that "write to X" requires write
        // AND edit to permit it.
        let mut write_rules = HashMap::new();
        write_rules.insert("**".to_string(), Action::Allow);
        let mut edit_rules = HashMap::new();
        edit_rules.insert("**".to_string(), Action::Allow);
        let config = PermissionConfig {
            write: Some(ToolPerm::Granular(write_rules)),
            edit: Some(ToolPerm::Granular(edit_rules)),
            ..Default::default()
        };
        let checker = PermissionChecker::new(&config, SecurityMode::Standard, None);
        let perm = std::sync::Arc::new(std::sync::Mutex::new(checker));

        let result = check_bash_segments(&Some(perm), &None, "echo hi > target/test-out.txt").await;
        assert!(
            result.is_ok(),
            "redirect to an explicitly-allowed target should pass; got {result:?}",
        );
    }
}
