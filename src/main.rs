//! scode CLI: index / search / search-multi / mcp

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use scode::corpus::{LoadOptions, TokenMode};
use scode::index::{index_and_maybe_write, Index};

#[derive(Debug, Parser)]
#[command(name = "scode", version, about = "δ inverted-index name locate for Java/Kotlin sources")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build a δ inverted index
    Index {
        /// Source tree or frozen-gavs.toml
        #[arg(long)]
        input: PathBuf,
        /// Output index directory
        #[arg(long)]
        out: PathBuf,
        /// Index backend (only delta is supported)
        #[arg(long, default_value = "delta")]
        backend: String,
        /// Token mode
        #[arg(long, value_enum, default_value_t = TokenModeArg::Idents)]
        token_mode: TokenModeArg,
        /// Download missing sources jars only when not already in a local Maven/Gradle cache
        #[arg(long, default_value_t = false)]
        fetch: bool,
        /// Cache directory for downloaded jars (scode download cache)
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// Local Maven repository root (default: $SCODE_LOCAL_REPO / $M2_REPO / ~/.m2/repository)
        #[arg(long)]
        local_repo: Option<PathBuf>,
        /// Remote Maven-layout base URL for --fetch when the jar is not local
        #[arg(long)]
        repository: Option<String>,
    },
    /// Locate a simple name
    Search {
        #[arg(long)]
        index: PathBuf,
        #[arg(long, short = 'q')]
        query: String,
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of text lines
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Locate multiple names (OR + dedup)
    SearchMulti {
        #[arg(long)]
        index: PathBuf,
        #[arg(long = "query", short = 'q', required = true)]
        queries: Vec<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        per_query_limit: Option<usize>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Run stdio MCP server
    Mcp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TokenModeArg {
    Idents,
    All,
}

impl From<TokenModeArg> for TokenMode {
    fn from(v: TokenModeArg) -> Self {
        match v {
            TokenModeArg::Idents => TokenMode::Idents,
            TokenModeArg::All => TokenMode::All,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Index {
            input,
            out,
            backend,
            token_mode,
            fetch,
            cache_dir,
            local_repo,
            repository,
        } => {
            if backend != "delta" {
                anyhow::bail!("unsupported backend `{backend}` (only `delta`)");
            }
            let index = index_and_maybe_write(
                &input,
                Some(&out),
                token_mode.into(),
                LoadOptions {
                    fetch,
                    cache_dir: cache_dir.as_deref(),
                    repository: repository.as_deref(),
                    local_repo: local_repo.as_deref(),
                },
            )?;
            let stats = index.stats();
            eprintln!(
                "indexed docs={} names={} occs={} posting_bytes={} -> {}",
                stats.docs,
                stats.names,
                stats.occurrences,
                stats.posting_bytes,
                out.display()
            );
        }
        Command::Search {
            index,
            query,
            limit,
            json,
        } => {
            let idx = Index::open_dir(&index)?;
            let hits = idx.search(&query, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else {
                for h in &hits {
                    println!("{}", h.display());
                }
            }
        }
        Command::SearchMulti {
            index,
            queries,
            limit,
            per_query_limit,
            json,
        } => {
            let idx = Index::open_dir(&index)?;
            let res = idx.search_multi(&queries, limit, per_query_limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                for h in &res.hits {
                    println!("{}", h.display());
                }
            }
        }
        Command::Mcp => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(scode::mcp::run_stdio())?;
        }
    }
    Ok(())
}