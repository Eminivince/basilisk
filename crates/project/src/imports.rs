//! Solidity `import` statement extraction.
//!
//! Pure text parser — we don't try to build an AST. Solidity supports
//! four import shapes per the language spec:
//!
//! ```solidity
//! import "foo.sol";                      // bare
//! import "foo.sol" as Foo;               // aliased file
//! import * as Foo from "foo.sol";        // wildcard
//! import {A, B as C} from "foo.sol";     // symbol list
//! ```
//!
//! Comments are stripped first (line + block) using the shared
//! [`crate::js_text::strip_comments`] helper. Solidity uses the same
//! comment syntax as JS, so reusing it keeps both parsers consistent.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{error::ProjectError, js_text};

/// One `import` statement extracted from a Solidity source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportStatement {
    /// Exactly the string between the import quotes — no resolution
    /// applied. Examples: `"./foo.sol"`, `"@oz/Ownable.sol"`,
    /// `"forge-std/Test.sol"`.
    pub raw_path: String,
    /// 1-based line number of the `import` keyword in the source.
    pub line: usize,
    pub kind: ImportKind,
}

/// The four import shapes Solidity supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportKind {
    /// `import "foo.sol";` — pulls every top-level symbol unqualified.
    Bare,
    /// `import "foo.sol" as Foo;` — file aliased to `Foo`.
    Aliased { alias: String },
    /// `import * as Foo from "foo.sol";` — wildcard alias.
    Wildcard { alias: String },
    /// `import {A, B as C} from "foo.sol";` — explicit symbol list.
    Symbols { symbols: Vec<ImportedSymbol> },
}

/// One entry inside an `import { ... }` symbol list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedSymbol {
    /// Name as it appears in the source file we're importing from.
    pub name: String,
    /// Optional `as Foo` rename in the importing file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// Read `path`, strip comments, and return every `import` statement found.
pub fn parse_imports_in_file(path: &Path) -> Result<Vec<ImportStatement>, ProjectError> {
    let source = fs::read_to_string(path).map_err(|e| ProjectError::io(path, e))?;
    Ok(parse_imports(&source))
}

/// Same as [`parse_imports_in_file`] but operates on an in-memory string.
/// Convenient for tests and embedded use.
pub fn parse_imports(source: &str) -> Vec<ImportStatement> {
    let cleaned = js_text::strip_comments(source);
    let mut out: Vec<ImportStatement> = Vec::new();

    // Order matters only for clarity in the test output: we collect
    // every kind, then sort by line number at the end.
    collect_wildcard(&cleaned, &mut out);
    collect_symbols(&cleaned, &mut out);
    collect_path(&cleaned, &mut out);

    out.sort_by_key(|s| s.line);
    out
}

/// Convenience: just the raw paths in source order.
pub fn raw_import_paths(statements: &[ImportStatement]) -> Vec<PathBuf> {
    statements
        .iter()
        .map(|s| PathBuf::from(&s.raw_path))
        .collect()
}

fn collect_wildcard(src: &str, out: &mut Vec<ImportStatement>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?ms)\bimport\s*\*\s+as\s+([A-Za-z_$][\w$]*)\s+from\s*['"]([^'"\n]+)['"]\s*;"#,
        )
        .unwrap()
    });
    for caps in re.captures_iter(src) {
        let alias = caps.get(1).unwrap().as_str().to_string();
        let raw_path = caps.get(2).unwrap().as_str().to_string();
        let line = line_of(src, caps.get(0).unwrap().start());
        out.push(ImportStatement {
            raw_path,
            line,
            kind: ImportKind::Wildcard { alias },
        });
    }
}

fn collect_symbols(src: &str, out: &mut Vec<ImportStatement>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?ms)\bimport\s*\{([^}]*)\}\s*from\s*['"]([^'"\n]+)['"]\s*;"#).unwrap()
    });
    for caps in re.captures_iter(src) {
        let symbols_blob = caps.get(1).unwrap().as_str();
        let raw_path = caps.get(2).unwrap().as_str().to_string();
        let line = line_of(src, caps.get(0).unwrap().start());
        let symbols = parse_symbol_list(symbols_blob);
        out.push(ImportStatement {
            raw_path,
            line,
            kind: ImportKind::Symbols { symbols },
        });
    }
}

/// Plain `import "foo.sol";` and `import "foo.sol" as Foo;`.
/// We exclude the wildcard / symbol shapes by requiring the next
/// non-whitespace character after `import` to be a quote.
fn collect_path(src: &str, out: &mut Vec<ImportStatement>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?ms)\bimport\s+['"]([^'"\n]+)['"](?:\s+as\s+([A-Za-z_$][\w$]*))?\s*;"#)
            .unwrap()
    });
    for caps in re.captures_iter(src) {
        let raw_path = caps.get(1).unwrap().as_str().to_string();
        let alias = caps.get(2).map(|m| m.as_str().to_string());
        let line = line_of(src, caps.get(0).unwrap().start());
        let kind = match alias {
            Some(alias) => ImportKind::Aliased { alias },
            None => ImportKind::Bare,
        };
        out.push(ImportStatement {
            raw_path,
            line,
            kind,
        });
    }
}

fn parse_symbol_list(blob: &str) -> Vec<ImportedSymbol> {
    blob.split(',')
        .filter_map(|chunk| {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                return None;
            }
            // `Name` or `Name as Alias`.
            let mut parts = chunk.split_whitespace();
            let name = parts.next()?.to_string();
            let alias = match (parts.next(), parts.next()) {
                (Some("as"), Some(alias)) => Some(alias.to_string()),
                _ => None,
            };
            Some(ImportedSymbol { name, alias })
        })
        .collect()
}

fn line_of(src: &str, byte_offset: usize) -> usize {
    // 1-based line number of `byte_offset` in `src`.
    src[..byte_offset.min(src.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_import() {
        let src = "import \"./foo.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].raw_path, "./foo.sol");
        assert_eq!(imports[0].line, 1);
        assert!(matches!(imports[0].kind, ImportKind::Bare));
    }

    #[test]
    fn parses_aliased_path_import() {
        let src = "import \"./foo.sol\" as Foo;\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        match &imports[0].kind {
            ImportKind::Aliased { alias } => assert_eq!(alias, "Foo"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parses_wildcard_import() {
        let src = "import * as Foo from \"./foo.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        match &imports[0].kind {
            ImportKind::Wildcard { alias } => assert_eq!(alias, "Foo"),
            other => panic!("got {other:?}"),
        }
        assert_eq!(imports[0].raw_path, "./foo.sol");
    }

    #[test]
    fn parses_symbol_list() {
        let src = "import {A, B as C, D} from \"./foo.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        match &imports[0].kind {
            ImportKind::Symbols { symbols } => {
                assert_eq!(symbols.len(), 3);
                assert_eq!(symbols[0].name, "A");
                assert!(symbols[0].alias.is_none());
                assert_eq!(symbols[1].name, "B");
                assert_eq!(symbols[1].alias.as_deref(), Some("C"));
                assert_eq!(symbols[2].name, "D");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parses_remapped_paths() {
        let src = r#"
import "@openzeppelin/contracts/access/Ownable.sol";
import {Test} from "forge-std/Test.sol";
"#;
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 2);
        assert_eq!(
            imports[0].raw_path,
            "@openzeppelin/contracts/access/Ownable.sol"
        );
        assert_eq!(imports[1].raw_path, "forge-std/Test.sol");
    }

    #[test]
    fn returns_imports_in_source_order_with_line_numbers() {
        let src = "// SPDX-License-Identifier: MIT\n\
                   pragma solidity ^0.8.0;\n\
                   \n\
                   import \"./a.sol\";\n\
                   import {B} from \"./b.sol\";\n\
                   import * as C from \"./c.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 3);
        assert_eq!(imports[0].raw_path, "./a.sol");
        assert_eq!(imports[0].line, 4);
        assert_eq!(imports[1].raw_path, "./b.sol");
        assert_eq!(imports[1].line, 5);
        assert_eq!(imports[2].raw_path, "./c.sol");
        assert_eq!(imports[2].line, 6);
    }

    #[test]
    fn line_comments_hide_imports() {
        let src = "// import \"./hidden.sol\";\nimport \"./visible.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].raw_path, "./visible.sol");
    }

    #[test]
    fn block_comments_hide_imports_and_preserve_lines() {
        let src = "/* import \"./hidden.sol\";\n   still inside\n*/\n\
                   import \"./visible.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].raw_path, "./visible.sol");
        // Line 4 because the block comment preserves its three newlines.
        assert_eq!(imports[0].line, 4);
    }

    #[test]
    fn import_keyword_inside_identifier_is_not_matched() {
        // `\b` word-boundary in the regex prevents `myImport` from matching.
        let src = "uint myImport = 0;\nimport \"./real.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].raw_path, "./real.sol");
    }

    #[test]
    fn single_quoted_paths_are_supported() {
        let src = "import './foo.sol';\nimport {X} from './bar.sol';\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].raw_path, "./foo.sol");
        assert_eq!(imports[1].raw_path, "./bar.sol");
    }

    #[test]
    fn no_imports_yields_empty_vec() {
        let src = "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.0;\ncontract A {}\n";
        assert!(parse_imports(src).is_empty());
    }

    #[test]
    fn raw_import_paths_helper_returns_pathbufs_in_order() {
        let src = "import \"./a.sol\";\nimport {X} from \"./b.sol\";\n";
        let imports = parse_imports(src);
        let paths = raw_import_paths(&imports);
        assert_eq!(
            paths,
            vec![PathBuf::from("./a.sol"), PathBuf::from("./b.sol")]
        );
    }

    #[test]
    fn empty_symbol_list_is_handled_gracefully() {
        // `import {} from "x";` is a valid (though pointless) parse.
        let src = "import {} from \"./foo.sol\";\n";
        let imports = parse_imports(src);
        assert_eq!(imports.len(), 1);
        match &imports[0].kind {
            ImportKind::Symbols { symbols } => assert!(symbols.is_empty()),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parse_imports_in_file_reads_from_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("A.sol");
        fs::write(&p, "import \"./B.sol\";\n").unwrap();
        let imports = parse_imports_in_file(&p).unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].raw_path, "./B.sol");
    }
}
