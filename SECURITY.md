# Security policy

## Supported versions

Security fixes land on the latest minor release.

| Version | Supported |
|---|---|
| 0.1.x | yes |

## Reporting a vulnerability

Please **do not open a public issue.** Use GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
on this repository, or email **joshuapbak@gmail.com**. Expect an
acknowledgement within 3 business days and a
fix or mitigation plan within 30 days.

## Threat model

Leviathan is built to run **next to sensitive data**, on a laptop or a
server you control.

- **No network.** The binary opens no sockets and never connects to a
  database. It reads files and stdin; the MCP server speaks stdio only.
- **Read-only serving.** The MCP server and every query command open the
  index with `SQLITE_OPEN_READ_ONLY`. There are no write tools, no SQL
  passthrough and no file access. Every tool is a fixed query shape, and user
  text reaches SQLite only as a bound parameter or a quoted FTS5 phrase.
- **No telemetry.** Nothing is collected or sent anywhere.
- **Data stays where you put it.** The index is a single file. Its contents
  are exactly what you ingested, and the effective field mapping is stored
  inside it; treat it with the same access controls as the source data.
- **`--sql` runs on your machine, with your input.** It's an indexing-time
  option for SQLite sources, opened read-only, and never exposed to agents
  through MCP.

In scope: query injection, crashes on malformed input, path or file access
through the MCP surface, and denial of service from a crafted query.
