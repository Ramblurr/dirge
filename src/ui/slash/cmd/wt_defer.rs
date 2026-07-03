//! Shared pack/parse helpers for the `/wt-merge` and `/wt-exit` deferred-work
//! sentinel errors.
//!
//! `/wt-merge` and `/wt-exit` (in `worktree.rs`) hand work back to the outer
//! event loop in `ui/mod.rs` via `anyhow` sentinel error strings that begin
//! `DEFER_WT_MERGE:` / `DEFER_WT_EXIT:`. Both sides must agree on the wire
//! format, so the pack and parse live here and are imported by both.
//!
//! The inter-field delimiter is the ASCII Unit Separator `\u{1f}` (0x1F). It
//! cannot appear in a git ref or a realistic filesystem path, unlike `:`
//! which macOS/Linux paths may legally contain — a `:` in `main_path` or
//! `wt_path` used to shift every subsequent field and silently mis-target the
//! merge or truncate the `chdir`. The human-visible `DEFER_WT_*:` tag prefix
//! is unchanged; only the inter-field delimiter changed from `:` to 0x1F.

/// Fields consumed by the outer loop when a `/wt-merge` is deferred.
/// `repo_name` is carried on the wire (the producer always has it) but is not
/// parsed back — no consumer reads it.
pub(crate) struct WtMerge {
    pub(crate) branch: String,
    pub(crate) target: String,
    pub(crate) main_path: String,
    pub(crate) wt_path: String,
}

/// Fields consumed by the outer loop when a `/wt-exit` is deferred.
pub(crate) struct WtExit {
    pub(crate) main_path: String,
    // Carried on the wire (the producer packs it) and asserted by the
    // round-trip test, but the consumer only acts on `main_path`.
    #[allow(dead_code)]
    pub(crate) wt_path: String,
}

const MERGE_TAG: &str = "DEFER_WT_MERGE:";
const EXIT_TAG: &str = "DEFER_WT_EXIT:";
/// Field separator: ASCII Unit Separator. See module docs for why not ':'.
const SEP: char = '\u{1f}';

pub(crate) fn pack_wt_merge(
    branch: &str,
    target: &str,
    main_path: &str,
    wt_path: &str,
    repo_name: &str,
) -> String {
    format!("{MERGE_TAG}{branch}{SEP}{target}{SEP}{main_path}{SEP}{wt_path}{SEP}{repo_name}")
}

pub(crate) fn parse_wt_merge(s: &str) -> Option<WtMerge> {
    let body = s.strip_prefix(MERGE_TAG)?;
    let parts: Vec<&str> = body.split(SEP).collect();
    if parts.len() != 5 {
        return None;
    }
    Some(WtMerge {
        branch: parts[0].to_string(),
        target: parts[1].to_string(),
        main_path: parts[2].to_string(),
        wt_path: parts[3].to_string(),
    })
}

pub(crate) fn pack_wt_exit(main_path: &str, wt_path: &str) -> String {
    format!("{EXIT_TAG}{main_path}{SEP}{wt_path}")
}

pub(crate) fn parse_wt_exit(s: &str) -> Option<WtExit> {
    let body = s.strip_prefix(EXIT_TAG)?;
    let parts: Vec<&str> = body.split(SEP).collect();
    if parts.len() != 2 {
        return None;
    }
    Some(WtExit {
        main_path: parts[0].to_string(),
        wt_path: parts[1].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_merge_roundtrip_preserves_colons_in_paths() {
        // macOS/Linux paths may legally contain ':'; a ':' delimiter would
        // shift every field after main_path and (with the old exact-count
        // parse) make the whole handoff a silent no-op.
        let branch = "feature/x";
        let target = "main";
        let main_path = "/Users/me/proj:v2";
        let wt_path = "/Users/me/proj:v2/.git/worktrees/wt";
        let repo_name = "proj";
        let packed = pack_wt_merge(branch, target, main_path, wt_path, repo_name);
        assert!(packed.starts_with(MERGE_TAG));
        let m = parse_wt_merge(&packed).expect("parse should succeed");
        assert_eq!(m.branch, branch);
        assert_eq!(m.target, target);
        assert_eq!(m.main_path, main_path);
        assert_eq!(m.wt_path, wt_path);
    }

    #[test]
    fn wt_exit_roundtrip_preserves_colons_in_paths() {
        let main_path = "/Users/me/proj:v2";
        let wt_path = "/Users/me/proj:v2/.git/worktrees/wt";
        let packed = pack_wt_exit(main_path, wt_path);
        assert!(packed.starts_with(EXIT_TAG));
        let x = parse_wt_exit(&packed).expect("parse should succeed");
        assert_eq!(x.main_path, main_path);
        assert_eq!(x.wt_path, wt_path);
    }

    #[test]
    fn parse_wt_merge_returns_none_on_wrong_field_count() {
        // too few (missing trailing repo_name)
        assert!(parse_wt_merge("DEFER_WT_MERGE:a:b:c:d").is_none());
        // too many
        assert!(parse_wt_merge("DEFER_WT_MERGE:a:b:c:d:e:f").is_none());
        // wrong tag entirely
        assert!(parse_wt_merge("DEFER_WT_EXIT:a:b").is_none());
        // missing tag
        assert!(parse_wt_merge("a:b:c:d:e").is_none());
    }

    #[test]
    fn parse_wt_exit_returns_none_on_wrong_field_count() {
        // too few
        assert!(parse_wt_exit("DEFER_WT_EXIT:only_one").is_none());
        // too many
        assert!(parse_wt_exit("DEFER_WT_EXIT:a:b:c").is_none());
        // wrong tag entirely
        assert!(parse_wt_exit("DEFER_WT_MERGE:a:b").is_none());
    }
}
