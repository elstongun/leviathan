//! Leviathan: deep memory for agents over large datasets.
//!
//! Index JSONL, JSON, CSV or SQLite records into a single SQLite/FTS5 file,
//! then answer questions with a few ranked, cited cards instead of the raw
//! data. A small field mapping (`leviathan.toml`, CLI flags, or inference)
//! says which fields are the id, title, searchable text, group, date and
//! filters. Use it from the CLI, as an MCP server, or as a library:
//!
//! ```no_run
//! use leviathan::{card::CardOptions, query::{SearchRequest, Store}};
//!
//! let store = Store::open("leviathan.db".as_ref(), CardOptions::default())?;
//! let hits = store.search(&SearchRequest {
//!     group: Some("acme".into()),
//!     query: "login loops after password reset".into(),
//!     filters: vec![("status".into(), "closed".into())],
//!     limit: 5,
//!     fallback: true,
//!     ..Default::default()
//! })?;
//! println!("{}", leviathan::render::search(&hits));
//! # anyhow::Ok(())
//! ```

pub mod card;
pub mod config;
pub mod fields;
pub mod index;
pub mod infer;
pub mod mcp;
pub mod query;
pub mod render;
pub mod source;
pub mod text;
pub mod wrap;

/// Current UTC time as RFC 3339, without a date-time dependency.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    text::format_unix(secs)
}
