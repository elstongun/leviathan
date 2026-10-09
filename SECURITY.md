# Security policy

## Supported versions

Security fixes land on the latest minor release.

| Version | Supported |
|---|---|
| 0.2.x | yes |
| 0.1.x | no |

## Reporting a vulnerability

Please **do not open a public issue.** Use GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository, or email **joshuapbak@gmail.com**. Expect an
acknowledgement within 3 business days and a
fix or mitigation plan within 30 days.

## Threat model

Leviathan is built to run **next to sensitive data**, on a laptop or a
server you control.

- **No network unless you ask for it.** Indexing, search, `mcp` and every
  `memory` command read files and stdin, speak stdio and open no sockets.
  Only `serve --http` listens, and only `mcp --remote` and
  `memory briefing --remote` connect out, to the URL you give. All network
  code is behind the `remote` Cargo feature;
  `cargo install leviathan-index --no-default-features` builds a binary
  without it. Leviathan never connects to a database.
- **Indexes are read-only to agents.** The MCP server, the REST API and every
  query command open data indexes with `SQLITE_OPEN_READ_ONLY`. There is no
  SQL passthrough and no file access. Every tool is a fixed query shape, and
  user text reaches SQLite only as a bound parameter or a quoted FTS5 phrase.
- **Memory writes are explicit and contained.** With `--memory`, agents can
  `remember` and `forget` in a separate memory file
  (`~/.leviathan/memory.db` by default, mode 600), and nowhere else. No model runs
  inside Leviathan. `--memory-tools recall` serves memory read-only.
- **Secrets are refused.** `remember` and `memory import` reject text that
  looks like a private key, cloud or API token, JWT, credential in a URL,
  password assignment or long high-entropy string. The guard has no off
  switch. It is a safety net, not a guarantee: don't ask agents to remember
  secrets.
- **HTTP serving is authenticated by default.** `serve --http` requires a
  bearer token (generated with mode 600, compared in constant time, never
  printed) or OAuth 2.1. OAuth uses PKCE S256 only, an approval code printed
  on the server's own terminal, short-lived access tokens and rotating
  refresh tokens. Codes and tokens are stored only as SHA-256 hashes.
  The open OAuth endpoints are rate-limited per caller.
  `--auth none` is refused on non-loopback addresses. Requests with a
  foreign `Origin` header are rejected (DNS rebinding), bodies are capped
  at 4 MB, and Leviathan does not terminate TLS: put it behind a TLS proxy
  or tunnel to expose it ([docs/REMOTE.md](docs/REMOTE.md)).
- **Agent configs never hold tokens.** `wrap` writes environment variable
  references or a stdio bridge that reads the token when it runs, and backs
  up every file it changes. It refuses to write project files through
  symlinks, so a repository can't redirect it into your own config.
- **No telemetry.** Nothing is collected or sent anywhere.
- **Data stays where you put it.** An index or memory store is a single
  file. Its contents are exactly what was ingested or remembered; treat it
  with the same access controls as the source data.
- **`--sql` runs on your machine, with your input.** It's an indexing-time
  option for SQLite sources, opened read-only, and never exposed to agents.

In scope: query injection, crashes on malformed input, path or file access
through the MCP or REST surface, authentication or OAuth bypass, token
leakage, memory writes outside the memory file, secrets that pass the guard
in obvious forms, and denial of service from a crafted request.
