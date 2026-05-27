---
name: security-guard-regex-upgrade
description: Converting substring-based security scanning to compiled regex with whitespace-evasion defeat
---
# security-guard-regex-upgrade

Converting substring-based security scanning to compiled regex with whitespace-evasion defeat.

## When to use

Any `const THREAT_PATTERNS: &[(&str, &str)]` using `content.contains()` or `content.to_lowercase().contains()` for prompt injection detection is vulnerable to whitespace-evasion. Multi-space attacks bypass substring checks looking for single-spaced phrases.

## Pattern

Replace with `LazyLock<Vec<(Regex, &str)>>`:

```rust
use regex::Regex;
use std::sync::LazyLock;

static THREAT_PATTERNS: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    vec![
        // Prompt injection — \s+ defeats whitespace-evasion
        (
            Regex::new(r"(?i)ignore\s+(previous|all|above|prior)\s+instructions").unwrap(),
            "prompt injection: role override",
        ),
        // Literal patterns stay literal:
        (Regex::new(r"\$\(curl").unwrap(), "shell command substitution"),
    ]
});

pub fn scan_content(content: &str) -> Result<(), String> {
    // 1. Check invisible unicode first (cheapest)
    for ch in INVISIBLE_CHARS {
        if content.contains(*ch) {
            return Err(format!("invisible unicode U+{:04X} detected", *ch as u32));
        }
    }
    // 2. Check regex patterns
    for (re, description) in THREAT_PATTERNS.iter() {
        if re.is_match(content) {
            return Err(format!("Security scan rejected: {}", description));
        }
    }
    Ok(())
}
```

## Invisible character set

Must match `memory_store.rs` exactly (10 chars):
`\u{200b}`, `\u{200c}`, `\u{200d}`, `\u{2060}`, `\u{fef}`, `\u{202a}`, `\u{202b}`, `\u{202c}`, `\u{202d}`, `\u{202e}`

## Pitfalls

- `scan_content()` should be a standalone function, not a method — testable without constructing full manager
- Regex patterns with `\s+` between words are more permissive but still catch injection variants that matter
- Test the legitimate-content-passes case carefully: realistic content mentioning env vars or auth tokens will match credential patterns and get blocked
- The `legitimate_skill_passes` test in guard.rs had to be revised twice because realistic prose triggered credential detection patterns