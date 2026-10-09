# Agent setup

`leviathan wrap <target>` prints everything one agent needs, and `--apply`
writes it. Run `leviathan wrap` alone to list every target.

```bash
leviathan wrap cursor                                   # the data index over MCP
leviathan wrap cursor --memory                          # plus memory tools
leviathan wrap cursor --memory --hooks --rules          # plus the session-start briefing and the always-on rule
leviathan wrap cursor --memory --hooks --rules --apply  # write it (user scope; --project for this repo)
leviathan wrap cursor --remote https://memory.example.com/mcp --hooks --rules   # a shared remote server
```

| Flag | What it adds |
|---|---|
| `--memory` | The `remember`, `recall`, `forget` and `memory_describe` tools, on the default store or `--memory=PATH` |
| `--remote URL` | Connect to a `leviathan serve --http` server instead of starting a local one ([REMOTE.md](REMOTE.md)) |
| `--hooks` | A session-start hook that injects the memory briefing, for agents that have one |
| `--rules` | The always-on instruction to recall first, remember durable facts with keys, and never store secrets |
| `--apply` | Write the files. JSON is merged (other servers and hooks are kept), TOML and markdown get a marked block, and each changed file is backed up to `.bak` with a diff printed. Re-running changes nothing. Files that aren't plain JSON (comments) are refused, not rewritten |
| `--project` | With `--apply`: write the project files instead of your user config |
| `--token-env NAME` | Environment variable for the remote token (default `LEVIATHAN_TOKEN`) |

Tokens never go into a config file. Remote configs either reference the
variable (`${LEVIATHAN_TOKEN}`, `bearer_token_env_var`, VS Code's secret
prompt) or run the stdio bridge, which reads the token at run time.

## Desktop and CLI agents

| Target | Agent | MCP config | Session-start hook | Rule |
|---|---|---|---|---|
| `claude` | Claude Code | `.mcp.json`, or `claude mcp add --scope user` | `SessionStart` in `.claude/settings.json` | `CLAUDE.md` |
| `codex` | OpenAI Codex (CLI, IDE, desktop) | `~/.codex/config.toml` | `SessionStart` in `~/.codex/hooks.json` | `AGENTS.md` |
| `cursor` | Cursor | `~/.cursor/mcp.json` | `sessionStart` in `~/.cursor/hooks.json` | `.cursor/rules/leviathan-memory.mdc` |
| `vscode` | VS Code + GitHub Copilot | user `mcp.json` or `.vscode/mcp.json` | `SessionStart` in `.github/hooks/` (preview) | `.github/copilot-instructions.md` |
| `copilot-cli` | GitHub Copilot CLI and coding agent | `~/.copilot/mcp-config.json` or `.github/mcp.json` | `sessionStart` in `.github/hooks/` | `AGENTS.md` |
| `gemini` | Gemini CLI | `~/.gemini/settings.json` | `SessionStart` in the same file | `GEMINI.md` |
| `cline` | Cline | `cline_mcp_settings.json` | `TaskStart` script | `.clinerules/leviathan-memory.md` |
| `kiro` | Kiro IDE and CLI | `~/.kiro/settings/mcp.json` | `SessionStart` in `.kiro/hooks/` | `.kiro/steering/leviathan-memory.md` |
| `windsurf` | Windsurf (Cascade) | `~/.codeium/windsurf/mcp_config.json` | — | `.windsurf/rules/leviathan-memory.md` |
| `devin` | Devin Desktop and CLI | `~/.config/devin/mcp_config.json` | — | `.devin/rules/leviathan-memory.md` |
| `roo` | Roo Code | `.roo/mcp.json` | — | `.roo/rules/leviathan-memory.md` |
| `kilo` | Kilo Code | `~/.config/kilo/kilo.json` | — | `AGENTS.md` |
| `continue` | Continue | `~/.continue/mcpServers/leviathan.yaml` | — | `.continue/rules/leviathan-memory.md` |
| `zed` | Zed | `~/.config/zed/settings.json` (`context_servers`) | — | `AGENTS.md` |
| `junie` | JetBrains Junie | `~/.junie/mcp/mcp.json` | — | `AGENTS.md` |
| `jetbrains` | JetBrains AI Assistant | Settings › Tools › AI Assistant › MCP | — | `AGENTS.md` |
| `goose` | Goose | `config.yaml` extensions (printed, pasted) | — | `.goosehints` |
| `opencode` | opencode | `~/.config/opencode/opencode.json` | — | `AGENTS.md` |
| `amp` | Amp | `~/.config/amp/settings.json` | — | `AGENTS.md` |
| `warp` | Warp | `~/.warp/.mcp.json` | — | `AGENTS.md` |
| `amazon-q` | Amazon Q Developer CLI (now Kiro CLI) | `~/.aws/amazonq/mcp.json` | — | `AGENTS.md` |
| `claude-desktop` | Claude Desktop | `claude_desktop_config.json` | — | system prompt |
| `lmstudio` | LM Studio | `~/.lmstudio/mcp.json` | — | system prompt |
| `generic` | Any MCP client | stdio `command` + `args` | — | `AGENTS.md` |

Agents without a context-injecting hook still get the briefing: the rule
tells them to call `recall` with no arguments first, and the tool's
description says the same.

Paths are where each agent reads its config as of this release, from each
agent's own documentation. Agents move fast. If one has changed, the
printed entry still shows the shape to paste, and an issue is welcome.

## Hosted apps, APIs and frameworks

These connect to a server reachable over HTTPS (`leviathan serve --http`, [REMOTE.md](REMOTE.md)) and need `--remote URL`:

| Target | Connects |
|---|---|
| `claude-ai` | claude.ai custom connector (OAuth) |
| `chatgpt` | ChatGPT developer-mode app (OAuth) |
| `anthropic-api` | Anthropic Messages API `mcp_servers` with `mcp_toolset` |
| `openai-api` | OpenAI Responses API `{"type": "mcp"}` tool |
| `openai-agents` | OpenAI Agents SDK `MCPServerStreamableHttp` |
| `langchain` | LangChain / LangGraph via `langchain-mcp-adapters` |
| `vercel-ai` | Vercel AI SDK MCP client |
| `rest` | Plain HTTP + OpenAPI (Custom GPT Actions, anything with HTTP) |

Without a server:

| Target | Prints |
|---|---|
| `openai-tools` | OpenAI function-calling tool schemas |
| `anthropic-tools` | Anthropic tool-use schemas |
| `gemini-tools` | Gemini function declarations |
| `shell` | Instructions for agents with a terminal but no MCP (Aider, OpenHands, scripts): the CLI commands |

Run the schemas' calls with `leviathan memory …` or `POST /v1/<tool>`.

## Smoke test

After setup, start a new session and ask the agent:

1. "What do you remember about me?" It should call `recall` (or show the briefing), and on a new store say memory is empty.
2. "Remember that I prefer pnpm over npm." It should call `remember` with a subject and a key such as `package_manager`.
3. "Actually, I switched to bun." It should `remember` under the same key, and the reply says the old value was replaced.
4. In a new session: "Which package manager do I use?" Expect bun, with no mention of pnpm.

From a shell, `leviathan memory list` and `leviathan memory history -s <you> -k package_manager` show what was written.
