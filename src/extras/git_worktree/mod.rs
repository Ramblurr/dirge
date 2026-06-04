use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub branch: String,
    pub worktree_path: PathBuf,
    pub main_repo_path: PathBuf,
}

pub fn detect() -> Option<WorktreeInfo> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let worktree_path: PathBuf = Path::new(&git_dir).canonicalize().ok()?;

    if common_dir == git_dir {
        return None;
    }

    let main_repo_path: PathBuf = Path::new(&common_dir).parent().map(|p| p.to_path_buf())?;
    let main_repo_path = main_repo_path.canonicalize().ok()?;

    let branch = current_branch().unwrap_or_default();

    Some(WorktreeInfo {
        branch,
        worktree_path: worktree_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(worktree_path),
        main_repo_path,
    })
}

pub fn current_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch == "HEAD" { None } else { Some(branch) }
}

pub fn default_branch(repo_path: &Path) -> Option<String> {
    for name in &["main", "master"] {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["rev-parse", "--verify", name])
            .output()
            .ok();
        if let Some(out) = output
            && out.status.success()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Reject branch names that would be unsafe or ambiguous as a `git
/// worktree add -b <name>` argument. EXT-8: pre-flight check against
/// the obviously-hostile shapes before invoking git; combined with the
/// `--` argv separator below, this defangs both flag-injection
/// (`--exec=…`) and the assorted git ref-name traversal / metachar
/// foot-guns (`..`, `~`, `:`, `HEAD`, control bytes).
fn validate_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("branch name must not be empty".to_string());
    }
    if name.starts_with('-') {
        // Leading `-` would be parsed by git as a flag even though
        // it sits in the positional slot — covered also by `--` below,
        // but reject early for a clearer error.
        return Err(format!(
            "branch name {name:?} must not start with '-' (looks like a git flag)"
        ));
    }
    if name == "HEAD" || name == "@" {
        return Err(format!("branch name {name:?} is a reserved git ref"));
    }
    if name.contains("..") {
        return Err(format!(
            "branch name {name:?} must not contain '..' (git ref-name rule)"
        ));
    }
    for bad in ['~', ':', '^', '?', '*', '['] {
        if name.contains(bad) {
            return Err(format!(
                "branch name {name:?} must not contain '{bad}' (git ref-name rule)"
            ));
        }
    }
    if name
        .chars()
        .any(|c| c == '\0' || (c.is_control() && c != '\t'))
    {
        return Err(format!(
            "branch name {name:?} must not contain null bytes or control characters"
        ));
    }
    Ok(())
}

pub fn create(name: &str) -> Result<(PathBuf, WorktreeInfo), String> {
    validate_branch_name(name)?;
    let target = format!("../{}", name);

    // EXT-8: insert `--` before the positional args so a maliciously-
    // crafted but technically-valid name can't be re-interpreted as
    // a flag by git's option parser. `validate_branch_name` already
    // rejects the obvious shapes; `--` is belt-and-suspenders.
    let output = Command::new("git")
        .args(["worktree", "add", "-b", name, "--", &target])
        .output()
        .map_err(|e| format!("failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr.trim()));
    }

    let wt_path = PathBuf::from(&target)
        .canonicalize()
        .map_err(|e| format!("failed to resolve worktree path: {}", e))?;

    let main_repo =
        std::env::current_dir().map_err(|e| format!("failed to get current dir: {}", e))?;

    Ok((
        wt_path.clone(),
        WorktreeInfo {
            branch: name.to_string(),
            worktree_path: wt_path,
            main_repo_path: main_repo,
        },
    ))
}

/// Run a git subcommand in `repo`, returning trimmed stdout on success
/// or a trimmed-stderr error on failure. Always inserts `-C <repo>`.
fn git_in(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// True if `repo`'s working tree has uncommitted changes (staged,
/// unstaged, or untracked).
fn is_dirty(repo: &Path) -> Result<bool, String> {
    Ok(!git_in(repo, &["status", "--porcelain"])?.is_empty())
}

/// Merge the worktree's branch into `target` in the main repo —
/// programmatically and conflict-safe (dirge-2qke). Replaces the prior
/// design that handed the whole merge (including "push and delete the
/// worktree") to an unconstrained LLM prompt.
///
/// Guarantees:
///   - Refuses if the worktree OR the main repo has uncommitted changes
///     (a conflicting merge against a dirty tree risks losing work).
///   - On a merge conflict the merge is **aborted** (`git merge --abort`)
///     so the repo is left exactly as it was — nothing half-merged.
///   - Never pushes and never deletes the worktree; the caller decides
///     what to do after a clean merge. So a failure can't strand work.
///
/// Returns `Ok(())` only when `target` cleanly contains the branch.
pub fn merge_worktree(info: &WorktreeInfo, target: &str) -> Result<(), String> {
    validate_branch_name(target)?;
    validate_branch_name(&info.branch)?;
    let main = info.main_repo_path.as_path();

    if is_dirty(&info.worktree_path)? {
        return Err(format!(
            "worktree '{}' has uncommitted changes — commit or discard them before merging",
            info.branch
        ));
    }
    if is_dirty(main)? {
        return Err(format!(
            "main repo at {} has uncommitted changes — commit or stash them before merging",
            main.display()
        ));
    }

    // Switch the main repo to the target branch (prefer `switch`, fall
    // back to `checkout` on older git). NOTE: no `--` separator here —
    // for `switch`/`checkout`/`merge` a `--` forces the following token to
    // be read as a PATHSPEC, not a branch/ref (e.g. `git checkout -- main`
    // restores a file named `main` instead of switching branches), and its
    // handling is git-version-dependent. `validate_branch_name` above
    // already rejects flag-shaped / metachar names, so the bare ref is safe.
    git_in(main, &["switch", target])
        .or_else(|_| git_in(main, &["checkout", target]))
        .map_err(|e| format!("failed to switch main repo to '{target}': {e}"))?;

    // --no-ff keeps the worktree's history explicit. On any failure
    // (conflict or otherwise) abort so the index/working tree are
    // restored to the pre-merge state.
    if let Err(e) = git_in(main, &["merge", "--no-ff", &info.branch]) {
        let _ = git_in(main, &["merge", "--abort"]);
        return Err(format!(
            "merge of '{}' into '{target}' could not complete cleanly and was aborted — \
             nothing was changed. Resolve it manually in {} (git merge {}). Details: {e}",
            info.branch,
            main.display(),
            info.branch
        ));
    }
    Ok(())
}

/// Remove a (merged) worktree from the main repo. Best-effort: callers
/// treat failure as non-fatal since the merge already succeeded. Must be
/// invoked with the cwd OUTSIDE the worktree being removed.
pub fn remove_worktree(main_repo: &Path, worktree_path: &Path) -> Result<(), String> {
    git_in(
        main_repo,
        &["worktree", "remove", "--", &worktree_path.to_string_lossy()],
    )
    .map(|_| ())
}

pub fn repo_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod merge_tests {
    //! dirge-2qke: `merge_worktree` is conflict-safe and never strands work.
    use super::*;

    /// Run git in `dir` with a fixed identity (so commits work without a
    /// global git config), panicking on failure with stderr.
    fn git(dir: &Path, args: &[&str]) -> String {
        let mut full = vec![
            "-c",
            "user.email=test@dirge",
            "-c",
            "user.name=dirge",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ];
        full.extend_from_slice(args);
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(&full)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(p: &Path, contents: &str) {
        std::fs::write(p, contents).unwrap();
    }

    /// A main repo on `main` with one commit, plus a sibling worktree on
    /// branch `feature`. Returns (info, tmp_root) — tmp_root is removed by
    /// the caller.
    fn setup() -> (WorktreeInfo, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "dirge-wt-merge-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("repo");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init"]);
        // Persist a repo-LOCAL identity into .git/config (shared across
        // worktrees). merge_worktree's internal git calls don't pass `-c`,
        // and CI runners have no global git identity, so without this the
        // `--no-ff` merge commit fails with "Committer identity unknown".
        git(&main, &["config", "user.email", "test@dirge.local"]);
        git(&main, &["config", "user.name", "dirge-test"]);
        git(&main, &["config", "commit.gpgsign", "false"]);
        write(&main.join("file.txt"), "base\n");
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "base"]);
        // Worktree on a new branch `feature`.
        let wt = root.join("feature");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                "--",
                wt.to_str().unwrap(),
            ],
        );
        let info = WorktreeInfo {
            branch: "feature".to_string(),
            worktree_path: wt,
            main_repo_path: main,
        };
        (info, root)
    }

    #[test]
    fn clean_merge_lands_feature_in_main() {
        let (info, root) = setup();
        // Distinct, non-conflicting change committed on the worktree.
        write(&info.worktree_path.join("new.txt"), "from feature\n");
        git(&info.worktree_path, &["add", "."]);
        git(&info.worktree_path, &["commit", "-m", "feature work"]);

        merge_worktree(&info, "main").expect("clean merge should succeed");

        // The feature file is now present in the main repo on `main`.
        assert_eq!(
            git(&info.main_repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert!(
            info.main_repo_path.join("new.txt").exists(),
            "merged file present in main"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn conflicting_merge_aborts_and_leaves_repo_clean() {
        let (info, root) = setup();
        // Conflicting edits to the SAME line on both branches.
        write(&info.main_repo_path.join("file.txt"), "main change\n");
        git(&info.main_repo_path, &["commit", "-am", "main edit"]);
        write(&info.worktree_path.join("file.txt"), "feature change\n");
        git(&info.worktree_path, &["commit", "-am", "feature edit"]);

        let err = merge_worktree(&info, "main").expect_err("conflicting merge must fail");
        assert!(
            err.contains("aborted"),
            "error should say it aborted: {err}"
        );
        // The merge was aborted: no MERGE_HEAD, clean tree, main's content intact.
        assert!(
            !info.main_repo_path.join(".git/MERGE_HEAD").exists(),
            "merge must be aborted (no MERGE_HEAD)"
        );
        assert!(
            git(&info.main_repo_path, &["status", "--porcelain"]).is_empty(),
            "main working tree must be clean after abort"
        );
        assert_eq!(
            std::fs::read_to_string(info.main_repo_path.join("file.txt")).unwrap(),
            "main change\n",
            "main's content is untouched by the aborted merge"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dirty_worktree_is_refused() {
        let (info, root) = setup();
        // Uncommitted change in the worktree.
        write(&info.worktree_path.join("file.txt"), "uncommitted\n");
        let err = merge_worktree(&info, "main").expect_err("dirty worktree must be refused");
        assert!(
            err.contains("uncommitted"),
            "error names the dirty state: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
