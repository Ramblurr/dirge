//! Multi-tier auto-compaction decision engine.
//!
//! Faithful port of `DeepSeek-Reasonix/src/context-manager.ts` (345 lines).
//!
//! Five threshold tiers govern when and how aggressively the loop
//! folds older context into a summary:
//!
//!   1. Turn-start fold (90%) — before first API call, catches terminal
//!      prior turn, session restore, huge user paste
//!   2. Post-response fold (75%) — normal growth → fold into summary
//!   3. Aggressive fold (78%) — normal fold didn't buy enough headroom
//!      → use half the tail budget
//!   4. Exit-with-summary (80%) — defense in depth: force final summary
//!      and end the turn
//!   5. Min-savings check (30%) — skip fold if head wouldn't shrink log
//!      enough
//!
//! Each threshold is a fraction of the model's context window
//! (`ctx_max`). The decision is made against `prompt_tokens` from
//! the API usage response, or a local estimate before the call.

use serde::Serialize;

// ================================================================
// Threshold constants — port of context-manager.ts:27-43
// ================================================================

/// Auto-fold when a turn's response shows promptTokens above
/// this fraction of ctxMax.
pub const HISTORY_FOLD_THRESHOLD: f64 = 0.75;

/// Tail budget after a normal fold, as a fraction of ctxMax.
pub const HISTORY_FOLD_TAIL_FRACTION: f64 = 0.2;

/// Above this fraction the normal fold's tail budget didn't
/// buy enough headroom — fold harder.
pub const HISTORY_FOLD_AGGRESSIVE_THRESHOLD: f64 = 0.78;

/// Tail budget after an aggressive fold — half the normal one,
/// sacrifices recent context for headroom.
pub const HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION: f64 = 0.1;

/// Skip the fold if the head wouldn't shrink the log by at
/// least this fraction.
pub const HISTORY_FOLD_MIN_SAVINGS_FRACTION: f64 = 0.3;

/// Above this fraction we exit the turn with a summary instead
/// of folding (defense in depth).
pub const FORCE_SUMMARY_THRESHOLD: f64 = 0.8;

/// Turn-start local estimate above this fraction triggers a
/// pre-iter fold. Covers cases the post-response fold can't
/// (terminal prior turn, fresh session restore, huge user
/// paste).
pub const TURN_START_FOLD_THRESHOLD: f64 = 0.9;

/// Approximate characters-per-token ratio for estimation when
/// actual token counts aren't available. Used by both run.rs's
/// turn-start heuristic and Session::estimate_message_tokens.
pub const CHARS_PER_TOKEN_ESTIMATE: u64 = 4;

// ================================================================
// Data types — port of context-manager.ts:67-85
// ================================================================

/// What action the context manager recommends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PostUsageDecisionKind {
    /// Context is within healthy limits — carry on.
    None,
    /// Fold older messages into a summary; keep the tail.
    Fold,
    /// Exceeded even the exit-with-summary threshold — force
    /// a final summary before ending the turn.
    ExitWithSummary,
}

/// Decision after a turn's response.
#[derive(Debug, Clone, Copy)]
pub struct PostUsageDecision {
    pub kind: PostUsageDecisionKind,
    pub prompt_tokens: u64,
    pub ctx_max: u64,
    pub ratio: f64,
    /// Token budget for the recent tail when kind is Fold.
    /// Smaller in the aggressive band.
    pub tail_budget: Option<u64>,
    /// True when this fold is in the aggressive band (78%-80%).
    pub aggressive: bool,
}

/// Turn-start estimate result.
#[derive(Debug, Clone, Copy)]
pub struct TurnStartEstimate {
    pub estimate_tokens: u64,
    pub ctx_max: u64,
    pub ratio: f64,
}

// ================================================================
// Decision logic — port of context-manager.ts:134-177
// ================================================================

/// Decide what to do after a turn's response — fold, exit with
/// summary, or carry on. Port of `ContextManager.decideAfterUsage`
/// (context-manager.ts:134-165).
///
/// `prompt_tokens`: the prompt_tokens value from the API usage
///   response. If `None`, the decision is `None` (no usage data).
/// `ctx_max`: the model's context window size in tokens.
/// `already_folded_this_turn`: true if we already folded earlier
///   in this turn (prevents double-fold).
pub fn decide_after_usage(
    prompt_tokens: Option<u64>,
    ctx_max: u64,
    already_folded_this_turn: bool,
) -> PostUsageDecision {
    let Some(prompt_tokens) = prompt_tokens else {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens: 0,
            ctx_max,
            ratio: 0.0,
            tail_budget: None,
            aggressive: false,
        };
    };
    if ctx_max == 0 {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens,
            ctx_max,
            ratio: 0.0,
            tail_budget: None,
            aggressive: false,
        };
    }
    let ratio = prompt_tokens as f64 / ctx_max as f64;

    if ratio > FORCE_SUMMARY_THRESHOLD {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::ExitWithSummary,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: None,
            aggressive: false,
        };
    }

    if already_folded_this_turn {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::None,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: None,
            aggressive: false,
        };
    }

    if ratio > HISTORY_FOLD_AGGRESSIVE_THRESHOLD {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::Fold,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: Some((ctx_max as f64 * HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION) as u64),
            aggressive: true,
        };
    }

    if ratio > HISTORY_FOLD_THRESHOLD {
        return PostUsageDecision {
            kind: PostUsageDecisionKind::Fold,
            prompt_tokens,
            ctx_max,
            ratio,
            tail_budget: Some((ctx_max as f64 * HISTORY_FOLD_TAIL_FRACTION) as u64),
            aggressive: false,
        };
    }

    PostUsageDecision {
        kind: PostUsageDecisionKind::None,
        prompt_tokens,
        ctx_max,
        ratio,
        tail_budget: None,
        aggressive: false,
    }
}

/// Turn-start estimate vs ctxMax. Caller folds if the ratio
/// crosses TURN_START_FOLD_THRESHOLD. Port of
/// `ContextManager.estimateTurnStart`
/// (context-manager.ts:167-177).
///
/// `estimate_tokens`: a local estimate of total request tokens
///   (messages + tools + system prompt).
/// `ctx_max`: the model's context window size in tokens.
pub fn estimate_turn_start(estimate_tokens: u64, ctx_max: u64) -> TurnStartEstimate {
    let ratio = if ctx_max == 0 {
        f64::INFINITY
    } else {
        estimate_tokens as f64 / ctx_max as f64
    };
    TurnStartEstimate {
        estimate_tokens,
        ctx_max,
        ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // decide_after_usage
    // ============================================================

    #[test]
    fn no_usage_data_returns_none() {
        let d = decide_after_usage(None, 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
        assert_eq!(d.ratio, 0.0);
    }

    #[test]
    fn below_threshold_returns_none() {
        // 50K out of 128K = ~39% → below 75% threshold
        let d = decide_after_usage(Some(50_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    #[test]
    fn above_75pct_triggers_fold() {
        // 98K out of 128K = ~76.5% → above 75%, below 78%
        let d = decide_after_usage(Some(98_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::Fold);
        assert!(!d.aggressive);
        // Tail budget: 20% of 128K = 25600
        assert_eq!(d.tail_budget, Some(25600));
    }

    #[test]
    fn above_78pct_triggers_aggressive_fold() {
        // 101K out of 128K = ~78.9% → above 78%
        let d = decide_after_usage(Some(101_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::Fold);
        assert!(d.aggressive);
        // Aggressive tail budget: 10% of 128K = 12800
        assert_eq!(d.tail_budget, Some(12800));
    }

    #[test]
    fn above_80pct_triggers_exit_with_summary() {
        // 105K out of 128K = ~82% → above 80%
        let d = decide_after_usage(Some(105_000), 128_000, false);
        assert_eq!(d.kind, PostUsageDecisionKind::ExitWithSummary);
    }

    #[test]
    fn already_folded_prevents_double_fold() {
        // Even though ratio is above 75%, we don't fold again
        let d = decide_after_usage(Some(100_000), 128_000, true);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    #[test]
    fn already_folded_does_not_prevent_exit_with_summary() {
        // Above 80% still triggers exit even if already folded
        let d = decide_after_usage(Some(105_000), 128_000, true);
        assert_eq!(d.kind, PostUsageDecisionKind::ExitWithSummary);
    }

    #[test]
    fn zero_ctx_max_handled_gracefully() {
        // ctx_max == 0 is degenerate (unknown model, config error).
        // Guard returns None rather than computing inf/NaN ratio.
        let d = decide_after_usage(Some(1000), 0, false);
        assert_eq!(d.kind, PostUsageDecisionKind::None);
    }

    // ============================================================
    // estimate_turn_start
    // ============================================================

    #[test]
    fn estimate_below_threshold() {
        let e = estimate_turn_start(50_000, 128_000);
        assert!(e.ratio < TURN_START_FOLD_THRESHOLD);
        assert_eq!(e.ctx_max, 128_000);
    }

    #[test]
    fn estimate_above_threshold() {
        let e = estimate_turn_start(120_000, 128_000);
        assert!(e.ratio > TURN_START_FOLD_THRESHOLD);
    }

    #[test]
    fn estimate_at_boundary() {
        let boundary = (128_000.0 * TURN_START_FOLD_THRESHOLD) as u64;
        let e = estimate_turn_start(boundary, 128_000);
        // At exactly the threshold — caller decides whether to fold
        assert!((e.ratio - TURN_START_FOLD_THRESHOLD).abs() < 0.001);
    }

    // ============================================================
    // Threshold constant sanity
    // ============================================================

    #[test]
    fn thresholds_are_strictly_ordered() {
        assert!(FORCE_SUMMARY_THRESHOLD > HISTORY_FOLD_AGGRESSIVE_THRESHOLD);
        assert!(HISTORY_FOLD_AGGRESSIVE_THRESHOLD > HISTORY_FOLD_THRESHOLD);
        assert!(HISTORY_FOLD_THRESHOLD > HISTORY_FOLD_MIN_SAVINGS_FRACTION);
    }

    #[test]
    fn aggressive_tail_is_smaller_than_normal_tail() {
        assert!(HISTORY_FOLD_AGGRESSIVE_TAIL_FRACTION < HISTORY_FOLD_TAIL_FRACTION);
    }
}
