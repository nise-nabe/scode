//! Java / Kotlin lexical identifier extraction.

use serde::{Deserialize, Serialize};

/// Token retention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenMode {
    /// Lexer identifiers outside comments/strings.
    Idents,
    /// Identifier-shaped tokens across the whole file (comments/strings included).
    All,
}

impl TokenMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenMode::Idents => "idents",
            TokenMode::All => "all",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "idents" | "ident" => Ok(TokenMode::Idents),
            "all" => Ok(TokenMode::All),
            other => anyhow::bail!("unknown token-mode `{other}` (expected idents|all)"),
        }
    }
}

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

/// Java/Kotlin-style identifier start: Unicode letter, `_`, or `$`.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

/// Java/Kotlin-style identifier continue: letter, digit, `_`, or `$`.
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn tokenize_all(source: &str) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut line_start = 0usize;
    let mut chars = source.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '\n' {
            line += 1;
            line_start = i + c.len_utf8();
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            let mut end = i + c.len_utf8();
            while let Some(&(ni, nc)) = chars.peek() {
                if is_ident_continue(nc) {
                    end = ni + nc.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let col = (start - line_start) as u32 + 1;
            out.push(Occurrence {
                name: source[start..end].to_string(),
                line,
                col,
            });
            continue;
        }
    }
    out
}

fn tokenize_idents(source: &str) -> Vec<Occurrence> {
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut line_start = 0usize;
    let mut chars = source.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        if c == '\n' {
            line += 1;
            line_start = i + c.len_utf8();
            continue;
        }

        if c == '/' {
            if let Some(&(_, '/')) = chars.peek() {
                chars.next();
                while let Some(&(_, nc)) = chars.peek() {
                    if nc == '\n' {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
            if let Some(&(_, '*')) = chars.peek() {
                chars.next();
                while let Some((ni, nc)) = chars.next() {
                    if nc == '\n' {
                        line += 1;
                        line_start = ni + nc.len_utf8();
                    }
                    if nc == '*'
                        && let Some(&(_, '/')) = chars.peek()
                    {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
        }

        // Text block """..."""
        if c == '"' {
            let is_text_block = matches!(chars.peek(), Some((_, '"')))
                && ({
                    let mut look = chars.clone();
                    look.next();
                    matches!(look.peek(), Some((_, '"')))
                });
            if is_text_block {
                chars.next(); // second "
                chars.next(); // third "
                while let Some((ni, nc)) = chars.next() {
                    if nc == '\n' {
                        line += 1;
                        line_start = ni + nc.len_utf8();
                    }
                    if nc == '"'
                        && let Some(&(_, '"')) = chars.peek()
                    {
                        let mut look = chars.clone();
                        look.next();
                        if matches!(look.peek(), Some((_, '"'))) {
                            chars.next();
                            chars.next();
                            break;
                        }
                    }
                }
                continue;
            }

            while let Some((ni, nc)) = chars.next() {
                if nc == '\\' {
                    chars.next();
                    continue;
                }
                if nc == '\n' {
                    line += 1;
                    line_start = ni + nc.len_utf8();
                    continue;
                }
                if nc == '"' {
                    break;
                }
            }
            continue;
        }

        if c == '\'' {
            while let Some((ni, nc)) = chars.next() {
                if nc == '\\' {
                    chars.next();
                    continue;
                }
                if nc == '\'' {
                    break;
                }
                if nc == '\n' {
                    line += 1;
                    line_start = ni + nc.len_utf8();
                }
            }
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            let mut end = i + c.len_utf8();
            while let Some(&(ni, nc)) = chars.peek() {
                if is_ident_continue(nc) {
                    end = ni + nc.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let col = (start - line_start) as u32 + 1;
            out.push(Occurrence {
                name: source[start..end].to_string(),
                line,
                col,
            });
            continue;
        }
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

    #[test]
    fn unicode_identifiers() {
        let src = "class Café { int αβγ = 1; String 名前; }\n";
        let occs = tokenize(src, TokenMode::Idents);
        let names: Vec<_> = occs.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"Café"));
        assert!(names.contains(&"αβγ"));
        assert!(names.contains(&"名前"));
    }
}
