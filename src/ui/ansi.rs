//! Shared ANSI / control-byte handling.
//!
//! Three things show up across the UI layer:
//!   1. Sanitizing text from untrusted producers (tool output, MCP
//!      server stderr, websearch results) before it reaches the
//!      chat buffer.
//!   2. Computing the visible width of strings that may embed SGR
//!      escapes (lives in `wrap::visible_width`).
//!   3. Building SGR colour sequences (lives in `markdown::ansi_fg`).
//!
//! Centralising (1) here means MCP / websearch / tool-output / chat
//! sanitization share one definition of "what's a control byte" —
//! previously each had its own filter, drifting in coverage (e.g.
//! one blocked C0 but not C1, another stripped `\r` but not C1
//! either).
//!
//! Threat model: a child process / search result / tool response
//! must not be able to steer the terminal (set color, move cursor,
//! disable mouse mode, switch alt screen, run OSC bell/notification,
//! emit DCS sequences). All known escape-introducer codepoints get
//! filtered:
//!   - C0 controls (U+0000..=U+001F) — including ESC (U+001B)
//!   - DEL (U+007F)
//!   - C1 controls (U+0080..=U+009F) — single-byte CSI / OSC / DCS
//!     in 8-bit terminals
//!
//! Newline and tab are SEPARATE knobs because some consumers want
//! to preserve them (chat markdown), others don't (chamber rows,
//! single-line banners).

use compact_str::CompactString;

/// What whitespace-class controls to preserve. The "block all"
/// posture is the safe default; consumers that need newline /
/// tab pass-through opt in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StripPolicy {
    /// Preserve U+000A (LF). Most chat consumers want this so
    /// multi-line content renders as separate rows.
    pub keep_newline: bool,
    /// Preserve U+0009 (TAB). Chamber rows expand tabs to spaces
    /// before this point; banners + single-line UIs leave tabs as
    /// space-equivalent and want them stripped (collapse to space).
    pub keep_tab: bool,
}

impl StripPolicy {
    /// Block everything; collapses to plain ASCII / non-control
    /// Unicode. Use for single-line banners, alert rows, MCP log
    /// lines where the rendering layer wraps after we return.
    pub const STRICT: Self = Self {
        keep_newline: false,
        keep_tab: false,
    };

    /// Preserve `\n` for multi-line consumers. Still strips ESC,
    /// CR, DEL, C1. Use for chat text / tool output that the
    /// renderer splits on `\n` itself.
    pub const KEEP_NEWLINE: Self = Self {
        keep_newline: true,
        keep_tab: false,
    };

    /// Preserve both `\n` and `\t`. Use for chat content that
    /// flows through markdown rendering (tabs survive into the
    /// rendered code-block).
    #[allow(dead_code)]
    pub const KEEP_BOTH: Self = Self {
        keep_newline: true,
        keep_tab: true,
    };
}

/// Strip control bytes from `s` according to `policy`. Review #14:
/// fast-path returns the input unchanged when no chars would be
/// filtered, avoiding the `chars().filter().collect()` allocation
/// for the common case (most MCP log lines / chat tokens have no
/// control bytes to strip).
pub fn strip_controls(s: &str, policy: StripPolicy) -> String {
    if s.chars().all(|c| keep_char(c, policy)) {
        return s.to_string();
    }
    s.chars().filter(|c| keep_char(*c, policy)).collect()
}

/// Same as `strip_controls` but returns a `CompactString` — used
/// by `sanitize_output` callers that store the result in chamber
/// row buffers. Currently unused at call sites (the MCP forwarder
/// is the highest-throughput caller and uses `strip_controls`);
/// kept as part of the deliberate API surface for the future
/// `sanitize_output` migration.
#[allow(dead_code)]
pub fn strip_controls_compact(s: &str, policy: StripPolicy) -> CompactString {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if keep_char(c, policy) {
            out.push(c);
        }
    }
    CompactString::from(out)
}

fn keep_char(c: char, policy: StripPolicy) -> bool {
    let cp = c as u32;
    if cp == 0x0A {
        return policy.keep_newline;
    }
    if cp == 0x09 {
        return policy.keep_tab;
    }
    // Block C0 controls, DEL, and C1 controls.
    if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_blocks_all_controls() {
        let s = "hello\x1b[31m world\u{9b}\x07\x00\t\n!";
        let out = strip_controls(s, StripPolicy::STRICT);
        assert_eq!(out, "hello[31m world!");
    }

    #[test]
    fn keep_newline_preserves_lf_only() {
        let s = "line1\nline2\x1b[31m\tend";
        let out = strip_controls(s, StripPolicy::KEEP_NEWLINE);
        assert_eq!(out, "line1\nline2[31mend");
    }

    #[test]
    fn keep_both_preserves_lf_and_tab() {
        let s = "a\tb\nc\x1b[0md";
        let out = strip_controls(s, StripPolicy::KEEP_BOTH);
        assert_eq!(out, "a\tb\nc[0md");
    }

    #[test]
    fn c1_csi_blocked() {
        // U+009B is single-byte CSI — must NOT survive any policy.
        let s = "before\u{9b}5;31mafter";
        for policy in [
            StripPolicy::STRICT,
            StripPolicy::KEEP_NEWLINE,
            StripPolicy::KEEP_BOTH,
        ] {
            let out = strip_controls(s, policy);
            assert!(
                !out.contains('\u{9b}'),
                "C1 CSI survived policy {policy:?}: {out:?}"
            );
        }
    }

    #[test]
    fn non_ascii_letters_pass_through() {
        let s = "naïve 日本語 🚀";
        for policy in [
            StripPolicy::STRICT,
            StripPolicy::KEEP_NEWLINE,
            StripPolicy::KEEP_BOTH,
        ] {
            assert_eq!(strip_controls(s, policy), s);
        }
    }
}
