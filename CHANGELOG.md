# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/elstongun/leviathan/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/elstongun/leviathan/releases/tag/v0.1.0
