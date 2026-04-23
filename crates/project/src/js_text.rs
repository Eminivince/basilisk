//! Shared text helpers for the JS/TS-adjacent config parsers.
//!
//! Hardhat and Truffle configs are JavaScript (or TypeScript). We can't
//! evaluate them in a sandboxed VM, so the best we can do is run regex
//! heuristics over the source text. To keep false positives down, we
//! strip comments first — otherwise a `// sources: "oops"` line would
//! get picked up by the same pattern that matches a real config key.

/// Remove JavaScript-style comments (`// line`, `/* block */`) from `src`.
///
/// Intentionally naïve: it does not understand strings, template
/// literals, or regex literals, so a comment-like substring inside a
/// quoted value will also be stripped. That's acceptable for our use
/// case (config files rarely embed these), and it keeps the
/// implementation predictable and dependency-free.
pub fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        // Line comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                // Preserve newlines so downstream line-numbers still make sense.
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_line_comments() {
        let s = "let x = 1; // trailing\nlet y = 2;\n// whole line\nlet z = 3;";
        let out = strip_comments(s);
        assert!(!out.contains("trailing"));
        assert!(!out.contains("whole line"));
        assert!(out.contains("let x = 1;"));
        assert!(out.contains("let z = 3;"));
    }

    #[test]
    fn strip_removes_block_comments_and_keeps_newlines() {
        let s = "a/* this\nspans\nlines */b";
        let out = strip_comments(s);
        assert!(!out.contains("spans"));
        assert_eq!(out.matches('\n').count(), 2);
        assert!(out.contains('a'));
        assert!(out.contains('b'));
    }

    #[test]
    fn strip_leaves_ordinary_slashes_alone() {
        let s = "const path = \"a/b/c\";";
        let out = strip_comments(s);
        assert_eq!(out, s);
    }

    #[test]
    fn strip_handles_unterminated_block_comment() {
        // Don't panic and don't loop forever — just eat to EOF.
        let s = "x = 1; /* no closing";
        let out = strip_comments(s);
        assert!(out.starts_with("x = 1;"));
    }
}
