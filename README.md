# scode

Compressed **gap + Elias-δ** inverted indexes for **identifier name locate** over immutable Maven/Gradle `sources.jar` keep-sets (and plain source trees).

Built from the Track A paper draft: on frozen library sources, δ postings beat a Huffman wavelet-matrix baseline for name locate; this repo ships the product δ path (CLI + MCP).

## Install

```bash
cargo build --release
# binary: target/release/scode
```

## CLI

```bash
# Index a source tree (idents = lexer identifiers outside comments/strings)
scode index --backend delta --input fixtures/demo --out /tmp/scode-demo

# Search
scode search --index /tmp/scode-demo --query HttpClient
# local:tree:0 src/main/java/demo/Foo.java:8:26

# Multi-query (OR + dedup + matched_queries tags)
scode search-multi --index /tmp/scode-demo -q HttpClient -q Foo --json

# Wider tokens (comments/strings included; closer to rg word-boundary identity)
scode index --input fixtures/demo --out /tmp/scode-all --token-mode all

# Frozen GAV list — prefers jars already in local Maven/Gradle caches
# (~/.m2/repository, $M2_REPO, Gradle modules cache). No remote host required.
scode index --input corpus/frozen-gavs.toml --out /tmp/scode-h

# Optional: fetch only what is missing, using a repo Maven/Gradle would use
# scode index --input corpus/frozen-gavs.toml --out /tmp/scode-h --fetch \
#   --repository https://my.mirror/maven2
```

### Token modes

| Mode | Meaning |
|------|---------|
| `idents` (default) | Java/Kotlin identifiers in code only |
| `all` | Identifier-shaped tokens across the whole file |

### GAV resolution (Maven/Gradle local first)

Frozen GAV indexing looks for `*-sources.jar` where Maven/Gradle already put them:

1. `--local-repo` / `$SCODE_LOCAL_REPO` / `$M2_REPO` / `~/.m2/repository` (Maven layout)
2. `~/.gradle/caches/modules-2/files-2.1/...` (Gradle module cache)
3. scode download cache (`.scode-cache`)

Remote URLs (`repository` / `sources_url` / `--repository`) are only needed with `--fetch` when the jar is not already local. scode does not hardcode Maven Central.

```toml
# repository is optional — only for --fetch of missing jars
# repository = "https://my.mirror/maven2"

[[artifacts]]
group = "com.acme"
artifact = "lib"
version = "1.0.0"
```

### Output

Hits are `GAV path:line:col`. Tree inputs use a synthetic GAV `local:tree:0`.

## MCP

```bash
scode mcp
```

stdio MCP tools:

| Tool | Role |
|------|------|
| `scode_index` | Build index. `out` optional (omit = memory-only). `memory_id` default `default` |
| `scode_search` | Name locate. `index` optional (`memory` / omit = RAM) |
| `scode_search_multi` | Multi-name OR + dedup + `matched_queries` |
| `scode_stats` | Index stats |
| `scode_load` / `scode_unload` | Load/unload a disk index in the session |

Cursor example (`mcp.json`):

```json
{
  "mcpServers": {
    "scode": {
      "command": "/path/to/scode",
      "args": ["mcp"]
    }
  }
}
```

CLI remains **disk-only**; memory-only indexes are MCP-session scoped (gone on process exit).

## Library layout

| Module | Role |
|--------|------|
| `lex` | `.java` / `.kt` / `.kts` tokenization |
| `intern` | Name dictionary |
| `delta` | Gap + Elias-δ codec |
| `corpus` | Trees + `frozen-gavs.toml` (+ optional Maven fetch) |
| `index` | Build / persist / locate / search-multi |
| `mcp` | stdio MCP server |

## License

MIT
