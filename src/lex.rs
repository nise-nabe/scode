//! Java / Kotlin lexical identifier extraction.

use crate::corpus::TokenMode;

/// A single name occurrence in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub name: String,
    pub line: u32, // 1-based
    pub col: u32,  // 1-based byte column within line
}

/// Extract identifier occurrences from Java/Kotlin source text.
pub fn tokenize(source: &str, mode: TokenMode) -> Vec<Occurrence> {
    match mode {
        TokenMode::Idents => tokenize_idents(source),
        TokenMode::All => tokenize_all(source),
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn tokenize_all(source: &str) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;

    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\n' {
            line += 1;
            line_start = i + 1;
            i += 1;
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let name = &source[start..i];
            let col = (start - line_start) as u32 + 1;
            out.push(Occurrence {
                name: name.to_string(),
                line,
                col,
            });
            continue;
        }
        i += 1;
    }
    out
}

fn tokenize_idents(source: &str) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '\n' {
            line += 1;
            line_start = i + 1;
            i += 1;
            continue;
        }

        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Text block """..."""
        if c == '"' && i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
            i += 3;
            while i + 2 < bytes.len() {
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if c == '"' {
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch == b'\\' {
                    i += 2;
                    continue;
                }
                if ch == b'\n' {
                    line += 1;
                    line_start = i + 1;
                    i += 1;
                    continue;
                }
                if ch == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if c == '\'' {
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch == b'\\' {
                    i += 2;
                    continue;
                }
                if ch == b'\'' {
                    i += 1;
                    break;
                }
                if ch == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                i += 1;
            }
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let name = &source[start..i];
            let col = (start - line_start) as u32 + 1;
            out.push(Occurrence {
                name: name.to_string(),
                line,
                col,
            });
            continue;
        }

        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents_skip_comments_and_strings() {
        let src = r#"
            // HttpClient in comment
            class Foo {
              /** CloseableHttpClient */
              String s = "HttpClient";
              HttpClient client;
            }
        "#;
        let occs = tokenize(src, TokenMode::Idents);
        let names: Vec<_> = occs.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"HttpClient"));
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"client"));
        assert_eq!(names.iter().filter(|&&n| n == "HttpClient").count(), 1);
        assert!(!names.contains(&"CloseableHttpClient"));
    }

    #[test]
    fn all_includes_comments() {
        let src = "// HttpClient here\nclass Foo { HttpClient c; }\n";
        let occs = tokenize(src, TokenMode::All);
        let count = occs.iter().filter(|o| o.name == "HttpClient").count();
        assert_eq!(count, 2);
    }
}