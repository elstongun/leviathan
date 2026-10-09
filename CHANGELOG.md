# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-09

### Added

- **Memory**: an optional read/write store for agents, in its own SQLite
  file (`--memory`, default `~/.leviathan/memory.db`). See
  [docs/MEMORY.md](docs/MEMORY.md).
  - MCP tools `remember`, `recall`, `forget` and `memory_describe`, served
    by `leviathan mcp --memory` beside the data tools or alone.
  - Keyed slots: writing a subject and key again replaces the current value
    and keeps the old one as history. Restatements merge, similar memories
    are returned as `related`, and subjects resolve aliases and typos.
  - `recall` ranks by relevance, recency, importance and use within a token
    budget, filters by subject, kind, namespace and date, and answers
    `as_of` and `history` questions. With no arguments it returns the
    session-start briefing.
  - A secret guard refuses keys, tokens, JWTs, credentials in URLs and
    high-entropy strings on every write and import.
  - `leviathan memory` subcommands: `init`, `remember`, `recall`,
    `briefing`, `forget`, `list`, `history`, `stats`, `export`, `import`
    (JSONL backups and markdown memory files) and `prune`.
  - `memory briefing --format claude|gemini|cursor|copilot|cline|json` for
    session-start hooks.
  - A `[memory]` section in `leviathan.toml`: path, namespaces, token
    budgets, recency half-lives per kind, pinned subjects.
- **Remote serving**: `leviathan serve --http ADDR` serves MCP over
  Streamable HTTP, a REST API (`/v1/<tool>`) and an OpenAPI document.
  Bearer-token auth by default; `--auth oauth` adds an OAuth 2.1
  authorization server (discovery, dynamic client registration, PKCE,
  rotating refresh tokens, approval code on the server terminal) for
  claude.ai and ChatGPT. Origin checks, body limits, per-caller rate limits
  on the OAuth endpoints and `--memory-tools recall` for read-only memory. See [docs/REMOTE.md](docs/REMOTE.md).
- `leviathan mcp --remote URL`: a stdio bridge to a remote server, for
  agents that only speak stdio; `memory briefing --remote URL` for hooks.
- `leviathan wrap` covers 24 agents and 12 hosted, API and framework
  targets (see [docs/AGENTS.md](docs/AGENTS.md)), with `--memory`, `--remote`,
  `--hooks` (session-start briefing), `--rules` (always-on instructions) and
  `--apply`, which merges into existing configs, backs up what it changes
  and is idempotent. `wrap` with no agent lists the targets. Function-calling
  schema exports for OpenAI, Anthropic and Gemini.
- Memory benchmark (`bench/memory`) against markdown memory files, with
  charts.
- Agent skill for memory (`skills/leviathan-memory/SKILL.md`).
- Cargo feature `remote` (default). `--no-default-features` builds without
  any network code.

### Changed

- MCP server code is shared between stdio and HTTP.
- `search` also accepts `where_list` (`["status=open"]`), for function-calling
  APIs whose schemas can't express the `where` object.

## [0.1.0] - 2026-10-05

### Added

- Generic record model: a field mapping (id, title, text, group, group name,
  date, filters, display fields, placeholder values, rank boosts) from
  `leviathan.toml`, CLI flags, or inference. Field paths support nesting
  (`a.b`), arrays (`items[].x`) and literal dotted keys.
- Sources: JSONL / NDJSON, JSON arrays, CSV / TSV (all optionally gzip),
  SQLite (`--sql` or the only table, JSON cells parsed), stdin with format
  sniffing, and directories.
- `leviathan init`: samples the data, profiles every field and writes a
  commented `leviathan.toml` with the reason for each choice (`--json` for
  agents).
- `leviathan index`: streaming, atomic SQLite/FTS5 build; skipped when sources
  and mapping are unchanged; unusable records are counted and reported
  (`--strict` to fail instead).
- `leviathan upsert` / `leviathan delete`: incremental changes by id with exact
  facet and group counts.
- `leviathan search`: BM25 ranking with configurable boosts, quoted phrases
  and `-exclusions`, group scoping with name resolution and ambiguity
  refusal, labeled fallback to other groups, `--where` facet filters, and
  `--since`/`--until` date ranges; `--sort newest` and `--offset` paging.
- `recent`, `resolve`, `get`, `describe` commands; compact text output by
  default, `--json` everywhere.
- `leviathan mcp`: read-only stdio MCP server with four tools whose
  descriptions include a summary of the open dataset.
- `leviathan wrap <agent>`: prints MCP configuration for Claude Code, Codex,
  Cursor, VS Code, Gemini CLI, Windsurf, or any stdio client.
- Agent skill (`skills/leviathan/SKILL.md`), examples (`examples/tickets`,
  `examples/maintenance`), and a deterministic synthetic benchmark
  (`bench/`).

[Unreleased]: https://github.com/elstongun/leviathan/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/elstongun/leviathan/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/elstongun/leviathan/releases/tag/v0.1.0
