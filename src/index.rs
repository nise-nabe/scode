//! Index build, persistence, and name locate.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::corpus::{self, CorpusInput, LoadOptions, SourceDoc};
use crate::delta::{decode_gaps, encode_gaps};
use crate::intern::Dictionary;
use crate::lex::{self, Occurrence, TokenMode};

/// A locate hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub gav: String,
    pub path: String,
    pub line: u32,
    pub col: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_queries: Vec<String>,
}

impl Hit {
    pub fn display(&self) -> String {
        if self.matched_queries.is_empty() {
            format!("{} {}:{}:{}", self.gav, self.path, self.line, self.col)
        } else {
            format!(
                "{} {}:{}:{} [{}]",
                self.gav,
                self.path,
                self.line,
                self.col,
                self.matched_queries.join(",")
            )
        }
    }

    fn key(&self) -> (String, String, u32, u32) {
        (self.gav.clone(), self.path.clone(), self.line, self.col)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub backend: String,
    pub token_mode: TokenMode,
    pub doc_count: usize,
    pub name_count: usize,
    pub occurrence_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub backend: String,
    pub token_mode: String,
    pub docs: usize,
    pub names: usize,
    pub occurrences: usize,
    pub posting_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DocMeta {
    gav: String,
    path: String,
}

#[derive(Debug, Clone, Copy)]
struct Occ {
    doc_id: u32,
    line: u32,
    col: u32,
}

/// In-memory / on-disk inverted index.
#[derive(Debug, Clone)]
pub struct Index {
    pub manifest: Manifest,
    dict: Dictionary,
    docs: Vec<DocMeta>,
    occs: Vec<Occ>,
    /// For each name id: (encoded gaps of occurrence indices, count).
    postings: Vec<(Vec<u8>, u32)>,
}

impl Index {
    pub fn stats(&self) -> IndexStats {
        let posting_bytes = self.postings.iter().map(|(b, _)| b.len()).sum();
        IndexStats {
            backend: self.manifest.backend.clone(),
            token_mode: self.manifest.token_mode.as_str().to_string(),
            docs: self.docs.len(),
            names: self.dict.len(),
            occurrences: self.occs.len(),
            posting_bytes,
        }
    }

    pub fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<Hit>> {
        let Some(id) = self.dict.lookup(query) else {
            return Ok(Vec::new());
        };
        let (blob, count) = self
            .postings
            .get(id as usize)
            .ok_or_else(|| anyhow::anyhow!("corrupt index: name id {id} missing from postings"))?;
        let indices = decode_gaps(blob, *count as usize)?;
        let mut hits = Vec::with_capacity(indices.len());
        for idx in indices {
            let occ = self.occs.get(idx as usize).ok_or_else(|| {
                anyhow::anyhow!("corrupt postings: occurrence index {idx} out of range")
            })?;
            let doc = self.docs.get(occ.doc_id as usize).ok_or_else(|| {
                anyhow::anyhow!("corrupt postings: doc_id {} out of range", occ.doc_id)
            })?;
            hits.push(Hit {
                gav: doc.gav.clone(),
                path: doc.path.clone(),
                line: occ.line,
                col: occ.col,
                matched_queries: Vec::new(),
            });
            if limit.is_some_and(|l| hits.len() >= l) {
                break;
            }
        }
        Ok(hits)
    }

    /// Multi-query OR with dedup and matched_queries tags.
    pub fn search_multi(
        &self,
        queries: &[String],
        limit: Option<usize>,
        per_query_limit: Option<usize>,
    ) -> anyhow::Result<SearchMultiResult> {
        let partials: Result<Vec<(String, Vec<Hit>)>, anyhow::Error> = queries
            .par_iter()
            .map(|q| {
                self.search(q, per_query_limit)
                    .map(|hits| (q.clone(), hits))
            })
            .collect();
        let partials = partials?;

        let mut map: HashMap<(String, String, u32, u32), Hit> = HashMap::new();
        for (q, hits) in partials {
            for mut h in hits {
                let k = h.key();
                map.entry(k)
                    .and_modify(|existing| {
                        if !existing.matched_queries.contains(&q) {
                            existing.matched_queries.push(q.clone());
                        }
                    })
                    .or_insert_with(|| {
                        h.matched_queries = vec![q.clone()];
                        h
                    });
            }
        }

        let mut merged: Vec<Hit> = map.into_values().collect();
        merged.sort_by(|a, b| {
            (&a.gav, &a.path, a.line, a.col).cmp(&(&b.gav, &b.path, b.line, b.col))
        });
        if let Some(l) = limit {
            merged.truncate(l);
        }
        Ok(SearchMultiResult { hits: merged })
    }

    pub fn write_to_dir(&self, dir: &Path) -> anyhow::Result<()> {
        // Write a complete index into a unique sibling temp dir, then swap into
        // place. Never delete the live directory before the new one is ready.
        // Also refuse to replace a directory that contains non-index files so
        // `--out` cannot wipe an unrelated tree.
        ensure_replaceable_index_dir(dir)?;
        let parent = dir.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("scode-index");
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmp = parent.join(format!(".{name}.tmp-{unique}"));
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        fs::create_dir_all(&tmp)?;

        let write_result = (|| -> anyhow::Result<()> {
            fs::write(
                tmp.join("manifest.json"),
                serde_json::to_vec_pretty(&self.manifest)?,
            )?;
            fs::write(tmp.join("dict.bin"), self.dict.to_bytes())?;
            fs::write(tmp.join("docs.json"), serde_json::to_vec(&self.docs)?)?;
            fs::write(tmp.join("occs.bin"), pack_occs(&self.occs))?;
            fs::write(tmp.join("postings.bin"), pack_postings(&self.postings))?;
            Ok(())
        })();

        if let Err(e) = write_result {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }

        if dir.exists() {
            let bak = parent.join(format!(".{name}.bak-{unique}"));
            if bak.exists() {
                fs::remove_dir_all(&bak)?;
            }
            fs::rename(dir, &bak).map_err(|e| {
                let _ = fs::remove_dir_all(&tmp);
                anyhow::anyhow!("backup {}: {e}", dir.display())
            })?;
            if let Err(e) = promote_tmp(&tmp, dir) {
                // Best-effort restore of the previous index.
                let _ = fs::rename(&bak, dir);
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
            let _ = fs::remove_dir_all(&bak);
        } else if let Err(e) = promote_tmp(&tmp, dir) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        Ok(())
    }

    pub fn open_dir(dir: &Path) -> anyhow::Result<Self> {
        let manifest: Manifest = serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
        let dict = Dictionary::from_bytes(&fs::read(dir.join("dict.bin"))?)?;
        let docs: Vec<DocMeta> = serde_json::from_slice(&fs::read(dir.join("docs.json"))?)?;
        let occs = unpack_occs(&fs::read(dir.join("occs.bin"))?)?;
        let postings = unpack_postings(&fs::read(dir.join("postings.bin"))?)?;
        if postings.len() != dict.len() {
            anyhow::bail!(
                "postings/dict length mismatch: {} vs {}",
                postings.len(),
                dict.len()
            );
        }
        Ok(Self {
            manifest,
            dict,
            docs,
            occs,
            postings,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMultiResult {
    pub hits: Vec<Hit>,
}

const INDEX_FILES: &[&str] = &[
    "manifest.json",
    "dict.bin",
    "docs.json",
    "occs.bin",
    "postings.bin",
];

/// Allow replace only for empty dirs or dirs that already look like an scode index.
/// Prevents accidentally wiping unrelated files under `--out`.
fn ensure_replaceable_index_dir(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    if !dir.is_dir() {
        anyhow::bail!("index out path is not a directory: {}", dir.display());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            anyhow::bail!(
                "refusing to replace {}: contains a non-UTF8 entry",
                dir.display()
            );
        };
        if !INDEX_FILES.contains(&name) {
            anyhow::bail!(
                "refusing to replace {}: contains non-index entry `{name}` \
                 (expected only {}); use an empty directory or an existing scode index",
                dir.display(),
                INDEX_FILES.join(", ")
            );
        }
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

fn promote_tmp(tmp: &Path, dir: &Path) -> anyhow::Result<()> {
    if let Err(e) = fs::rename(tmp, dir) {
        copy_dir_all(tmp, dir).map_err(|copy_err| {
            anyhow::anyhow!(
                "finalize index {}: rename failed ({e}), copy failed ({copy_err})",
                dir.display()
            )
        })?;
        let _ = fs::remove_dir_all(tmp);
    }
    Ok(())
}

fn pack_occs(occs: &[Occ]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + occs.len() * 12);
    out.extend_from_slice(&(occs.len() as u32).to_le_bytes());
    for o in occs {
        out.extend_from_slice(&o.doc_id.to_le_bytes());
        out.extend_from_slice(&o.line.to_le_bytes());
        out.extend_from_slice(&o.col.to_le_bytes());
    }
    out
}

fn unpack_occs(mut data: &[u8]) -> anyhow::Result<Vec<Occ>> {
    if data.len() < 4 {
        anyhow::bail!("truncated occs");
    }
    let n = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    data = &data[4..];
    if data.len() < n * 12 {
        anyhow::bail!("truncated occs payload");
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let doc_id = u32::from_le_bytes(data[..4].try_into().unwrap());
        let line = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let col = u32::from_le_bytes(data[8..12].try_into().unwrap());
        data = &data[12..];
        out.push(Occ { doc_id, line, col });
    }
    Ok(out)
}

fn pack_postings(postings: &[(Vec<u8>, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(postings.len() as u32).to_le_bytes());
    for (blob, count) in postings {
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        out.extend_from_slice(blob);
    }
    out
}

fn unpack_postings(mut data: &[u8]) -> anyhow::Result<Vec<(Vec<u8>, u32)>> {
    if data.len() < 4 {
        anyhow::bail!("truncated postings");
    }
    let n = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    data = &data[4..];
    // Each posting needs at least an 8-byte header; reject absurd `n` before allocating.
    if n > 0 && data.len() / 8 < n {
        anyhow::bail!(
            "postings count {n} exceeds remaining data ({} bytes)",
            data.len()
        );
    }
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|e| anyhow::anyhow!("postings count {n} too large to allocate: {e}"))?;
    for _ in 0..n {
        if data.len() < 8 {
            anyhow::bail!("truncated posting header");
        }
        let count = u32::from_le_bytes(data[..4].try_into().unwrap());
        let len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        data = &data[8..];
        if data.len() < len {
            anyhow::bail!("truncated posting blob");
        }
        let mut blob = Vec::new();
        blob.try_reserve_exact(len)
            .map_err(|e| anyhow::anyhow!("posting blob length {len} too large to allocate: {e}"))?;
        blob.extend_from_slice(&data[..len]);
        data = &data[len..];
        out.push((blob, count));
    }
    Ok(out)
}

/// Build an index from corpus input.
pub fn build_index(
    input: &CorpusInput,
    token_mode: TokenMode,
    opts: LoadOptions<'_>,
) -> anyhow::Result<Index> {
    let docs = corpus::load_docs(input, opts)?;
    build_from_docs(docs, token_mode)
}

/// Build an index from in-memory documents.
pub fn build_from_docs(docs_in: Vec<SourceDoc>, token_mode: TokenMode) -> anyhow::Result<Index> {
    let mut dict = Dictionary::new();
    let mut docs = Vec::with_capacity(docs_in.len());
    let mut occs = Vec::new();
    let mut posting_lists: BTreeMap<u32, Vec<u32>> = BTreeMap::new();

    let mut tokenized: Vec<(SourceDoc, Vec<Occurrence>)> = docs_in
        .into_par_iter()
        .map(|doc| {
            let file_occs = lex::tokenize(&doc.text, token_mode);
            (doc, file_occs)
        })
        .collect();

    tokenized.sort_by(|a, b| (&a.0.gav, &a.0.path).cmp(&(&b.0.gav, &b.0.path)));

    for (doc, file_occs) in tokenized {
        let doc_id = docs.len() as u32;
        docs.push(DocMeta {
            gav: doc.gav,
            path: doc.path,
        });
        for o in file_occs {
            let name_id = dict.intern(&o.name);
            let occ_id = occs.len() as u32;
            occs.push(Occ {
                doc_id,
                line: o.line,
                col: o.col,
            });
            posting_lists.entry(name_id).or_default().push(occ_id);
        }
    }

    let mut postings = vec![(Vec::new(), 0u32); dict.len()];
    for (name_id, mut indices) in posting_lists {
        indices.sort_unstable();
        indices.dedup();
        let count = indices.len() as u32;
        let blob = encode_gaps(&indices);
        postings[name_id as usize] = (blob, count);
    }

    let manifest = Manifest {
        backend: "delta".into(),
        token_mode,
        doc_count: docs.len(),
        name_count: dict.len(),
        occurrence_count: occs.len(),
    };

    Ok(Index {
        manifest,
        dict,
        docs,
        occs,
        postings,
    })
}

/// CLI/MCP helper: build and optionally persist.
pub fn index_and_maybe_write(
    input: &Path,
    out: Option<&Path>,
    token_mode: TokenMode,
    opts: LoadOptions<'_>,
) -> anyhow::Result<Index> {
    let corpus = CorpusInput::detect(input);
    let index = build_index(&corpus, token_mode, opts)?;
    if let Some(dir) = out {
        index.write_to_dir(dir)?;
    }
    Ok(index)
}

/// Session store for MCP in-memory / loaded indexes.
#[derive(Default)]
pub struct MemoryStore {
    map: HashMap<String, Arc<Index>>,
    path_map: HashMap<String, Arc<Index>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_memory(&mut self, id: &str, index: Index) -> Arc<Index> {
        let arc = Arc::new(index);
        self.map.insert(id.to_string(), arc.clone());
        arc
    }

    /// Register an already-built index under a disk path without re-reading it.
    pub fn insert_path(&mut self, path: &Path, index: Arc<Index>) {
        let key = path.to_string_lossy().into_owned();
        self.path_map.insert(key, index);
    }

    pub fn get_memory(&self, id: &str) -> Option<Arc<Index>> {
        self.map.get(id).cloned()
    }

    pub fn unload_memory(&mut self, id: &str) -> bool {
        self.map.remove(id).is_some()
    }

    pub fn load_path(&mut self, path: &Path) -> anyhow::Result<Arc<Index>> {
        let key = path.to_string_lossy().into_owned();
        if let Some(existing) = self.path_map.get(&key) {
            return Ok(existing.clone());
        }
        let idx = Index::open_dir(path)?;
        let arc = Arc::new(idx);
        self.path_map.insert(key, arc.clone());
        Ok(arc)
    }

    /// Return a cached index without touching disk.
    pub fn peek(&self, index_arg: Option<&str>) -> Option<Arc<Index>> {
        match index_arg {
            None | Some("") | Some("memory") => self.get_memory("default"),
            Some(p) => {
                let key = Path::new(p).to_string_lossy().into_owned();
                self.path_map
                    .get(&key)
                    .cloned()
                    .or_else(|| self.get_memory(p))
            }
        }
    }

    /// Insert a disk-loaded index, returning any racing winner already present.
    pub fn insert_loaded_path(&mut self, path: &Path, index: Arc<Index>) -> Arc<Index> {
        let key = path.to_string_lossy().into_owned();
        self.path_map.entry(key).or_insert_with(|| index).clone()
    }

    pub fn unload_path(&mut self, path: &Path) -> bool {
        let key = path.to_string_lossy().into_owned();
        self.path_map.remove(&key).is_some()
    }

    pub fn has_loaded_path(&self, path: &Path) -> bool {
        let key = path.to_string_lossy().into_owned();
        self.path_map.contains_key(&key)
    }

    pub fn resolve(&mut self, index_arg: Option<&str>) -> anyhow::Result<Arc<Index>> {
        match index_arg {
            None | Some("") | Some("memory") => self
                .get_memory("default")
                .ok_or_else(|| anyhow::anyhow!("no in-memory index; call scode_index first")),
            Some(p) => {
                let path = Path::new(p);
                // loaded path (via peek/path_map) is handled by callers; here prefer memory
                // slot ids over colliding filesystem paths.
                if let Some(mem) = self.get_memory(p) {
                    Ok(mem)
                } else if self.has_loaded_path(path) || path.exists() {
                    self.load_path(path)
                } else {
                    anyhow::bail!("index not found: {p}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_search_exact_ident() {
        let docs = vec![SourceDoc {
            gav: "demo:lib:1".into(),
            path: "Foo.java".into(),
            text: r#"
                class Foo {
                  HttpClient client;
                  CloseableHttpClient other;
                }
            "#
            .into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        let hits = idx.search("HttpClient", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "Foo.java");
        let close = idx.search("CloseableHttpClient", None).unwrap();
        assert_eq!(close.len(), 1);
    }

    #[test]
    fn search_multi_dedup_and_tags() {
        let docs = vec![SourceDoc {
            gav: "demo:lib:1".into(),
            path: "A.java".into(),
            text: "class A { HttpClient a; Foo b; }\n".into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        let res = idx
            .search_multi(
                &["HttpClient".into(), "Foo".into(), "HttpClient".into()],
                None,
                None,
            )
            .unwrap();
        assert_eq!(res.hits.len(), 2);
        let hc = res
            .hits
            .iter()
            .find(|h| h.matched_queries.iter().any(|q| q == "HttpClient"))
            .unwrap();
        assert!(hc.matched_queries.contains(&"HttpClient".to_string()));
    }

    #[test]
    fn persist_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let docs = vec![SourceDoc {
            gav: "g:a:1".into(),
            path: "X.java".into(),
            text: "class X { Bar b; }\n".into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        idx.write_to_dir(dir.path()).unwrap();
        let loaded = Index::open_dir(dir.path()).unwrap();
        assert_eq!(loaded.search("Bar", None).unwrap().len(), 1);
        assert_eq!(loaded.stats().names, idx.stats().names);
    }

    #[test]
    fn refuses_non_index_out_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();
        let docs = vec![SourceDoc {
            gav: "g:a:1".into(),
            path: "X.java".into(),
            text: "class X { Bar b; }\n".into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        let err = idx.write_to_dir(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("non-index") || err.contains("refusing"),
            "{err}"
        );
        assert!(dir.path().join("notes.txt").is_file());
    }

    #[test]
    fn memory_id_preferred_over_filesystem_path() {
        let docs = vec![SourceDoc {
            gav: "demo:lib:1".into(),
            path: "Foo.java".into(),
            text: "class Foo {}".into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        let mut store = MemoryStore::new();
        let tmp = tempfile::tempdir().unwrap();
        let collision = tmp.path().join("slot");
        std::fs::create_dir_all(&collision).unwrap();
        // Write a tiny valid-looking dir is unnecessary; exists() alone matters for resolve.
        let key = collision.to_string_lossy().into_owned();
        store.insert_memory(&key, idx);
        let got = store.resolve(Some(&key)).unwrap();
        assert!(store.get_memory(&key).is_some());
        // Should not attempt to load the empty directory as an index.
        assert_eq!(got.stats().docs, 1);
    }

    #[test]
    fn unload_prefers_memory_slot_when_path_also_exists() {
        let docs = vec![SourceDoc {
            gav: "demo:lib:1".into(),
            path: "Foo.java".into(),
            text: "class Foo {}".into(),
        }];
        let idx = build_from_docs(docs, TokenMode::Idents).unwrap();
        let mut store = MemoryStore::new();
        let tmp = tempfile::tempdir().unwrap();
        let collision = tmp.path().join("slot");
        std::fs::create_dir_all(&collision).unwrap();
        let key = collision.to_string_lossy().into_owned();
        store.insert_memory(&key, idx);
        // Mimic scode_unload precedence without filesystem-first.
        let path = Path::new(&key);
        assert!(!store.has_loaded_path(path));
        assert!(store.get_memory(&key).is_some());
        assert!(store.unload_memory(&key));
        assert!(store.get_memory(&key).is_none());
    }
}
