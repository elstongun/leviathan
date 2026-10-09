# Remote: `leviathan serve --http`

`leviathan serve --http` serves the same tools as `leviathan mcp` over the
network, so agents on other machines and hosted apps can share one memory
store (and, optionally, one data index).

```bash
leviathan serve --http 127.0.0.1:7777 --memory                    # memory only
leviathan serve --http 127.0.0.1:7777 --memory --index tickets.db # memory and a data index
```

| Endpoint | What it is |
|---|---|
| `POST /mcp` | MCP Streamable HTTP (protocol 2025-11-25 down to 2024-11-05), JSON responses. `GET /mcp` is 405: the server never pushes |
| `POST /v1/<tool>` | REST: the tool's JSON arguments in, its JSON result out. `?format=text` returns the agent-facing text |
| `GET /openapi.json` | OpenAPI 3.1 for `/v1/*` (Custom GPT Actions, code generators). It describes tools only, never the data |
| `GET /healthz` | `{"ok": true, "version": …}` |
| `/.well-known/oauth-*`, `/oauth/*` | OAuth 2.1, with `--auth oauth` |

The server opens the data index read-only, like every other command. Memory
is the only thing it writes. `--memory-tools recall` serves memory read-only.

## Authentication

| `--auth` | Who gets in |
|---|---|
| `token` (default) | Requests with `Authorization: Bearer <token>`. The token lives in `--token-file` (default `leviathan.token` beside the memory file), which is created with mode 600 on first start. The server prints the file's path, never the token |
| `oauth` | The token, or OAuth 2.1 access tokens issued to hosted apps (below). Needs `--public-url https://…` unless bound to loopback |
| `none` | Everyone. Refused on anything but a loopback address |

Tokens are compared in constant time. Browser requests are checked against
their `Origin`. Loopback origins, the `--public-url` origin and each
`--allow-origin` pass, and anything else gets a 403.

Clients read the token from `$LEVIATHAN_TOKEN` (or `--token-file`, or the
variable named by `--token-env`):

```bash
export LEVIATHAN_TOKEN="$(cat ~/.leviathan/leviathan.token)"   # on each client machine
```

## TLS

The server speaks plain HTTP. On anything beyond localhost or a private
network, put TLS in front and keep Leviathan bound to `127.0.0.1`:

```bash
# Caddy: automatic certificates
caddy reverse-proxy --from memory.example.com --to 127.0.0.1:7777

# Or a tunnel, no open ports:
cloudflared tunnel --url http://127.0.0.1:7777
tailscale funnel 7777            # or `tailscale serve` for your tailnet only
```

Leviathan warns at startup if it listens beyond loopback without an `https://` public URL.

## Connecting agents

Most desktop agents connect with `leviathan wrap`:

```bash
leviathan wrap claude --remote https://memory.example.com/mcp --hooks --rules
leviathan wrap cursor --remote https://memory.example.com/mcp --hooks --rules --apply
```

- Agents whose configs can read the token from an environment variable or a secret store are set up to connect over HTTP natively: Claude Code, Cursor, Windsurf, Devin, Warp, Codex (`bearer_token_env_var`), VS Code (`inputs`), Continue (`secrets`) and Goose (`env_keys`).
- Every other agent gets the stdio bridge, `leviathan mcp --remote URL`. It forwards MCP over HTTP and reads the token from the environment at run time, so no config file ever holds it.
- The briefing hooks call `leviathan memory briefing --remote URL`.

## Hosted apps (OAuth)

claude.ai connectors and ChatGPT apps sign in with OAuth. Leviathan includes
a minimal single-user OAuth 2.1 authorization server, built to what these
apps use:

- Authorization-server and protected-resource metadata (RFC 8414 and RFC 9728).
- Dynamic client registration (RFC 7591).
- Authorization code with PKCE S256 only, and refresh tokens that rotate on every use.
- Access tokens last an hour and refresh tokens 30 days. Both are stored only as SHA-256 hashes, in `leviathan-auth.db` beside the token file.

```bash
leviathan serve --http 127.0.0.1:7777 --memory --auth oauth --public-url https://memory.example.com
```

1. Add the connector:
   - **claude.ai:** Settings › Connectors › Add custom connector, with URL `https://memory.example.com/mcp`. On Team and Enterprise plans, an owner adds it under Organization settings.
   - **ChatGPT:** Settings › Apps & Connectors › Advanced › Developer mode, then Create, with the same URL and OAuth.
2. The app registers itself and opens Leviathan's approval page.
3. The server's log prints a one-time approval code:
   ```text
   [leviathan] oauth: "Claude" asks to connect. Approval code: 4821-0937 (valid 10 minutes)
   ```
   Type it into the page. Only whoever can read the server's log can approve a connection. Five wrong codes cancel the request.

The OAuth endpoints are open to anyone who can reach the server, so each
caller gets a budget: 10 registrations an hour, 30 authorization requests
and 30 approval attempts per 10 minutes, 120 token requests per 10 minutes,
and at most 5 pending approvals at once. Registered clients that never
got a token are dropped after a day once the client table is full. The
caller is the peer address; behind a proxy on the same machine (Caddy,
cloudflared, tailscale) it is the last `X-Forwarded-For` entry the proxy
adds. A proxy on another machine makes every caller look like one, so run
the proxy beside Leviathan.

`leviathan wrap claude-ai --remote URL` and `leviathan wrap chatgpt --remote URL` print these steps.

## APIs and frameworks

```bash
leviathan wrap anthropic-api --remote https://memory.example.com/mcp   # Messages API mcp_servers + mcp_toolset
leviathan wrap openai-api --remote https://memory.example.com/mcp      # Responses API {"type": "mcp"} tool
leviathan wrap openai-agents --remote …                                # Agents SDK MCPServerStreamableHttp
leviathan wrap langchain --remote …                                    # langchain-mcp-adapters
leviathan wrap vercel-ai --remote …                                    # AI SDK MCP client
leviathan wrap rest --remote …                                         # curl + OpenAPI
leviathan wrap openai-tools                                            # function schemas, no server needed
```

Requests from hosted APIs come from the provider's servers, so the URL has to be public (HTTPS) and the token is sent per request.

## Running it as a service

```ini
# ~/.config/systemd/user/leviathan.service
[Unit]
Description=Leviathan memory server

[Service]
ExecStart=%h/.cargo/bin/leviathan serve --http 127.0.0.1:7777 --memory
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now leviathan
```

## Building without it

The HTTP server and client are behind the default `remote` feature.
`cargo install leviathan-index --no-default-features` builds a binary with
no networking code at all. `serve` and `--remote` then explain that they're
unavailable.
