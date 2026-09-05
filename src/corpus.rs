//! Corpus ingestion: source trees, frozen GAV TOML, local Maven/Gradle caches.

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
    /// Optional remote Maven-layout base URL for `--fetch` when the artifact is
    /// not already in a local Maven/Gradle cache. Prefer local caches — the same
    /// places Maven/Gradle write into after resolving their configured repos.
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    artifacts: Vec<GavEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GavEntry {
    pub group: String,
    pub artifact: String,
    pub version: String,
    /// Per-artifact remote repository base URL (fetch only).
    #[serde(default)]
    pub repository: Option<String>,
    /// Full URL to the `-sources.jar` (fetch only; skips Maven-layout join).
    #[serde(default)]
    pub sources_url: Option<String>,
}

impl GavEntry {
    pub fn coordinate(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }

    /// Relative Maven-layout path under a local repository root.
    pub fn maven_layout_sources_rel(&self) -> PathBuf {
        PathBuf::from(self.group.replace('.', "/"))
            .join(&self.artifact)
            .join(&self.version)
            .join(format!("{}-{}-sources.jar", self.artifact, self.version))
    }

    /// Resolve a remote sources URL for `--fetch` (not used for local cache hits).
    ///
    /// Priority: `sources_url` → entry `repository` → CLI/MCP override → file `repository`.
    pub fn resolve_sources_url(
        &self,
        file_repository: Option<&str>,
        repo_override: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(url) = self.sources_url.as_deref().filter(|s| !s.is_empty()) {
            return Ok(url.to_string());
        }
        let repo = self
            .repository
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| repo_override.filter(|s| !s.is_empty()))
            .or_else(|| file_repository.filter(|s| !s.is_empty()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no remote repository for {}:{}:{} — not found in local Maven/Gradle caches; \
                     set TOML/CLI `repository`, per-artifact `sources_url`, or install the \
                     sources jar into the local Maven repository",
                    self.group, self.artifact, self.version
                )
            })?;
        Ok(maven_sources_url(repo, &self.group, &self.artifact, &self.version))
    }
}

/// Join a Maven-layout repository base with GAV into a `-sources.jar` URL.
pub fn maven_sources_url(repository: &str, group: &str, artifact: &str, version: &str) -> String {
    let base = repository.trim_end_matches('/');
    let group_path = group.replace('.', "/");
    format!("{base}/{group_path}/{artifact}/{version}/{artifact}-{version}-sources.jar")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Default local Maven repository (`$SCODE_LOCAL_REPO` / `$M2_REPO` / `~/.m2/repository`).
pub fn default_maven_local_repo() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCODE_LOCAL_REPO") {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("M2_REPO") {
        return Some(PathBuf::from(p));
    }
    Some(home_dir()?.join(".m2").join("repository"))
}

fn default_gradle_modules_cache() -> Option<PathBuf> {
    Some(
        home_dir()?
            .join(".gradle")
            .join("caches")
            .join("modules-2")
            .join("files-2.1"),
    )
}

fn maven_layout_sources(local_repo: &Path, gav: &GavEntry) -> PathBuf {
    local_repo.join(gav.maven_layout_sources_rel())
}

fn find_gradle_sources(gav: &GavEntry) -> Option<PathBuf> {
    let base = default_gradle_modules_cache()?
        .join(&gav.group)
        .join(&gav.artifact)
        .join(&gav.version);
    if !base.is_dir() {
        return None;
    }
    let needle = format!("{}-{}-sources.jar", gav.artifact, gav.version);
    for entry in WalkDir::new(&base).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == needle)
        {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// Locate a sources jar already on disk (Maven local / Gradle / scode download cache).
pub fn find_local_sources_jar(
    gav: &GavEntry,
    local_repo: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(repo) = local_repo {
        let p = maven_layout_sources(repo, gav);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(default_repo) = default_maven_local_repo() {
        let already_tried = local_repo.is_some_and(|p| p == default_repo.as_path());
        if !already_tried {
            let p = maven_layout_sources(&default_repo, gav);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(p) = find_gradle_sources(gav) {
        return Some(p);
    }
    if let Some(cache) = cache_dir {
        let p = cache.join(format!(
            "{}-{}-{}-sources.jar",
            gav.group.replace('.', "_"),
            gav.artifact,
            gav.version
        ));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Load all source documents from an input.
pub fn load_docs(
    input: &CorpusInput,
    fetch: bool,
    cache_dir: Option<&Path>,
    repository: Option<&str>,
    local_repo: Option<&Path>,
) -> anyhow::Result<Vec<SourceDoc>> {
    match input {
        CorpusInput::Tree(root) => load_tree(root, "local:tree:0"),
        CorpusInput::FrozenGavs(toml_path) => {
            load_frozen(toml_path, fetch, cache_dir, repository, local_repo)
        }
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
    repository: Option<&str>,
    local_repo: Option<&Path>,
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

    let file_repo = file.repository.as_deref();
    let mut docs = Vec::new();
    for gav in &file.artifacts {
        let jar_path = if let Some(existing) =
            find_local_sources_jar(gav, local_repo, Some(cache.as_path()))
        {
            existing
        } else if fetch {
            let dest = cache.join(format!(
                "{}-{}-{}-sources.jar",
                gav.group.replace('.', "_"),
                gav.artifact,
                gav.version
            ));
            let url = gav.resolve_sources_url(file_repo, repository)?;
            download_url(&url, &dest)?;
            dest
        } else {
            anyhow::bail!(
                "sources jar not found for {} in local Maven/Gradle caches \
                 (tried --local-repo, ~/.m2/repository, Gradle modules cache, {}); \
                 pass --fetch with a remote `repository`, or install sources into the local repo",
                gav.coordinate(),
                cache.display()
            );
        };
        docs.extend(load_sources_jar(&jar_path, &gav.coordinate())?);
    }
    Ok(docs)
}

fn download_url(url: &str, dest: &Path) -> anyhow::Result<()> {
    eprintln!("fetching {url}");
    let resp = ureq::get(url)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maven_url_join_trims_slash() {
        let url = maven_sources_url(
            "https://example.corp/maven2/",
            "com.acme",
            "lib",
            "1.2.3",
        );
        assert_eq!(
            url,
            "https://example.corp/maven2/com/acme/lib/1.2.3/lib-1.2.3-sources.jar"
        );
    }

    #[test]
    fn resolve_prefers_sources_url_then_entry_repo() {
        let mut gav = GavEntry {
            group: "g".into(),
            artifact: "a".into(),
            version: "1".into(),
            repository: Some("https://entry.example/m2".into()),
            sources_url: Some("https://cdn.example/a-sources.jar".into()),
        };
        assert_eq!(
            gav.resolve_sources_url(Some("https://file.example/m2"), Some("https://cli.example/m2"))
                .unwrap(),
            "https://cdn.example/a-sources.jar"
        );
        gav.sources_url = None;
        assert_eq!(
            gav.resolve_sources_url(Some("https://file.example/m2"), Some("https://cli.example/m2"))
                .unwrap(),
            "https://entry.example/m2/g/a/1/a-1-sources.jar"
        );
        gav.repository = None;
        assert_eq!(
            gav.resolve_sources_url(Some("https://file.example/m2"), Some("https://cli.example/m2"))
                .unwrap(),
            "https://cli.example/m2/g/a/1/a-1-sources.jar"
        );
        let err = gav.resolve_sources_url(None, None).unwrap_err().to_string();
        assert!(err.contains("no remote repository"), "{err}");
    }

    #[test]
    fn finds_jar_in_maven_local_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let gav = GavEntry {
            group: "com.acme".into(),
            artifact: "lib".into(),
            version: "1.0.0".into(),
            repository: None,
            sources_url: None,
        };
        let dest = maven_layout_sources(tmp.path(), &gav);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // minimal zip so is_file() is enough for finder; loader not called here
        fs::write(&dest, b"pk\x03\x04").unwrap();
        let found = find_local_sources_jar(&gav, Some(tmp.path()), None).unwrap();
        assert_eq!(found, dest);
    }
}