# Coordinated Subagents

Dirge can coordinate background subagents so the main agent can split independent research, implementation, and review work across specialized profiles. Instead of reacting to each result as it arrives, dirge waits for the current batch to finish and gives the main agent one reconciliation update.

This feature is opt-in. Without `subagent_dispatch_strategy`, `task` works as a normal independent subagent tool.

> Coordinated dispatch runs in the interactive TUI. It is not enabled for `--print`, ACP, or other noninteractive runs.

## Quick start

Create one read-only and one read-write agent profile, then enable coordinated dispatch in `~/.config/dirge/config.json` or your project configuration:

```jsonc
{
  "subagent_dispatch_strategy": "full",
  "subagent_write_isolation": "auto",
  "agents": {
    "researcher": {
      "model": "haiku",
      "description": "Investigates the repository and reports findings",
      "subagent": {
        "tools": "readonly",
        "max_turns": 15
      }
    },
    "implementer": {
      "model": "sonnet",
      "description": "Implements isolated changes and verifies them",
      "subagent": {
        "tools": "readwrite",
        "max_turns": 25,
        "timeout_secs": 900
      }
    }
  }
}
```

Restart dirge after changing configuration. At startup, dirge resolves the eligible profiles and enables coordination only when it has both a `readonly` and a `readwrite` profile with tools available.

For the full profile format, precedence rules, and regular `task(agent="…")` use, see [Agent profiles](agents.md).

## Dispatch strategies

Set `subagent_dispatch_strategy` to one of these values:

- **`off`** — the default. `task(background=true)` results are delivered independently.
- **`optional`** — the main agent may handle small tasks directly, but uses coordinated batches when it delegates work.
- **`full`** — substantive routed work is delegated as coordinated background tasks.

Invalid or empty values resolve to `off` with a warning. `optional` warns and disables coordination if the required profiles are unavailable. `full` reports a startup error instead, so a missing profile or disabled tools cannot silently change its behavior.

## Profiles used for coordination

A coordinating session needs both of these profile tiers:

- **`readonly`** profiles can inspect the repository, search files, and use enabled web tools. Use them for research, review, and investigation.
- **`readwrite`** profiles can also edit files and run commands. Use them for implementation work.

Profiles with `subagent.tools: "toolless"` do not satisfy either coordinator tier. The `allow` and `deny` fields can narrow a profile's tool set, but cannot grant tools outside its tier.

Useful subagent settings are:

- `tools`: `toolless`, `readonly`, or `readwrite`.
- `max_turns`: maximum number of turns for a tooled subagent. The default is 25.
- `timeout_secs`: wall-clock timeout in seconds. Dirge clamps it to 30 through 3600 seconds.
- `allow` and `deny`: restrict the selected tier's tool set further.

In Markdown profiles, use the corresponding frontmatter names:

```markdown
---
description: Repository investigator
subagent_tools: readonly
subagent_max_turns: 15
subagent_timeout_secs: 600
subagent_deny: [webfetch]
---
Investigate the requested area, cite relevant files, and return concise findings.
```

## How a coordinated batch works

The main agent starts subagents with `task(background=true)`. A batch can include up to four running subagents.

Dirge collects all terminal results in the current batch before continuing the coordinator. The reconciliation update includes each task's prompt and outcome, retry information, and—when a writer ran in a worktree—its branch, path, commits, and retention status.

This prevents the main agent from acting on a partial implementation while another task in the same batch is still running. Background shell commands are separate from subagent dispatch and are not part of this barrier.

### Retries

A failed coordinated task can be retried once with `retry_of=<task-id>`. The original task must be a known failed coordinator task. Completed, running, cancelled, unknown, and already-retried tasks cannot be retried.

Cancelled work is intentionally not retryable: the main agent should decide whether to start new work after the cancellation rather than treating it as an ordinary task failure.

## Read-write subagents and worktrees

Read-write subagents can modify code. Set `subagent_write_isolation` to choose where they work:

- **`auto`** — the default. Dirge creates an isolated Git worktree when it can; otherwise it runs one serialized writer in the parent checkout.
- **`worktree`** — require a worktree. The dispatch fails if isolation is unavailable.
- **`serialize`** — always use the parent checkout and allow only one writer at a time.

Worktree write-isolation requires a Linux sandbox (bwrap). Without a confining sandbox, the writer's shell could escape the worktree via `../` or absolute paths — a false isolation guarantee — so dirge refuses to create a worktree for `auto` and errors on `worktree`. When a worktree is unavailable, `auto` falls back to running the writer *serialized in the parent checkout*. This fallback path **requires a clean parent checkout** — a dirty checkout makes the writer fail with a clear message; commit or stash first. `worktree` mode fails whenever a worktree cannot be created (no confining sandbox, missing `git-worktree` feature, dirty parent, or MicroVM sandbox — which has its own full isolation).

An isolated writer gets a newly built tool registry rooted at its own worktree. Its file, search, navigation, and shell tools are confined to that checkout. Background shell jobs also use a writer-local store, so the writer cannot inspect or terminate the parent agent's shell jobs.

Worktrees and branches use a `dirge-task-<task-uuid>` name. The writer is instructed to inspect its status, make a descriptive commit, run relevant checks, and report its commit, changed files, tests, and unresolved issues.

### Reconciliation and cleanup

Dirge does not automatically merge a writer branch. The main agent reviews the result, chooses whether to merge it, and performs any integration and final verification in the main checkout.

A dirty worktree or a worktree containing commits is retained for recovery. A clean worktree with no commits may be removed. If a session is cancelled, the same retention rule applies, so useful committed work remains available at the reported salvage path.

## Operational limits

- Coordinated subagents must run in the background. Read-write coordinator profiles cannot run as foreground tasks because their isolation and lifecycle tracking are background-only.
- At most four background subagents run at once.
- The coordinator is separate from the manual `orchestrator.janet` and `delegate.janet` plugin workflows. Do not combine them in the same session.
- `/cd` does not reload coordinator profiles. Restart dirge after changing profile definitions or project directory configuration.

## Troubleshooting

- **Coordinator mode does not start:** ensure tools are enabled and that loaded profiles include both `readonly` and `readwrite` tiers. `full` prints the missing tier at startup.
- **A writer runs in the parent checkout:** `auto` could not create an isolated worktree because Git worktree support is unavailable or the active sandbox is not confining (not bwrap), so the writer runs serialized in the parent checkout — which must be clean (see below).
- **A writer dispatch fails immediately:** use `auto` for fallback behavior, or fix the repository/sandbox prerequisite required by `worktree` mode.
- **A writer fails with "cannot run a read-write subagent in a dirty parent checkout":** commit or stash your uncommitted changes, or run under a Linux bwrap sandbox for worktree isolation.
- **A writer's changes are not in the main branch:** this is expected. Inspect the reconciliation result and merge the reported branch deliberately.
- **A task result has not appeared yet:** a coordinated batch is delivered only after every task in that batch reaches a terminal state. Do not poll for partial output; continue other work or wait for the reconciliation update.

## Related documentation

- [Agent profiles](agents.md) for defining subagent profiles and their tool policies.
- [Permissions](permissions.md) for the authorization policy that still applies to subagent tool calls.
- [Configuration](config.md) for sandbox and other runtime configuration.
