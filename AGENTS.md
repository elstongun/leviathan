# AGENTS.md

Guidance for coding agents working **on** Leviathan. (To *use* Leviathan from
an agent, install [`skills/leviathan/SKILL.md`](skills/leviathan/SKILL.md).)

## Layout

| Path | What lives there |
|---|---|
| `src/config.rs` | `leviathan.toml` model, validation, CLI field flags (docs/CONFIG.md is the contract) |
| `src/fields.rs` | Field paths (`a.b`, `items[].x`) and the mapping that turns a record into what is indexed |
| `src/source.rs` | Readers: JSONL, JSON array, CSV/TSV, SQLite, stdin, gzip, directory discovery |
| `src/infer.rs` | `leviathan init`: field profiling and mapping proposal |
| `src/text.rs` | Query parsing, safe FTS5 expressions, tokens, date normalization |
| `src/index.rs` | Streaming atomic build, upsert/delete, facet counts, SQLite schema |
| `src/query.rs` | Group resolution tiers, scoped/filtered BM25 search, browse, describe, get |
| `src/card.rs` | Result-card presentation and snippets |
| `src/render.rs` | Compact text output (the default for agents) |
| `src/mcp.rs` | stdio MCP server (JSON-RPC 2.0) |
| `examples/` | Example datasets and configs; `examples/tickets` backs the integration tests |
| `bench/synth` | Deterministic synthetic maintenance log + gold queries |
| `bench/run_bench.py`, `bench/report.py` | Benchmark harness and chart rendering |

## Rules

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` must pass.
- The engine is domain-neutral. Nothing in `src/` may assume a kind of data
  (tickets, logs, maintenance); domain knowledge belongs in a config.
- stdout of `leviathan mcp` is protocol only. Diagnostics go to stderr.
- The server is read-only by design. Do not add write tools, SQL passthrough
  or file access to the MCP surface.
- Never drop information silently. Truncation is marked (`…`, `shown N of M`)
  and skipped input records are counted.
- Ranking changes must come with a benchmark run (`bench/run_bench.py`) and
  the before/after numbers in the PR. Labels in `queries.jsonl` come from the
  generator, never from the ranker's output.
- Do not commit real data. Fixtures and examples are synthetic.
