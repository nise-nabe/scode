//! Corpus ingestion: source trees, frozen GAV TOML, optional Maven fetch.

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::lex::{self, Occurrence};

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

/// One source document ready for indexing.
#[derive(Debug, Clone)]
pub struct SourceDoc {
    pub gav: String,
    pub path: String,
    pub text: String,
}

/// Input specification for indexing.
#[derive(Debug, Clone)]
pub enum CorpusInput {
    Tree(PathBuf),
    FrozenGavs(PathBuf),
}

impl CorpusInput {
    pub fn detect(path: &Path) -> Self {
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "toml")
        {
            CorpusInput::FrozenGavs(path.to_path_buf())
        } else {
            CorpusInput::Tree(path.to_path_buf())
        }
    }
}

#[derive(Debug, Deserialize)]
struct FrozenFile {
    #[serde(default)]
    artifacts: Vec<GavEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GavEntry {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl GavEntry {
    pub fn coordinate(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }

    pub fn sources_url(&self) -> String {
        let group_path = self.group.replace('.', "/");
        format!(
            "https://repo1.maven.org/maven2/{group_path}/{artifact}/{version}/{artifact}-{version}-sources.jar",
            artifact = self.artifact,
            version = self.version,
        )
    }
}

/// Load all source documents from an input.
pub fn load_docs(
    input: &CorpusInput,
    fetch: bool,
    cache_dir: Option<&Path>,
) -> anyhow::Result<Vec<SourceDoc>> {
    match input {
        CorpusInput::Tree(root) => load_tree(root, "local:tree:0"),
        CorpusInput::FrozenGavs(toml_path) => load_frozen(toml_path, fetch, cache_dir),
    }
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("java" | "kt" | "kts")
    )
}

fn load_tree(root: &Path, gav: &str) -> anyhow::Result<Vec<SourceDoc>> {
    let mut docs = Vec::new();
    if root.is_file() {
        if is_source_file(root) {
            let text = fs::read_to_string(root)?;
            docs.push(SourceDoc {
                gav: gav.to_string(),
                path: root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                text,
            });
        }
        return Ok(docs);
    }
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || !is_source_file(path) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path);
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        docs.push(SourceDoc {
            gav: gav.to_string(),
            path: rel.to_string_lossy().replace('\\', "/"),
            text,
        });
    }
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(docs)
}

fn load_frozen(
    toml_path: &Path,
    fetch: bool,
    cache_dir: Option<&Path>,
) -> anyhow::Result<Vec<SourceDoc>> {
    let text = fs::read_to_string(toml_path)?;
    let file: FrozenFile = toml::from_str(&text)?;
    if file.artifacts.is_empty() {
        anyhow::bail!("no [[artifacts]] in {}", toml_path.display());
    }
    let cache = cache_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            toml_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(".scode-cache")
        });
    fs::create_dir_all(&cache)?;

    let mut docs = Vec::new();
    for gav in &file.artifacts {
        let jar_path = cache.join(format!(
            "{}-{}-{}-sources.jar",
            gav.group.replace('.', "_"),
            gav.artifact,
            gav.version
        ));
        if !jar_path.exists() {
            if !fetch {
                anyhow::bail!(
                    "missing cached sources jar {} (pass --fetch to download)",
                    jar_path.display()
                );
            }
            download_sources(gav, &jar_path)?;
        }
        docs.extend(load_sources_jar(&jar_path, &gav.coordinate())?);
    }
    Ok(docs)
}

fn download_sources(gav: &GavEntry, dest: &Path) -> anyhow::Result<()> {
    let url = gav.sources_url();
    eprintln!("fetching {url}");
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| anyhow::anyhow!("download {url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| anyhow::anyhow!("read body: {e}"))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dest, bytes)?;
    Ok(())
}

fn load_sources_jar(jar: &Path, gav: &str) -> anyhow::Result<Vec<SourceDoc>> {
    let file = fs::File::open(jar)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut docs = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        if !is_source_file(Path::new(&name)) {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        let Ok(text) = String::from_utf8(buf) else {
            continue;
        };
        docs.push(SourceDoc {
            gav: gav.to_string(),
            path: name.replace('\\', "/"),
            text,
        });
    }
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(docs)
}

/// Extract occurrences for a document.
pub fn occurrences_for(doc: &SourceDoc, mode: TokenMode) -> Vec<Occurrence> {
    lex::tokenize(&doc.text, mode)
}

/// Load sources from jar bytes (tests / helpers).
pub fn load_sources_jar_bytes(bytes: &[u8], gav: &str) -> anyhow::Result<Vec<SourceDoc>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut docs = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        if !is_source_file(Path::new(&name)) {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        let Ok(text) = String::from_utf8(buf) else {
            continue;
        };
        docs.push(SourceDoc {
            gav: gav.to_string(),
            path: name.replace('\\', "/"),
            text,
        });
    }
    docs.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(docs)
}