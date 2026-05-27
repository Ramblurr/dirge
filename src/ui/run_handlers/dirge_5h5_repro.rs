//! dirge-5h5 reproduction harness.
//!
//! Isolated repro for the "first 6 chambers show TOP+BOTTOM only,
//! LAST chamber has full content" bug on N parallel reads. Drives the
//! chamber state machine directly — same `Renderer` + same
//! `handle_tool_result` the production path uses — but bypasses the
//! tokio::select! / bridge / agent_loop layers so we can isolate the
//! chamber logic from any concurrency-layer interactions.
//!
//! Each test:
//!   1. Constructs a fresh Renderer + RunCtx scaffolding.
//!   2. Simulates N ToolCall events by inlining the same chamber-TOP
//!      paint logic from `ui/mod.rs::AgentEvent::ToolCall`.
//!   3. Simulates N ToolResult events by calling the production
//!      `handle_tool_result` in some order.
//!   4. Walks `renderer.buffer_lines()` and asserts every TOP has at
//!      least one body row before its matching BOTTOM.
//!
//! Run individually with:
//!   cargo test --features "<features>" --bin dirge -- dirge_5h5_repro
//!
//! Re-run with tracing on the production trace target to see the
//! per-event chamber state:
//!   RUST_LOG=dirge::ui::chamber=trace cargo test … -- --nocapture

use crossterm::style::Color;

use crate::cli::Cli;
use crate::config::Config;
use crate::session::Session;
use crate::ui::colors::c_tool;
use crate::ui::events::sanitize_output;
use crate::ui::renderer::Renderer;
use crate::ui::tool_display::{
    chamber_widths, close_tool_chamber_passive, fit_banner_header, format_tool_banner_value,
};

use super::RunCtx;
use super::tool_result::handle_tool_result;

/// Mirrors the inline ToolCall handling in `ui/mod.rs:1713+`. Kept in
/// sync with that path — if production logic drifts, update here.
fn simulate_tool_call(ctx: &mut RunCtx<'_>, id: &str, name: &str, args: serde_json::Value) {
    // Push to tool_calls_buf — production code does this too.
    ctx.tool_calls_buf.push(crate::session::ToolCallEntry {
        id: id.to_string(),
        name: name.to_string(),
        args: args.clone(),
        state: crate::session::ToolCallState::Interrupted,
    });
    *ctx.tool_calls_this_run = ctx.tool_calls_this_run.saturating_add(1);

    // Close any prior chamber via the passive path.
    close_tool_chamber_passive(
        ctx.renderer,
        ctx.last_tool_name,
        ctx.tool_chamber_open,
        ctx.chamber_top_start,
        ctx.chamber_top_end,
    )
    .expect("close passive");

    *ctx.last_tool_name = Some(name.to_string());
    *ctx.last_tool_call_id = Some(id.to_string());

    // Paint the chamber TOP: spacer + header.
    *ctx.chamber_top_start = Some(ctx.renderer.buffer_len());
    ctx.renderer.write_line("", Color::White).expect("spacer");
    let upper = name.to_ascii_uppercase();
    let raw_value = format_tool_banner_value(name, &args);
    let raw_value = sanitize_output(&raw_value).into_string();
    let (frame_w, _) = chamber_widths(ctx.renderer);
    let header = fit_banner_header(&upper, &raw_value, frame_w);
    ctx.renderer.write_line(&header, c_tool()).expect("header");
    *ctx.chamber_top_end = Some(ctx.renderer.buffer_len());
    *ctx.tool_chamber_open = true;
}

/// One chamber's slice of the rendered buffer: the TOP banner, all
/// rows between it and the next BOTTOM, and the BOTTOM itself.
#[derive(Debug)]
struct Chamber {
    /// Buffer line index of the `╭─ NAME …` row.
    top_idx: usize,
    /// Buffer line index of the closing `╰─…` row.
    bottom_idx: usize,
    /// Number of body rows between TOP and BOTTOM (exclusive of both).
    body_rows: usize,
    /// The tool name extracted from the TOP banner ("READ", "WRITE", …).
    name: String,
    /// First body row's content, if any (for debugging the failure
    /// case — keep even though Debug derives it implicitly).
    #[allow(dead_code)]
    first_body: Option<String>,
}

/// Walk the buffer, pair every TOP with its next BOTTOM, count body
/// rows. Returns one entry per chamber in buffer order.
fn collect_chambers(renderer: &Renderer) -> Vec<Chamber> {
    let lines: Vec<String> = renderer
        .buffer_lines()
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with('\u{256d}') {
            // `╭` — TOP row. Find next BOTTOM.
            let name = extract_banner_name(&lines[i]);
            let top_idx = i;
            let mut j = i + 1;
            while j < lines.len() && !lines[j].starts_with('\u{2570}') {
                j += 1;
            }
            if j < lines.len() {
                // Found BOTTOM. Body rows are i+1..j (exclusive of both).
                let body_rows = j.saturating_sub(top_idx).saturating_sub(1);
                let first_body = lines.get(top_idx + 1).cloned();
                out.push(Chamber {
                    top_idx,
                    bottom_idx: j,
                    body_rows,
                    name,
                    first_body,
                });
                i = j + 1;
            } else {
                // Unclosed chamber — still record so the assertion
                // surfaces it clearly.
                out.push(Chamber {
                    top_idx,
                    bottom_idx: usize::MAX,
                    body_rows: 0,
                    name,
                    first_body: None,
                });
                break;
            }
        } else {
            i += 1;
        }
    }
    out
}

fn extract_banner_name(top_line: &str) -> String {
    // Banner shape: `╭─ NAME ─ value ─...─╮` — split on `─` and take
    // the first non-empty trimmed token after the leading `╭─`.
    top_line
        .split('\u{2500}') // `─`
        .map(|s| s.trim())
        .find(|s| !s.is_empty() && !s.starts_with('\u{256d}'))
        .unwrap_or("?")
        .to_string()
}

/// Build a fresh (Cli, Config, Session, Renderer) tuple suitable for
/// driving RunCtx. Returns owned values so the test can build the
/// RunCtx with &mut refs.
fn fresh_scaffold() -> (Cli, Config, Session, Renderer) {
    use clap::Parser;
    let cli = Cli::parse_from::<_, &str>(["dirge"]);
    let cfg = Config::default();
    let session = Session::new("test-provider", "test-model", 200_000);
    let renderer = Renderer::new().expect("renderer");
    (cli, cfg, session, renderer)
}

/// Build a RunCtx<'_> from already-mutable scaffolding pieces.
/// Returns the RunCtx by-value (lifetime tied to the borrows).
#[allow(clippy::too_many_arguments)]
fn make_ctx<'a>(
    renderer: &'a mut Renderer,
    session: &'a mut Session,
    state: &'a mut State,
    cli: &'a Cli,
    cfg: &'a Config,
) -> RunCtx<'a> {
    RunCtx {
        renderer,
        session,
        response_buf: &mut state.response_buf,
        response_start_line: &mut state.response_start_line,
        reasoning_buf: &mut state.reasoning_buf,
        reasoning_start_line: &mut state.reasoning_start_line,
        agent_line_started: &mut state.agent_line_started,
        last_tool_name: &mut state.last_tool_name,
        last_tool_call_id: &mut state.last_tool_call_id,
        tool_chamber_open: &mut state.tool_chamber_open,
        chamber_top_start: &mut state.chamber_top_start,
        chamber_top_end: &mut state.chamber_top_end,
        tool_calls_buf: &mut state.tool_calls_buf,
        tool_calls_this_run: &mut state.tool_calls_this_run,
        last_collapsed: &mut state.last_collapsed,
        last_user_prompt: &mut state.last_user_prompt,
        cli,
        cfg,
    }
}

/// Free-standing State struct mirrored from the macro — kept as a
/// real type so `make_ctx` can take `&mut State`.
struct State {
    response_buf: String,
    response_start_line: Option<usize>,
    reasoning_buf: String,
    reasoning_start_line: Option<usize>,
    agent_line_started: bool,
    last_tool_name: Option<String>,
    last_tool_call_id: Option<String>,
    tool_chamber_open: bool,
    chamber_top_start: Option<usize>,
    chamber_top_end: Option<usize>,
    tool_calls_buf: Vec<crate::session::ToolCallEntry>,
    tool_calls_this_run: u32,
    last_collapsed: Option<crate::ui::tool_display::CollapsedToolResult>,
    last_user_prompt: String,
}

impl State {
    fn new() -> Self {
        Self {
            response_buf: String::new(),
            response_start_line: None,
            reasoning_buf: String::new(),
            reasoning_start_line: None,
            agent_line_started: false,
            last_tool_name: None,
            last_tool_call_id: None,
            tool_chamber_open: false,
            chamber_top_start: None,
            chamber_top_end: None,
            tool_calls_buf: Vec::new(),
            tool_calls_this_run: 0,
            last_collapsed: None,
            last_user_prompt: String::new(),
        }
    }
}

/// Fire N ToolCall events and N ToolResult events in some `result_order`
/// permutation. Returns the rendered chambers. Bodies are
/// `BODY_TEMPLATE.replace("{i}", &i.to_string())` so each chamber has
/// unique, non-empty content.
async fn drive(n: usize, result_order: Vec<usize>) -> Vec<Chamber> {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();

    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    // Fire ToolCalls in dispatch order: c0, c1, ..., c(n-1).
    for i in 0..n {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
    }

    // Fire ToolResults in `result_order`. Body is "file {i} body line one\nfile {i} body line two".
    for i in result_order {
        let id = format!("call-{i}");
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id, body)
            .await
            .expect("handle_tool_result");
    }

    drop(ctx);
    collect_chambers(&renderer)
}

fn assert_all_chambers_have_body(chambers: &[Chamber], expected_count: usize) {
    assert_eq!(
        chambers.len(),
        expected_count,
        "expected {expected_count} chambers, got {}: {chambers:#?}",
        chambers.len()
    );
    let mut empties = Vec::new();
    for (i, c) in chambers.iter().enumerate() {
        if c.bottom_idx == usize::MAX {
            empties.push(format!(
                "chamber {i} ({}): UNCLOSED — TOP at line {} has no matching BOTTOM",
                c.name, c.top_idx
            ));
        } else if c.body_rows == 0 {
            empties.push(format!(
                "chamber {i} ({}): TOP+BOTTOM only — no body rows between lines {} and {}",
                c.name, c.top_idx, c.bottom_idx
            ));
        }
    }
    if !empties.is_empty() {
        panic!(
            "dirge-5h5 reproduced: {}/{} chambers have no body. Details:\n  {}",
            empties.len(),
            chambers.len(),
            empties.join("\n  ")
        );
    }
}

// ============================================================
// Scenarios
// ============================================================

/// Scenario A: 7 ToolCalls, 7 ToolResults in dispatch order.
/// Baseline — exercises the "id matches last_tool_call_id" path on
/// the FIRST result (because last_tool_call_id is the latest call's
/// id from the last ToolCall in the burst), and the dirge-jzj fresh-
/// chamber path on the rest.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_dispatch_order() {
    let chambers = drive(7, (0..7).collect()).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario B: 7 ToolCalls, 7 ToolResults in REVERSE order. The
/// FIRST result (c6) matches the last_tool_call_id (also c6 from
/// the last ToolCall in the burst). All subsequent results go
/// through the dirge-jzj path.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_reverse_order() {
    let chambers = drive(7, (0..7).rev().collect()).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario C: 7 ToolCalls, 7 ToolResults in a SCRAMBLED order. Tests
/// the case where the first result is neither the dispatch-first nor
/// dispatch-last — it ALWAYS hits the dirge-jzj path because the id
/// won't match last_tool_call_id.
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_scrambled_order() {
    // Picked once, deterministic — no PRNG to keep CI repeatable.
    let order = vec![3, 0, 6, 1, 5, 2, 4];
    let chambers = drive(7, order).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario D: 2 ToolCalls, 2 ToolResults — minimum case that
/// exercises both paths (one id-match, one dirge-jzj). Useful for
/// localising the bug if only the larger N reproduces.
#[tokio::test]
async fn dirge_5h5_repro_two_parallel_reverse_order() {
    let chambers = drive(2, vec![0, 1]).await;
    assert_all_chambers_have_body(&chambers, 2);
}

/// Scenario E: 7 ToolCalls, 7 ToolResults, but the LAST result is
/// the one matching last_tool_call_id. Mirrors the user's report
/// shape (which seems to suggest the LAST chamber is the one that
/// rendered correctly).
#[tokio::test]
async fn dirge_5h5_repro_seven_parallel_match_arrives_last() {
    // ToolCall order: c0..c6 → last_tool_call_id = c6 after burst.
    // Result order: 0, 1, 2, 3, 4, 5, 6 — so c6 (the matching id)
    // arrives LAST. The first 6 (c0..c5) all go through dirge-jzj.
    let chambers = drive(7, vec![0, 1, 2, 3, 4, 5, 6]).await;
    assert_all_chambers_have_body(&chambers, 7);
}

/// Scenario F: Interleaved — alternate ToolCall and ToolResult.
/// Mirrors the SEQUENTIAL dispatch shape (one call, one result, etc.)
/// — confirms the chamber state machine handles that path cleanly.
/// Should always pass; if it fails we know the bug isn't parallel-
/// specific.
#[tokio::test]
async fn dirge_5h5_repro_interleaved_baseline() {
    let (cli, cfg, mut session, mut renderer) = fresh_scaffold();
    let mut state = State::new();
    let mut ctx = make_ctx(&mut renderer, &mut session, &mut state, &cli, &cfg);

    for i in 0..7 {
        let id = format!("call-{i}");
        simulate_tool_call(
            &mut ctx,
            &id,
            "read",
            serde_json::json!({"path": format!("/tmp/file{i}.txt")}),
        );
        let body = format!("file {i} body line one\nfile {i} body line two");
        handle_tool_result(&mut ctx, id.clone(), body)
            .await
            .expect("handle_tool_result");
    }
    drop(ctx);
    let chambers = collect_chambers(&renderer);
    assert_all_chambers_have_body(&chambers, 7);
}
