//! scode: gap+Elias-δ inverted indexes for identifier name locate.

pub mod corpus;
pub mod delta;
pub mod index;
pub mod intern;
pub mod lex;
pub mod mcp;

pub use corpus::{CorpusInput, LoadOptions, SourceDoc, TokenMode};
pub use index::{
    build_from_docs, build_index, index_and_maybe_write, Hit, Index, IndexStats, MemoryStore,
    SearchMultiResult,
};
pub use lex::Occurrence;