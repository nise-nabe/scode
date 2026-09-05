//! Fixture smoke tests for CLI-facing index flows.

use scode::corpus::{CorpusInput, TokenMode};
use scode::index::{build_from_docs, build_index};
use scode::SourceDoc;
use std::path::PathBuf;

#[test]
fn fixture_tree_idents_httpclient() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/demo");
    let idx =
        build_index(&CorpusInput::detect(&root), TokenMode::Idents, false, None, None, None)
            .unwrap();
    let hits = idx.search("HttpClient", None);
    // Foo.java field + ctor param + Bar.java param = 3 (idents; comments/strings excluded)
    assert_eq!(hits.len(), 3, "hits={hits:?}");
    assert!(hits
        .iter()
        .all(|h| h.path.ends_with("Foo.java") || h.path.ends_with("Bar.java")));
    assert_eq!(idx.search("CloseableHttpClient", None).len(), 1);
}

#[test]
fn fixture_tree_all_includes_comment_string() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/demo");
    let idx =
        build_index(&CorpusInput::detect(&root), TokenMode::All, false, None, None, None).unwrap();
    let hits = idx.search("HttpClient", None);
    // idents 3 + javadoc + line comment + string literal in Foo.java
    assert!(hits.len() >= 5, "expected all-mode to find comment/string hits, got {}", hits.len());
}

#[test]
fn search_multi_tags() {
    let docs = vec![SourceDoc {
        gav: "g:a:1".into(),
        path: "A.java".into(),
        text: "class A { HttpClient x; Foo y; }\n".into(),
    }];
    let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
    let res = idx.search_multi(&["HttpClient".into(), "Foo".into()], None, None);
    assert_eq!(res.hits.len(), 2);
    assert!(res.hits.iter().any(|h| h.matched_queries.contains(&"Foo".to_string())));
}