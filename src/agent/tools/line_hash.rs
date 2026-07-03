//! Per-line content hashes for hash-anchored editing.
//!
//! Hash-anchored editing (see `edit_lines`) lets a model edit a file
//! by line *range* plus a tiny per-line content hash instead of
//! reproducing the exact old text. `read(line_hashes=true)` shows the
//! hash beside each line; the model echoes the hashes for the range
//! it wants to replace, and the edit tool recomputes them from disk
//! and rejects the edit if any line drifted. The hash is a *guard*
//! (did this line change since you read it?), not a locator — lines
//! are addressed by number, the hash only confirms.
//!
//! Why 3 hex chars: the model has to read and echo these, so they
//! must be short. 12 bits (4096 buckets) per line is cheap on tokens,
//! but a single drifted line has a ~1/4096 chance of hashing to the
//! value the model echoed, and across a multi-line range those odds
//! accumulate — a silent clobber of the exact content the guard exists
//! to protect (dirge-w9q9).
//!
//! To buy back the safety without widening the echoed hash, each line
//! is hashed together with its PREDECESSOR. A single line drifting then
//! perturbs TWO adjacent hashes — its own (as the content) and the next
//! line's (as that line's predecessor) — so slipping a stale edit past
//! the guard needs two independent 12-bit collisions (~1/16.7M) for all
//! but a range's final line, instead of one. The coupling stays LOCAL
//! (only the immediate successor is affected), so an edit elsewhere in
//! the file is never spuriously rejected. `read` and `edit_lines` both
//! derive the predecessor from the real adjacent file line, so their
//! hashes agree.
//!
//! The function is FNV-1a (32-bit) folded to 12 bits and is stable
//! forever — it must not depend on a process-random seed, or a hash
//! shown by one `read` call wouldn't match the next.

use crate::hash::fnv32;

/// Content hash for a line, coupled to its predecessor, as exactly 3
/// lowercase hex chars.
///
/// `prev` is the preceding line's content, or `None` for the first line
/// of the file (which has no predecessor and hashes to the same value it
/// did before coupling was introduced). `line` is the line content with
/// no trailing newline; callers normalize CRLF to LF first so the same
/// logical line hashes identically regardless of on-disk line endings. A
/// `\n` joins `prev` and `line` — unambiguous because a line never
/// contains `\n` (the read tool splits on it).
pub fn line_hash(prev: Option<&str>, line: &str) -> String {
    let h = match prev {
        None => fnv32(line.as_bytes()),
        Some(p) => {
            let mut buf = Vec::with_capacity(p.len() + 1 + line.len());
            buf.extend_from_slice(p.as_bytes());
            buf.push(b'\n');
            buf.extend_from_slice(line.as_bytes());
            fnv32(&buf)
        }
    };
    // Fold to 12 bits: xor the top bits down so all input bits
    // influence the 3-char output.
    let folded = (h ^ (h >> 12) ^ (h >> 24)) & 0xfff;
    format!("{folded:03x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_three_lowercase_hex_chars() {
        for s in ["", "x", "fn main() {}", "    let y = 1;", "🦀 unicode"] {
            let h = line_hash(None, s);
            assert_eq!(h.len(), 3, "hash {h:?} for {s:?} not 3 chars");
            assert!(
                h.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
                "hash {h:?} not lowercase hex"
            );
        }
    }

    #[test]
    fn first_line_hash_output_is_locked() {
        // The first line (no predecessor) hashes exactly as it did before
        // predecessor-coupling, so these values stay part of the contract.
        for (input, expected) in [
            ("", "c8d"),
            ("x", "0bf"),
            ("fn main() {}", "8a1"),
            ("    let y = 1;", "b4d"),
            ("🦀 unicode", "64b"),
            ("let total = a + b;", "e3b"),
        ] {
            assert_eq!(line_hash(None, input), expected, "drift for {input:?}");
        }
    }

    #[test]
    fn hash_is_deterministic() {
        // Stability is the whole contract: the same (prev, line) must hash
        // the same on every call / process.
        assert_eq!(
            line_hash(Some("a"), "let total = a + b;"),
            line_hash(Some("a"), "let total = a + b;")
        );
    }

    #[test]
    fn distinct_lines_usually_differ() {
        // Not a guarantee (12-bit space), but these common cases must
        // separate or the guard is useless in practice.
        let a = line_hash(None, "    return Ok(());");
        let b = line_hash(None, "    return Err(e);");
        assert_ne!(a, b);
    }

    #[test]
    fn whitespace_is_significant() {
        // A re-indented line *did* change, so its hash must change —
        // the guard protects against editing stale content.
        assert_ne!(line_hash(None, "x = 1"), line_hash(None, "  x = 1"));
    }

    #[test]
    fn predecessor_is_part_of_the_hash() {
        // dirge-w9q9: the same line under a different predecessor hashes
        // differently — that's what perturbs the successor's hash when a
        // line drifts, and it distinguishes None (first line) from a real
        // empty predecessor.
        let line = "    let y = 1;";
        assert_ne!(
            line_hash(Some("prev-a"), line),
            line_hash(Some("prev-b"), line)
        );
        assert_ne!(line_hash(None, line), line_hash(Some(""), line));
    }
}
