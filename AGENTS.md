# AGENTS.md

Guidance for coding agents working **on** Leviathan. (To *use* Leviathan from
an agent, install [`skills/leviathan/SKILL.md`](skills/leviathan/SKILL.md),
and [`skills/leviathan-memory/SKILL.md`](skills/leviathan-memory/SKILL.md)
for memory.)

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
| `src/mcp.rs` | MCP tools and JSON-RPC 2.0 session, shared by stdio and HTTP |
| `src/memory/` | Memory store: schema and migrations (`mod.rs`), `remember`/`forget` (`write.rs`), `recall`/briefing (`recall.rs`), secret guard (`guard.rs`) |
| `src/memory_cli.rs` | `leviathan memory …` subcommands and hook output formats |
| `src/http/` | `serve --http` (MCP, REST, OpenAPI) and the client used by `mcp --remote` and remote briefings (feature `remote`) |
| `src/oauth.rs` | OAuth 2.1 authorization server for `serve --http --auth oauth` (feature `remote`) |
| `src/wrap.rs` | `leviathan wrap`: per-agent config, hooks, rules and `--apply` merging (docs/AGENTS.md is the contract) |
| `examples/` | Example datasets and configs; `examples/tickets` backs the integration tests |
| `bench/synth` | Deterministic synthetic maintenance log + gold queries |
| `bench/run_bench.py`, `bench/report.py` | Benchmark harness and chart rendering |
| `bench/memory/` | Memory benchmark against markdown memory files, and its charts |

## Rules

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo test --workspace` must pass, with default features and with
  `--no-default-features` (no network code).
- The engine is domain-neutral. Nothing in `src/` may assume a kind of data
  (tickets, logs, maintenance); domain knowledge belongs in a config.
- stdout of `leviathan mcp` is protocol only. Diagnostics go to stderr.
- Data indexes are read-only to agents. The only writes an agent can make
  are memory `remember` and `forget`, in the separate memory file. Do not add
  SQL passthrough, file access or index writes to the MCP or REST surface.
- Memory writes are explicit: no model runs inside Leviathan, and nothing is
  written that a caller did not send. The secret guard has no off switch.
- Network code stays behind the `remote` feature and runs only for
  `serve --http`, `mcp --remote` and remote briefings. Tokens are never
  printed, logged or written into agent configs.
- Never drop information silently. Truncation is marked (`…`, `shown N of M`)
  and skipped input records are counted.
- Ranking changes must come with a benchmark run (`bench/run_bench.py`, or
  `bench/memory/run_memory_bench.py` for recall) and the before/after numbers
  in the PR. Labels in `queries.jsonl` come from the
  generator, never from the ranker's output.
- Do not commit real data. Fixtures and examples are synthetic.
