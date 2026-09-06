//! Corpus ingestion: source trees, frozen GAV TOML, local Maven/Gradle caches.

use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use walkdir::WalkDir;

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

/// Shared corpus-load knobs for CLI and MCP.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadOptions<'a> {
    pub fetch: bool,
    pub cache_dir: Option<&'a Path>,
    pub repository: Option<&'a str>,
    pub local_repo: Option<&'a Path>,
}

const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_JAR_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JAR_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

fn validate_gav_segment(kind: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("{kind} must not be empty");
    }
    if value == "." || value == ".." {
        anyhow::bail!("{kind} `{value}` is not allowed");
    }
    if value.contains(['/', '\\', '\0']) {
        anyhow::bail!("{kind} `{value}` contains forbidden path characters");
    }
    Ok(())
}

impl GavEntry {
    pub fn coordinate(&self) -> String {
        format!("{}:{}:{}", self.group, self.artifact, self.version)
    }

    /// Reject GAV fields that could escape cache/repo roots via path join.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.group.is_empty() {
            anyhow::bail!("group must not be empty");
        }
        for part in self.group.split('.') {
            validate_gav_segment("group", part)?;
        }
        validate_gav_segment("artifact", &self.artifact)?;
        validate_gav_segment("version", &self.version)?;
        Ok(())
    }

    /// Single-component cache filename (safe after [`Self::validate`]).
    pub fn cache_jar_name(&self) -> String {
        format!(
            "{}-{}-{}-sources.jar",
            self.group.replace('.', "_"),
            self.artifact,
            self.version
        )
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
                    self.group,
                    self.artifact,
                    self.version
                )
            })?;
        Ok(maven_sources_url(
            repo,
            &self.group,
            &self.artifact,
            &self.version,
        ))
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
        let p = cache.join(gav.cache_jar_name());
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Load all source documents from an input.
pub fn load_docs(input: &CorpusInput, opts: LoadOptions<'_>) -> anyhow::Result<Vec<SourceDoc>> {
    match input {
        CorpusInput::Tree(root) => load_tree(root, "local:tree:0"),
        CorpusInput::FrozenGavs(toml_path) => load_frozen(toml_path, opts),
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
            Err(e) => {
                eprintln!("warning: skipping {}: {e}", path.display());
                continue;
            }
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

fn load_frozen(toml_path: &Path, opts: LoadOptions<'_>) -> anyhow::Result<Vec<SourceDoc>> {
    let text = fs::read_to_string(toml_path)?;
    let file: FrozenFile = toml::from_str(&text)?;
    if file.artifacts.is_empty() {
        anyhow::bail!("no [[artifacts]] in {}", toml_path.display());
    }
    let cache = opts.cache_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        toml_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(".scode-cache")
    });
    fs::create_dir_all(&cache)?;

    let file_repo = file.repository.as_deref();
    let mut docs = Vec::new();
    for gav in &file.artifacts {
        gav.validate()?;
        let jar_path = if let Some(existing) =
            find_local_sources_jar(gav, opts.local_repo, Some(cache.as_path()))
        {
            existing
        } else if opts.fetch {
            let dest = cache.join(gav.cache_jar_name());
            let url = gav.resolve_sources_url(file_repo, opts.repository)?;
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

fn copy_with_limit(
    reader: &mut impl Read,
    writer: &mut impl Write,
    limit: u64,
) -> anyhow::Result<u64> {
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| anyhow::anyhow!("transfer size overflow"))?;
        if total > limit {
            anyhow::bail!("transfer exceeded size limit ({limit} bytes)");
        }
        writer.write_all(&buf[..n])?;
    }
    Ok(total)
}

/// Redact userinfo from URLs before logging (best-effort, no extra deps).
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(at) = rest.find('@') {
            return format!("{}{}", &url[..scheme_end + 3], &rest[at + 1..]);
        }
    }
    url.to_string()
}

fn download_url(url: &str, dest: &Path) -> anyhow::Result<()> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        anyhow::bail!("refusing non-http(s) download URL: {url}");
    }
    eprintln!("fetching {}", redact_url(url));
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Unique sibling temp path so concurrent fetches of the same GAV cannot
    // clobber each other; rename into place only after a full successful write.
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut tmp_os = dest.as_os_str().to_owned();
    tmp_os.push(format!(".tmp-{unique}"));
    let tmp = PathBuf::from(tmp_os);

    let result = (|| -> anyhow::Result<()> {
        let resp = ureq::get(url)
            .timeout(Duration::from_secs(300))
            .call()
            .map_err(|e| anyhow::anyhow!("download {url}: {e}"))?;
        let mut file =
            fs::File::create(&tmp).map_err(|e| anyhow::anyhow!("create {}: {e}", tmp.display()))?;
        let mut reader = resp.into_reader();
        copy_with_limit(&mut reader, &mut file, MAX_DOWNLOAD_BYTES)
            .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| anyhow::anyhow!("sync {}: {e}", tmp.display()))?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp, dest) {
        fs::copy(&tmp, dest).map_err(|copy_err| {
            anyhow::anyhow!(
                "finalize {}: rename failed ({e}), copy failed ({copy_err})",
                dest.display()
            )
        })?;
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
}

fn load_sources_jar(jar: &Path, gav: &str) -> anyhow::Result<Vec<SourceDoc>> {
    let file = fs::File::open(jar)?;
    load_sources_jar_reader(file, gav)
}

/// Extract sources from jar bytes (tests / helpers).
pub fn load_sources_jar_bytes(bytes: &[u8], gav: &str) -> anyhow::Result<Vec<SourceDoc>> {
    load_sources_jar_reader(Cursor::new(bytes), gav)
}

/// Reject zip-slip style paths (`..`, absolute, drive prefixes) before indexing names.
fn is_safe_jar_entry_path(name: &str) -> bool {
    let path = Path::new(name);
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn load_sources_jar_reader<R: Read + io::Seek>(
    reader: R,
    gav: &str,
) -> anyhow::Result<Vec<SourceDoc>> {
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut docs = Vec::new();
    let mut total_bytes = 0u64;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        if !is_safe_jar_entry_path(&name) {
            anyhow::bail!("jar entry has unsafe path `{name}`");
        }
        if !is_source_file(Path::new(&name)) {
            continue;
        }
        if entry.size() > MAX_JAR_ENTRY_BYTES {
            anyhow::bail!(
                "jar entry `{name}` is too large ({} bytes; limit {MAX_JAR_ENTRY_BYTES})",
                entry.size()
            );
        }
        let mut buf = Vec::new();
        entry.take(MAX_JAR_ENTRY_BYTES + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > MAX_JAR_ENTRY_BYTES {
            anyhow::bail!("jar entry `{name}` exceeded size limit ({MAX_JAR_ENTRY_BYTES} bytes)");
        }
        let Ok(text) = String::from_utf8(buf) else {
            continue;
        };
        total_bytes = total_bytes
            .checked_add(text.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("jar uncompressed size overflow"))?;
        if total_bytes > MAX_JAR_TOTAL_BYTES {
            anyhow::bail!("jar uncompressed content exceeds limit ({MAX_JAR_TOTAL_BYTES} bytes)");
        }
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
        let url = maven_sources_url("https://example.corp/maven2/", "com.acme", "lib", "1.2.3");
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
            gav.resolve_sources_url(
                Some("https://file.example/m2"),
                Some("https://cli.example/m2")
            )
            .unwrap(),
            "https://cdn.example/a-sources.jar"
        );
        gav.sources_url = None;
        assert_eq!(
            gav.resolve_sources_url(
                Some("https://file.example/m2"),
                Some("https://cli.example/m2")
            )
            .unwrap(),
            "https://entry.example/m2/g/a/1/a-1-sources.jar"
        );
        gav.repository = None;
        assert_eq!(
            gav.resolve_sources_url(
                Some("https://file.example/m2"),
                Some("https://cli.example/m2")
            )
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

    #[test]
    fn rejects_path_traversal_in_gav() {
        let bad = GavEntry {
            group: "com.acme".into(),
            artifact: "x/../../evil".into(),
            version: "1.0".into(),
            repository: None,
            sources_url: None,
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("forbidden") || err.contains("path"), "{err}");

        let bad_group = GavEntry {
            group: "com/../evil".into(),
            artifact: "lib".into(),
            version: "1".into(),
            repository: None,
            sources_url: None,
        };
        assert!(bad_group.validate().is_err());
    }

    #[test]
    fn download_rejects_non_http_urls() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("x.jar");
        let err = download_url("file:///etc/passwd", &dest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-http"), "{err}");
    }

    #[test]
    fn rejects_zip_slip_jar_entry_paths() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            w.start_file("../Evil.java", SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"class Evil {}").unwrap();
            w.finish().unwrap();
        }
        let err = load_sources_jar_bytes(buf.get_ref(), "g:a:1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unsafe path"), "{err}");
    }
}
