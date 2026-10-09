//! `leviathan wrap <target>`: the setup that connects Leviathan to an agent,
//! an app, an API or a framework. It prints by default; `--apply` merges the
//! MCP entry (and, with --hooks / --rules, the hook and rule) into the
//! agent's own files, showing what changed and keeping a `.bak` copy.
//!
//! Formats follow each agent's documentation as of October 2026; the golden
//! tests in `tests/wrap.rs` pin them so a change is deliberate.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value, json};

use crate::mcp::{MemoryTools, tool_definitions};

#[derive(Debug, Clone, Default, clap::Args)]
pub struct WrapFlags {
    /// A remote Leviathan (`leviathan serve --http`): its MCP URL, e.g. https://mem.example.com/mcp.
    #[arg(long, value_name = "URL")]
    pub remote: Option<String>,
    /// Environment variable that holds the bearer token for --remote.
    #[arg(long, value_name = "NAME", default_value = "LEVIATHAN_TOKEN")]
    pub token_env: String,
    /// Also print the session-start hook that injects the memory briefing.
    #[arg(long)]
    pub hooks: bool,
    /// Also print the always-on rule that tells the agent how to use memory.
    #[arg(long)]
    pub rules: bool,
    /// Write it into the agent's files (merging, with a .bak copy) instead of printing.
    #[arg(long)]
    pub apply: bool,
    /// With --apply: use the project's files (./.cursor, ./.vscode, ...) instead of the user's.
    #[arg(long)]
    pub project: bool,
}

pub struct Context {
    pub exe: PathBuf,
    pub index: PathBuf,
    pub memory: Option<PathBuf>,
    pub flags: WrapFlags,
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// `{"mcpServers": {"leviathan": {...}}}` with an optional stdio `type`.
    McpServers {
        stdio_type: Option<&'static str>,
        http_type: Option<&'static str>,
        url_key: &'static str,
        native_remote: bool,
    },
    /// VS Code: `servers`, `type` stdio/http, `${input:...}` secrets.
    VsCode,
    /// Codex: `[mcp_servers.leviathan]` in TOML.
    Codex,
    /// opencode and Kilo Code: `mcp`, `local` with a command array.
    Opencode,
    /// Zed: `context_servers`.
    Zed,
    /// Amp: `amp.mcpServers`.
    Amp,
    /// Continue: one YAML file per server.
    Continue,
    /// Goose: `extensions` in config.yaml.
    Goose,
}

#[derive(Clone, Copy, PartialEq)]
enum Hook {
    /// `{"hooks": {"SessionStart": [{"matcher", "hooks": [{type, command}]}]}}`, plain text out.
    ClaudeStyle { matcher: &'static str, format: &'static str },
    /// Cursor `hooks.json` v1, `sessionStart`, `additional_context` out.
    Cursor,
    /// VS Code Copilot `.github/hooks/*.json`, `SessionStart` without matcher nesting.
    VsCode,
    /// Copilot CLI `.github/hooks/*.json` v1, `sessionStart` with bash/powershell.
    CopilotCli,
    /// Kiro `.kiro/hooks/*.json`, `SessionStart` trigger, plain text out.
    Kiro,
    /// Cline: an executable `.clinerules/hooks/TaskStart`.
    Cline,
}

#[derive(Clone, Copy, PartialEq)]
enum RuleFile {
    /// Append a marked block to a markdown file.
    Block(&'static str),
    /// A file of its own, with frontmatter.
    Own(&'static str, &'static str),
}

struct Agent {
    name: &'static str,
    title: &'static str,
    shape: Shape,
    /// User-level config (`~` expanded); `None` when the agent only has a UI.
    user: Option<&'static str>,
    project: Option<&'static str>,
    hook: Option<(Hook, &'static str, &'static str)>,
    rule: Option<RuleFile>,
    note: &'static str,
}

const fn servers(
    stdio_type: Option<&'static str>,
    http_type: Option<&'static str>,
    url_key: &'static str,
    native_remote: bool,
) -> Shape {
    Shape::McpServers { stdio_type, http_type, url_key, native_remote }
}

const AGENTS_MD: RuleFile = RuleFile::Block("AGENTS.md");

const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        title: "Claude Code",
        shape: servers(None, Some("http"), "url", true),
        user: None,
        project: Some(".mcp.json"),
        hook: Some((
            Hook::ClaudeStyle { matcher: "startup|resume|clear|compact", format: "text" },
            "~/.claude/settings.json",
            ".claude/settings.json",
        )),
        rule: Some(RuleFile::Block("CLAUDE.md")),
        note: "Or one command (user scope, every project):",
    },
    Agent {
        name: "claude-desktop",
        title: "Claude Desktop",
        shape: servers(None, None, "url", false),
        user: Some("@claude-desktop"),
        project: None,
        hook: None,
        rule: None,
        note: "Restart Claude Desktop after editing. For remote memory it runs the local bridge; claude.ai connectors also appear here.",
    },
    Agent {
        name: "codex",
        title: "OpenAI Codex (CLI, IDE extension, desktop)",
        shape: Shape::Codex,
        user: Some("~/.codex/config.toml"),
        project: Some(".codex/config.toml"),
        hook: Some((
            Hook::ClaudeStyle { matcher: "startup|resume", format: "text" },
            "~/.codex/hooks.json",
            ".codex/hooks.json",
        )),
        rule: Some(AGENTS_MD),
        note: "Project config applies to trusted projects only; hooks need trust too.",
    },
    Agent {
        name: "cursor",
        title: "Cursor",
        shape: servers(None, None, "url", true),
        user: Some("~/.cursor/mcp.json"),
        project: Some(".cursor/mcp.json"),
        hook: Some((Hook::Cursor, "~/.cursor/hooks.json", ".cursor/hooks.json")),
        rule: Some(RuleFile::Own(
            ".cursor/rules/leviathan-memory.mdc",
            "---\ndescription: How to use Leviathan memory\nalwaysApply: true\n---\n",
        )),
        note: "Background agents read the project file.",
    },
    Agent {
        name: "vscode",
        title: "VS Code with GitHub Copilot",
        shape: Shape::VsCode,
        user: Some("@vscode-user"),
        project: Some(".vscode/mcp.json"),
        hook: Some((
            Hook::VsCode,
            "~/.copilot/hooks/leviathan-vscode.json",
            ".github/hooks/leviathan-vscode.json",
        )),
        rule: Some(RuleFile::Block(".github/copilot-instructions.md")),
        note: "Hooks are a preview feature of the local agent.",
    },
    Agent {
        name: "copilot-cli",
        title: "GitHub Copilot CLI (and Copilot coding agent via .github/mcp.json)",
        shape: servers(Some("local"), Some("http"), "url", false),
        user: Some("~/.copilot/mcp-config.json"),
        project: Some(".github/mcp.json"),
        hook: Some((Hook::CopilotCli, "~/.copilot/hooks/leviathan.json", ".github/hooks/leviathan.json")),
        rule: Some(AGENTS_MD),
        note: "",
    },
    Agent {
        name: "windsurf",
        title: "Windsurf (Cascade)",
        shape: servers(None, None, "serverUrl", true),
        user: Some("~/.codeium/windsurf/mcp_config.json"),
        project: None,
        hook: None,
        rule: Some(RuleFile::Own(".windsurf/rules/leviathan-memory.md", "---\ntrigger: always_on\n---\n")),
        note: "Windsurf hooks cannot inject context, so the always-on rule does the briefing.",
    },
    Agent {
        name: "devin",
        title: "Devin Desktop (formerly Windsurf) and Devin CLI",
        shape: servers(None, None, "serverUrl", true),
        user: Some("~/.config/devin/mcp_config.json"),
        project: Some(".devin/mcp_config.json"),
        hook: None,
        rule: Some(RuleFile::Own(".devin/rules/leviathan-memory.md", "---\ntrigger: always_on\n---\n")),
        note: "",
    },
    Agent {
        name: "gemini",
        title: "Gemini CLI",
        shape: servers(None, None, "httpUrl", false),
        user: Some("~/.gemini/settings.json"),
        project: Some(".gemini/settings.json"),
        hook: Some((
            Hook::ClaudeStyle { matcher: "startup", format: "gemini" },
            "~/.gemini/settings.json",
            ".gemini/settings.json",
        )),
        rule: Some(RuleFile::Block("GEMINI.md")),
        note: "",
    },
    Agent {
        name: "cline",
        title: "Cline",
        shape: servers(None, Some("streamableHttp"), "url", false),
        user: Some("~/.cline/data/settings/cline_mcp_settings.json"),
        project: None,
        hook: Some((Hook::Cline, "~/Documents/Cline/Hooks/TaskStart", ".clinerules/hooks/TaskStart")),
        rule: Some(RuleFile::Own(".clinerules/leviathan-memory.md", "")),
        note: "In the editor: MCP Servers > Configure > paste the entry. Enable hooks in Cline's settings.",
    },
    Agent {
        name: "roo",
        title: "Roo Code",
        shape: servers(None, Some("streamable-http"), "url", false),
        user: None,
        project: Some(".roo/mcp.json"),
        hook: None,
        rule: Some(RuleFile::Own(".roo/rules/leviathan-memory.md", "")),
        note: "Global servers: MCP settings > Edit Global MCP.",
    },
    Agent {
        name: "kilo",
        title: "Kilo Code",
        shape: Shape::Opencode,
        user: Some("~/.config/kilo/kilo.json"),
        project: Some("kilo.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "",
    },
    Agent {
        name: "continue",
        title: "Continue",
        shape: Shape::Continue,
        user: Some("~/.continue/mcpServers/leviathan.yaml"),
        project: Some(".continue/mcpServers/leviathan.yaml"),
        hook: None,
        rule: Some(RuleFile::Own(".continue/rules/leviathan-memory.md", "")),
        note: "",
    },
    Agent {
        name: "zed",
        title: "Zed",
        shape: Shape::Zed,
        user: Some("~/.config/zed/settings.json"),
        project: Some(".zed/settings.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "",
    },
    Agent {
        name: "jetbrains",
        title: "JetBrains AI Assistant",
        shape: servers(None, None, "url", false),
        user: None,
        project: None,
        hook: None,
        rule: Some(AGENTS_MD),
        note: "Settings > Tools > AI Assistant > Model Context Protocol (MCP) > Add > As JSON, and paste the entry.",
    },
    Agent {
        name: "junie",
        title: "JetBrains Junie (IDE and CLI)",
        shape: servers(None, None, "url", false),
        user: Some("~/.junie/mcp/mcp.json"),
        project: Some(".junie/mcp/mcp.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "",
    },
    Agent {
        name: "goose",
        title: "Goose",
        shape: Shape::Goose,
        user: None,
        project: None,
        hook: None,
        rule: Some(RuleFile::Block(".goosehints")),
        note: "Add under `extensions:` in ~/.config/goose/config.yaml (or run `goose configure` > Add Extension).",
    },
    Agent {
        name: "kiro",
        title: "Kiro (IDE and CLI; formerly Amazon Q Developer CLI)",
        shape: servers(None, None, "url", false),
        user: Some("~/.kiro/settings/mcp.json"),
        project: Some(".kiro/settings/mcp.json"),
        hook: Some((Hook::Kiro, "~/.kiro/hooks/leviathan.json", ".kiro/hooks/leviathan.json")),
        rule: Some(RuleFile::Own(".kiro/steering/leviathan-memory.md", "---\ninclusion: always\n---\n")),
        note: "",
    },
    Agent {
        name: "amazon-q",
        title: "Amazon Q Developer CLI (now Kiro CLI)",
        shape: servers(None, None, "url", false),
        user: Some("~/.aws/amazonq/mcp.json"),
        project: Some(".amazonq/mcp.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "Kiro reads these legacy paths too; `leviathan wrap kiro` is the current setup.",
    },
    Agent {
        name: "opencode",
        title: "opencode",
        shape: Shape::Opencode,
        user: Some("~/.config/opencode/opencode.json"),
        project: Some("opencode.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "",
    },
    Agent {
        name: "amp",
        title: "Amp",
        shape: Shape::Amp,
        user: Some("~/.config/amp/settings.json"),
        project: Some(".amp/settings.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "Workspace servers need `amp mcp approve leviathan`.",
    },
    Agent {
        name: "warp",
        title: "Warp",
        shape: servers(None, None, "url", true),
        user: Some("~/.warp/.mcp.json"),
        project: Some(".warp/.mcp.json"),
        hook: None,
        rule: Some(AGENTS_MD),
        note: "Or Settings > AI > MCP servers > Add, and paste the entry.",
    },
    Agent {
        name: "lmstudio",
        title: "LM Studio",
        shape: servers(None, None, "url", false),
        user: Some("~/.lmstudio/mcp.json"),
        project: None,
        hook: None,
        rule: None,
        note: "Or Program > Install > Edit mcp.json.",
    },
    Agent {
        name: "generic",
        title: "Any MCP client",
        shape: servers(None, None, "url", false),
        user: None,
        project: None,
        hook: None,
        rule: Some(AGENTS_MD),
        note: "Most clients take this `mcpServers` shape.",
    },
];

/// Targets that are not desktop agents: hosted apps, APIs, frameworks.
const OTHER: &[(&str, &str)] = &[
    ("claude-ai", "claude.ai custom connector (remote, OAuth)"),
    ("chatgpt", "ChatGPT developer-mode app (remote, OAuth)"),
    ("anthropic-api", "Anthropic Messages API MCP connector"),
    ("openai-api", "OpenAI Responses API remote MCP tool"),
    ("openai-agents", "OpenAI Agents SDK"),
    ("langchain", "LangChain / LangGraph (langchain-mcp-adapters)"),
    ("vercel-ai", "Vercel AI SDK"),
    ("rest", "Plain HTTP / OpenAPI (Custom GPT Actions, any framework)"),
    ("openai-tools", "OpenAI function-calling schemas"),
    ("anthropic-tools", "Anthropic tool-use schemas"),
    ("gemini-tools", "Gemini function declarations"),
    ("shell", "Agents with a terminal but no MCP (Aider, OpenHands, scripts)"),
];

pub fn targets() -> Vec<&'static str> {
    AGENTS.iter().map(|a| a.name).chain(OTHER.iter().map(|(n, _)| *n)).collect()
}

/// Every target with a one-line description, for docs and `--help`.
pub fn catalog() -> Vec<(&'static str, &'static str)> {
    AGENTS.iter().map(|a| (a.name, a.title)).chain(OTHER.iter().copied()).collect()
}

fn agent(name: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.name == name)
}

fn home() -> PathBuf {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|v| std::env::var_os(v).filter(|h| !h.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}

/// Project files are relative to the repository, which may not be trusted:
/// a symlinked `.cursor/mcp.json` must not redirect a write into the user's
/// own config. Paths under the home directory are the user's and may be
/// symlinks (dotfile managers).
fn refuse_project_symlinks(path: &Path) -> Result<()> {
    if path.is_absolute() {
        return Ok(());
    }
    let mut at = PathBuf::new();
    for part in path.components() {
        at.push(part);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!(
                "refusing to write through the symlink {}; check where it points and edit by hand",
                at.display()
            );
        }
    }
    Ok(())
}

fn expand(path: &str) -> PathBuf {
    match path {
        "@claude-desktop" => {
            if cfg!(target_os = "macos") {
                home().join("Library/Application Support/Claude/claude_desktop_config.json")
            } else if cfg!(windows) {
                appdata().join("Claude").join("claude_desktop_config.json")
            } else {
                home().join(".config/Claude/claude_desktop_config.json")
            }
        }
        "@vscode-user" => {
            if cfg!(target_os = "macos") {
                home().join("Library/Application Support/Code/User/mcp.json")
            } else if cfg!(windows) {
                appdata().join("Code").join("User").join("mcp.json")
            } else {
                home().join(".config/Code/User/mcp.json")
            }
        }
        p => match p.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(p),
        },
    }
}

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Roaming"))
}

fn display_path(path: &str) -> String {
    match path {
        "@claude-desktop" => "claude_desktop_config.json (macOS: ~/Library/Application Support/Claude/, Windows: %APPDATA%\\Claude\\)".into(),
        "@vscode-user" => "your user mcp.json (Command Palette: MCP: Open User Configuration)".into(),
        p => p.to_string(),
    }
}

/// Quote for a hook command line (sh and cmd both accept double quotes).
fn quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:=@+\\".contains(c)) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

impl Context {
    fn exe(&self) -> String {
        self.exe.display().to_string()
    }

    /// Arguments for the stdio server (or the stdio bridge with --remote).
    fn stdio_args(&self) -> Vec<String> {
        match &self.flags.remote {
            Some(url) => {
                let mut args = vec!["mcp".into(), "--remote".into(), url.clone()];
                args.extend(self.token_env_arg());
                args
            }
            None => {
                let mut args = vec!["mcp".into(), "--index".into(), self.index.display().to_string()];
                if let Some(m) = &self.memory {
                    args.push(format!("--memory={}", m.display()));
                }
                args
            }
        }
    }

    /// The bridge reads the token from the environment, so configs that
    /// cannot interpolate secrets still never contain one.
    fn bridge_env(&self) -> Option<Value> {
        self.flags.remote.as_ref().map(|_| json!({ (self.flags.token_env.clone()): format!("<your token, or set {} in your shell>", self.flags.token_env) }))
    }

    fn briefing_command(&self, format: &str) -> String {
        let mut cmd = format!("{} memory briefing", quote(&self.exe()));
        match &self.flags.remote {
            Some(url) => {
                cmd.push_str(&format!(" --remote {}", quote(url)));
                if let Some(a) = self.token_env_arg() {
                    cmd.push_str(&format!(" {a}"));
                }
            }
            None => {
                if let Some(m) = &self.memory {
                    cmd.push_str(&format!(" --memory={}", quote(&m.display().to_string())));
                }
            }
        }
        if format != "text" {
            cmd.push_str(&format!(" --format {format}"));
        }
        cmd
    }

    fn memory_on(&self) -> bool {
        self.memory.is_some() || self.flags.remote.is_some()
    }

    fn token_env_arg(&self) -> Option<String> {
        (self.flags.token_env != "LEVIATHAN_TOKEN").then(|| format!("--token-env={}", self.flags.token_env))
    }
}

/// The `leviathan` server entry for one agent.
fn server_entry(a: &Agent, ctx: &Context) -> (String, Value) {
    let exe = ctx.exe();
    let args = ctx.stdio_args();
    let env = ctx.bridge_env();
    let token = &ctx.flags.token_env;
    let remote = ctx.flags.remote.as_deref();
    let with_env = |mut v: Value| {
        if let Some(e) = &env {
            v["env"] = e.clone();
        }
        v
    };
    match a.shape {
        Shape::McpServers { stdio_type, http_type, url_key, native_remote } => {
            let entry = match remote {
                Some(url) if native_remote => {
                    let header = match a.name {
                        "claude" | "warp" => format!("Bearer ${{{token}}}"),
                        _ => format!("Bearer ${{env:{token}}}"),
                    };
                    let mut v = json!({ (url_key): url, "headers": {"Authorization": header} });
                    if let Some(t) = http_type {
                        v["type"] = json!(t);
                    }
                    v
                }
                _ => {
                    let mut v = json!({"command": exe, "args": args});
                    if let Some(t) = stdio_type {
                        v["type"] = json!(t);
                    }
                    if a.name == "copilot-cli" {
                        v["tools"] = json!(["*"]);
                    }
                    with_env(v)
                }
            };
            ("mcpServers".into(), json!({"mcpServers": {"leviathan": entry}}))
        }
        Shape::VsCode => {
            let doc = match remote {
                Some(url) => json!({
                    "inputs": [{"type": "promptString", "id": "leviathan-token", "description": "Leviathan bearer token", "password": true}],
                    "servers": {"leviathan": {"type": "http", "url": url, "headers": {"Authorization": "Bearer ${input:leviathan-token}"}}}
                }),
                None => json!({"servers": {"leviathan": {"type": "stdio", "command": exe, "args": args}}}),
            };
            ("servers".into(), doc)
        }
        Shape::Opencode => {
            let entry = match remote {
                Some(url) => {
                    json!({"type": "remote", "url": url, "enabled": true, "oauth": false, "headers": {"Authorization": format!("Bearer {{env:{token}}}")}})
                }
                None => {
                    let mut cmd = vec![exe];
                    cmd.extend(args);
                    json!({"type": "local", "command": cmd, "enabled": true})
                }
            };
            let mut doc = json!({"mcp": {"leviathan": entry}});
            if a.name == "opencode" {
                doc = json!({"$schema": "https://opencode.ai/config.json", "mcp": doc["mcp"].clone()});
            }
            ("mcp".into(), doc)
        }
        Shape::Zed => (
            "context_servers".into(),
            json!({"context_servers": {"leviathan": with_env(json!({"command": exe, "args": args}))}}),
        ),
        Shape::Amp => {
            let entry = match remote {
                Some(url) => {
                    json!({"url": url, "headers": {"Authorization": format!("Bearer ${{{token}}}")}})
                }
                None => json!({"command": exe, "args": args}),
            };
            ("amp.mcpServers".into(), json!({"amp.mcpServers": {"leviathan": entry}}))
        }
        Shape::Codex | Shape::Continue | Shape::Goose => (String::new(), Value::Null),
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn codex_toml(ctx: &Context) -> String {
    match &ctx.flags.remote {
        Some(url) => format!(
            "[mcp_servers.leviathan]\nurl = {}\nbearer_token_env_var = {}\n",
            toml_str(url),
            toml_str(&ctx.flags.token_env)
        ),
        None => {
            let args: Vec<String> = ctx.stdio_args().iter().map(|a| toml_str(a)).collect();
            format!(
                "[mcp_servers.leviathan]\ncommand = {}\nargs = [{}]\n",
                toml_str(&ctx.exe()),
                args.join(", ")
            )
        }
    }
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn continue_yaml(ctx: &Context) -> String {
    let mut out =
        "name: Leviathan\nversion: 0.0.1\nschema: v1\nmcpServers:\n  - name: leviathan\n".to_string();
    match &ctx.flags.remote {
        Some(url) => out.push_str(&format!(
            "    type: streamable-http\n    url: {}\n    requestOptions:\n      headers:\n        Authorization: \"Bearer ${{{{ secrets.{} }}}}\"\n",
            yaml_str(url),
            ctx.flags.token_env
        )),
        None => {
            let args: Vec<String> = ctx.stdio_args().iter().map(|a| yaml_str(a)).collect();
            out.push_str(&format!("    type: stdio\n    command: {}\n    args: [{}]\n", yaml_str(&ctx.exe()), args.join(", ")));
        }
    }
    out
}

fn goose_yaml(ctx: &Context) -> String {
    match &ctx.flags.remote {
        Some(url) => format!(
            "extensions:\n  leviathan:\n    type: streamable_http\n    name: leviathan\n    enabled: true\n    uri: {}\n    headers:\n      Authorization: \"Bearer ${{{t}}}\"\n    env_keys: [{t}]\n    timeout: 300\n",
            yaml_str(url),
            t = ctx.flags.token_env
        ),
        None => {
            let args: Vec<String> = ctx.stdio_args().iter().map(|a| yaml_str(a)).collect();
            format!(
                "extensions:\n  leviathan:\n    type: stdio\n    name: leviathan\n    enabled: true\n    cmd: {}\n    args: [{}]\n    timeout: 300\n",
                yaml_str(&ctx.exe()),
                args.join(", ")
            )
        }
    }
}

/// The hook document (JSON) or script for one agent.
fn hook_doc(hook: Hook, ctx: &Context) -> Value {
    match hook {
        Hook::ClaudeStyle { matcher, format } => json!({"hooks": {"SessionStart": [
            {"matcher": matcher, "hooks": [{"type": "command", "command": ctx.briefing_command(format), "timeout": 15}]}
        ]}}),
        Hook::Cursor => {
            json!({"version": 1, "hooks": {"sessionStart": [{"command": ctx.briefing_command("cursor"), "timeout": 15}]}})
        }
        Hook::VsCode => {
            json!({"hooks": {"SessionStart": [{"type": "command", "command": ctx.briefing_command("claude"), "timeout": 15}]}})
        }
        Hook::CopilotCli => {
            let cmd = ctx.briefing_command("copilot");
            json!({"version": 1, "hooks": {"sessionStart": [{"type": "command", "bash": cmd, "powershell": cmd, "timeoutSec": 15}]}})
        }
        Hook::Kiro => json!({"version": "v1", "hooks": [
            {"name": "leviathan-memory", "trigger": "SessionStart", "action": {"type": "command", "command": ctx.briefing_command("text")}}
        ]}),
        Hook::Cline => Value::String(format!("#!/bin/sh\nexec {}\n", ctx.briefing_command("cline"))),
    }
}

pub const RULE_START: &str = "<!-- leviathan:memory -->";
pub const RULE_END: &str = "<!-- /leviathan:memory -->";

/// What agents are told about memory. Short: it is read every session.
pub fn rules_body(shell: bool) -> String {
    let (recall, remember, forget) = if shell {
        (
            "`leviathan memory recall [words] [-s subject]`",
            "`leviathan memory remember -s <subject> -k <key> \"<claim>\"`",
            "`leviathan memory forget`",
        )
    } else {
        ("`recall`", "`remember`", "`forget`")
    };
    let start = if shell { "`leviathan memory briefing`" } else { "`recall` with no arguments" };
    format!(
        "## Memory (Leviathan)\n\n\
         - At the start of a session, run {start} to load what is already known.\n\
         - Before acting on a person, project, service or past decision, {recall} with words or a subject.\n\
         - When you learn something durable, {remember} it: one claim each, as a fact, preference, decision \
         (with the reason), lesson, event or task.\n\
         - Give anything that can change a subject and a key (editor, deploy.target, owner) so the next write replaces it. \
         When the user corrects you, remember the correction under the same key, or {forget} the old memory.\n\
         - Never store secrets, credentials or tokens, and nothing transient (file contents, command output, today's plan).\n"
    )
}

fn rule_text(rule: RuleFile, shell: bool) -> (String, String) {
    match rule {
        RuleFile::Block(path) => {
            (path.to_string(), format!("{RULE_START}\n{}{RULE_END}\n", rules_body(shell)))
        }
        RuleFile::Own(path, front) => (path.to_string(), format!("{front}{}", rules_body(shell))),
    }
}

fn header(title: &str) -> String {
    format!("# Leviathan for {title}\n")
}

pub fn run(target: &str, ctx: &Context) -> Result<String> {
    if (ctx.flags.hooks || ctx.flags.rules) && !ctx.memory_on() {
        bail!("--hooks and --rules set up memory; add --memory (or --remote URL)");
    }
    if let Some(a) = agent(target) {
        return if ctx.flags.apply { apply(a, ctx) } else { Ok(print_agent(a, ctx)) };
    }
    if ctx.flags.apply {
        bail!("--apply works for desktop agents; {target} is configured in its own app or code");
    }
    other(target, ctx)
}

fn tool_schema_cost(ctx: &Context) -> usize {
    let memory = ctx.memory_on().then_some((MemoryTools::All, "default"));
    tool_definitions(None, memory, ctx.memory.is_none() || ctx.index.exists()).to_string().len() / 4
}

fn print_agent(a: &Agent, ctx: &Context) -> String {
    let mut out = header(a.title);
    out.push_str(&format!(
        "# MCP adds ~{} tokens of tool schema to every session. Agents with a shell can skip it: \
         `leviathan search ...`{}.\n\n",
        tool_schema_cost(ctx),
        if ctx.memory_on() { " and `leviathan memory recall ...`" } else { "" }
    ));
    let where_ = match (a.user, a.project) {
        (Some(u), Some(p)) => format!("{} (you) or {} (this project)", display_path(u), p),
        (Some(u), None) => display_path(u),
        (None, Some(p)) => format!("{p} (this project)"),
        (None, None) => "the app's MCP settings".into(),
    };
    out.push_str(&format!("## MCP server: add to {where_}\n"));
    let entry = match a.shape {
        Shape::Codex => codex_toml(ctx),
        Shape::Continue => continue_yaml(ctx),
        Shape::Goose => goose_yaml(ctx),
        _ => format!("{}\n", pretty(&server_entry(a, ctx).1)),
    };
    out.push_str(&entry);
    if a.name == "claude" {
        out.push_str(&format!("\n{}\n", a.note));
        // Remote too goes through the stdio bridge here: it reads the token at
        // run time, where `--header` would store it in Claude's state file.
        let args: Vec<String> = ctx.stdio_args().iter().map(|s| quote(s)).collect();
        out.push_str(&format!(
            "claude mcp add --scope user leviathan -- {} {}\n",
            quote(&ctx.exe()),
            args.join(" ")
        ));
    } else if !a.note.is_empty() {
        out.push_str(&format!("# {}\n", a.note));
    }
    if ctx.flags.remote.is_some() {
        let t = &ctx.flags.token_env;
        out.push_str(&if entry.contains("<your token") {
            format!(
                "# Token: the bridge reads ${t}. Delete the env entry to inherit it from the environment the agent starts \
                 from, or replace the placeholder (then keep this file out of git).\n"
            )
        } else if entry.contains("${input:") {
            "# Token: VS Code asks once and keeps it in its secret storage.\n".to_string()
        } else {
            format!("# Token: read from ${t} when the agent starts; the config holds only the variable name.\n")
        });
    }
    if ctx.flags.hooks {
        if ctx.flags.remote.is_some() {
            out.push_str(&format!(
                "\n# The briefing hook reads ${} from the agent's environment.",
                ctx.flags.token_env
            ));
        }
        match a.hook {
            Some((hook, user, project)) => {
                out.push_str(&format!("\n## Session-start briefing: {user} (you) or {project} (this project)\n"));
                match hook_doc(hook, ctx) {
                    Value::String(script) => {
                        out.push_str(&script);
                        out.push_str("# make it executable: chmod +x\n");
                    }
                    doc => {
                        out.push_str(&pretty(&doc));
                        out.push('\n');
                    }
                }
            }
            None => out.push_str(&format!("\n## Session-start briefing\n# {} has no hook that can add context; the rule below tells it to call `recall` first.\n", a.title)),
        }
    }
    if ctx.flags.rules {
        match a.rule {
            Some(rule) => {
                let (path, text) = rule_text(rule, false);
                out.push_str(&format!("\n## Always-on rule: {path}\n{text}"));
            }
            None => out.push_str(&format!(
                "\n## Rule\n# {} has no rules file; paste this into its system prompt:\n{}",
                a.title,
                rules_body(false)
            )),
        }
    }
    if a.user.is_some() || a.project.is_some() || a.hook.is_some() {
        let mut flags = String::new();
        if ctx.memory.is_some() {
            flags.push_str(" --memory");
        }
        if let Some(r) = &ctx.flags.remote {
            flags.push_str(&format!(" --remote {r}"));
        }
        out.push_str(&format!(
            "\n# Write it for you: leviathan wrap {}{flags} --hooks --rules --apply [--project]\n",
            a.name
        ));
    }
    out
}

/// A single file change for `--apply`.
struct Change {
    path: PathBuf,
    before: Option<String>,
    after: String,
    executable: bool,
}

fn read_json(path: &Path) -> Result<(Option<String>, Value)> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok((Some(text), json!({}))),
        Ok(text) => {
            let v: Value = serde_json::from_str(&text).with_context(|| {
                format!("{} is not plain JSON (comments?); add the entry by hand from `leviathan wrap` without --apply", path.display())
            })?;
            if !v.is_object() {
                bail!("{} does not hold a JSON object", path.display());
            }
            Ok((Some(text), v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((None, json!({}))),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut().expect("object")
}

/// Merge `patch` into `base`: objects merge key by key, the leviathan
/// entry itself is replaced, other servers are untouched.
fn merge_server(base: &mut Value, patch: &Value) {
    if let (Some(b), Some(p)) = (base.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            if k == "leviathan" || k == "inputs" && v.is_array() {
                if k == "inputs" {
                    let list = b.entry(k.clone()).or_insert_with(|| json!([]));
                    if let (Some(list), Some(items)) = (list.as_array_mut(), v.as_array()) {
                        for item in items {
                            if !list.iter().any(|x| x.get("id") == item.get("id")) {
                                list.push(item.clone());
                            }
                        }
                    }
                } else {
                    b.insert(k.clone(), v.clone());
                }
            } else if v.is_object() && b.get(k).is_some_and(Value::is_object) {
                merge_server(b.get_mut(k).expect("present"), v);
            } else {
                b.insert(k.clone(), v.clone());
            }
        }
    }
}

fn is_ours(v: &Value) -> bool {
    v.to_string().contains("memory briefing")
}

/// Add our hook entry to every event list in `patch`, replacing an earlier
/// Leviathan entry rather than duplicating it.
fn merge_hooks(base: &mut Value, patch: &Value) {
    let Some(p) = patch.as_object() else { return };
    let b = object(base);
    for (k, v) in p {
        match (k.as_str(), v) {
            ("hooks", Value::Object(events)) => {
                let hooks = object(b.entry("hooks").or_insert_with(|| json!({})));
                for (event, entries) in events {
                    let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
                    if !list.is_array() {
                        *list = json!([]);
                    }
                    let list = list.as_array_mut().expect("array");
                    list.retain(|e| !is_ours(e));
                    list.extend(entries.as_array().cloned().unwrap_or_default());
                }
            }
            ("hooks", Value::Array(entries)) => {
                let list = b.entry("hooks").or_insert_with(|| json!([]));
                if !list.is_array() {
                    *list = json!([]);
                }
                let list = list.as_array_mut().expect("array");
                list.retain(|e| !is_ours(e));
                list.extend(entries.iter().cloned());
            }
            _ => {
                b.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
}

fn json_change(path: PathBuf, f: impl FnOnce(&mut Value)) -> Result<Change> {
    let (before, mut v) = read_json(&path)?;
    f(&mut v);
    Ok(Change { path, before, after: format!("{}\n", pretty(&v)), executable: false })
}

fn text_change(path: PathBuf, after: impl FnOnce(Option<&str>) -> Result<String>) -> Result<Change> {
    let before = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let after = after(before.as_deref())?;
    Ok(Change { path, before, after, executable: false })
}

/// Replace the marked block, or append it.
pub fn upsert_block(existing: Option<&str>, block: &str) -> String {
    let Some(text) = existing else { return block.to_string() };
    if let (Some(s), Some(e)) = (text.find(RULE_START), text.find(RULE_END))
        && s < e
    {
        let end = e + RULE_END.len();
        let rest = text[end..].strip_prefix('\n').unwrap_or(&text[end..]);
        return format!("{}{block}{rest}", &text[..s]);
    }
    let sep = if text.is_empty() || text.ends_with("\n\n") {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{text}{sep}{block}")
}

/// Replace the `[mcp_servers.leviathan]` table, or append it, keeping the
/// rest of the file (comments included) as it is.
pub fn upsert_toml_table(existing: Option<&str>, table: &str) -> String {
    let Some(text) = existing else { return table.to_string() };
    let header = "[mcp_servers.leviathan]";
    let lines: Vec<&str> = text.lines().collect();
    if let Some(start) = lines.iter().position(|l| l.trim() == header) {
        let end = lines[start + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .map_or(lines.len(), |i| start + 1 + i);
        let mut out: Vec<String> = lines[..start].iter().map(|s| s.to_string()).collect();
        out.extend(table.trim_end().lines().map(str::to_string));
        if end < lines.len() {
            out.push(String::new());
        }
        out.extend(lines[end..].iter().map(|s| s.to_string()));
        return format!("{}\n", out.join("\n").trim_end());
    }
    let sep = if text.ends_with("\n\n") || text.is_empty() {
        ""
    } else if text.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{text}{sep}{table}")
}

fn apply(a: &Agent, ctx: &Context) -> Result<String> {
    let scope = if ctx.flags.project { a.project } else { a.user.or(a.project) };
    let mut changes = Vec::new();
    match (a.shape, scope) {
        (_, None) => bail!(
            "{} is configured in its own settings, not a file; run `leviathan wrap {}` and paste the entry",
            a.title,
            a.name
        ),
        (Shape::Goose, _) => {
            bail!("Goose keeps extensions in config.yaml; run `leviathan wrap goose` and paste the entry")
        }
        (Shape::Codex, Some(p)) => {
            let table = codex_toml(ctx);
            changes.push(text_change(expand(p), |old| Ok(upsert_toml_table(old, &table)))?);
        }
        (Shape::Continue, Some(p)) => {
            let doc = continue_yaml(ctx);
            changes.push(text_change(expand(p), |_| Ok(doc))?);
        }
        (_, Some(p)) => {
            let patch = server_entry(a, ctx).1;
            changes.push(json_change(expand(p), |v| merge_server(v, &patch))?);
        }
    }
    if ctx.flags.hooks
        && let Some((hook, user, project)) = a.hook
    {
        let path = expand(if ctx.flags.project { project } else { user });
        match hook_doc(hook, ctx) {
            Value::String(script) => {
                let mut c = text_change(path.clone(), |old| match old {
                    Some(o) if !o.trim().is_empty() && !o.contains("memory briefing") => bail!(
                        "{} already exists and is not Leviathan's; add the briefing line to it by hand",
                        path.display()
                    ),
                    _ => Ok(script),
                })?;
                c.executable = true;
                changes.push(c);
            }
            doc => {
                // Gemini keeps hooks in the same settings file as servers.
                if let Some(existing) = changes.iter_mut().find(|c| c.path == path) {
                    let mut v: Value = serde_json::from_str(&existing.after)?;
                    merge_hooks(&mut v, &doc);
                    existing.after = format!("{}\n", pretty(&v));
                } else {
                    changes.push(json_change(path, |v| merge_hooks(v, &doc))?);
                }
            }
        }
    }
    if ctx.flags.rules
        && let Some(rule) = a.rule
    {
        // Rules live with the project the agent works in, whatever the scope.
        let (path, text) = rule_text(rule, false);
        let path = PathBuf::from(path);
        changes.push(match rule {
            RuleFile::Block(_) => text_change(path, |old| Ok(upsert_block(old, &text)))?,
            RuleFile::Own(..) => text_change(path, |_| Ok(text))?,
        });
    }

    for c in &changes {
        refuse_project_symlinks(&c.path)?;
        refuse_project_symlinks(Path::new(&format!("{}.bak", c.path.display())))?;
    }
    let mut out = header(a.title);
    for c in &changes {
        if c.before.as_deref() == Some(c.after.as_str()) {
            out.push_str(&format!("unchanged {}\n", c.path.display()));
            continue;
        }
        if let Some(dir) = c.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        if let Some(before) = &c.before {
            let bak = PathBuf::from(format!("{}.bak", c.path.display()));
            std::fs::write(&bak, before).with_context(|| format!("write {}", bak.display()))?;
        }
        std::fs::write(&c.path, &c.after).with_context(|| format!("write {}", c.path.display()))?;
        #[cfg(unix)]
        if c.executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&c.path, std::fs::Permissions::from_mode(0o755))?;
        }
        out.push_str(&format!(
            "{} {}\n",
            if c.before.is_some() { "updated" } else { "created" },
            c.path.display()
        ));
        out.push_str(&diff(c.before.as_deref().unwrap_or(""), &c.after));
    }
    if a.name == "claude" && !ctx.flags.project {
        out.push_str(
            "note: Claude Code keeps user servers in its own state file; for every project run:\n  ",
        );
        out.push_str(print_agent(a, ctx).lines().find(|l| l.starts_with("claude mcp add")).unwrap_or(""));
        out.push('\n');
    }
    if ctx.flags.remote.is_some() {
        out.push_str(&format!("set {} in the environment the agent starts from\n", ctx.flags.token_env));
    }
    Ok(out)
}

/// Lines removed and added, in order; enough to review a config change.
fn diff(before: &str, after: &str) -> String {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut out = String::new();
    for l in &old[prefix..old.len() - suffix] {
        out.push_str(&format!("  - {l}\n"));
    }
    for l in &new[prefix..new.len() - suffix] {
        out.push_str(&format!("  + {l}\n"));
    }
    out
}

fn remote_url(ctx: &Context, target: &str) -> Result<String> {
    ctx.flags.remote.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "{target} connects to a server over HTTPS: run `leviathan serve --http ... --memory` on a reachable host, \
             then pass its MCP URL with --remote https://<host>/mcp"
        )
    })
}

fn base_url(mcp_url: &str) -> String {
    mcp_url.trim_end_matches('/').trim_end_matches("/mcp").to_string()
}

/// Tool schemas for function calling, flattened for each provider.
fn function_tools(ctx: &Context) -> Vec<Value> {
    let data = ctx.memory.is_none() || ctx.index.exists();
    let memory = ctx.memory_on().then_some((MemoryTools::All, "default"));
    let mut tools = tool_definitions(None, memory, data).as_array().cloned().unwrap_or_default();
    for t in &mut tools {
        if let Some(props) = t.pointer_mut("/inputSchema/properties").and_then(Value::as_object_mut) {
            props.remove("where");
            props.remove("format");
        }
    }
    tools
}

fn other(target: &str, ctx: &Context) -> Result<String> {
    let title = OTHER.iter().find(|(n, _)| *n == target).map(|(_, t)| *t).unwrap_or(target);
    let mut out = header(title);
    let token = &ctx.flags.token_env;
    match target {
        "claude-ai" | "chatgpt" => {
            let url = remote_url(ctx, target)?;
            out.push_str(&format!(
                "# 1. On a host with a public HTTPS name (a VPS, or a tunnel: Cloudflare Tunnel, Tailscale Funnel):\n\
                 leviathan serve --http 127.0.0.1:7777 --memory --auth oauth --public-url {}\n\
                 # 2. Add the connector:\n",
                base_url(&url)
            ));
            if target == "claude-ai" {
                out.push_str(&format!(
                    "#    claude.ai > Settings > Connectors > Add custom connector\n#    URL: {url}\n\
                     #    (Team and Enterprise: an owner adds it under Organization settings > Connectors)\n"
                ));
            } else {
                out.push_str(&format!(
                    "#    ChatGPT > Settings > Apps > Advanced settings > Developer mode: on\n\
                     #    then Create app > MCP server URL: {url} > Authentication: OAuth\n"
                ));
            }
            out.push_str(
                "# 3. Sign in: the approval page asks for the one-time code that `serve` prints in its log.\n\
                 # Both apps register themselves (OAuth dynamic client registration) and refresh their own tokens.\n",
            );
        }
        "anthropic-api" => {
            let url = remote_url(ctx, target)?;
            out.push_str("# POST https://api.anthropic.com/v1/messages with header: anthropic-beta: mcp-client-2025-11-20\n");
            out.push_str(&pretty(&json!({
                "model": "<model>",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "<prompt>"}],
                "mcp_servers": [{"type": "url", "url": url, "name": "leviathan", "authorization_token": format!("<${token}>")}],
                "tools": [{"type": "mcp_toolset", "mcp_server_name": "leviathan"}]
            })));
            out.push('\n');
        }
        "openai-api" => {
            let url = remote_url(ctx, target)?;
            out.push_str(
                "# Responses API: add to `tools`. The token is sent with every request and not stored.\n",
            );
            out.push_str(&pretty(&json!({
                "type": "mcp",
                "server_label": "leviathan",
                "server_description": "Search indexed data and read/write long-term memory",
                "server_url": url,
                "authorization": format!("<${token}>"),
                "require_approval": "never"
            })));
            out.push('\n');
        }
        "openai-agents" => {
            let url = remote_url(ctx, target)?;
            out.push_str(&format!(
                "import os\nfrom agents import Agent\nfrom agents.mcp import MCPServerStreamableHttp\n\n\
                 leviathan = MCPServerStreamableHttp(\n    name=\"leviathan\",\n    params={{\"url\": \"{url}\", \"headers\": {{\"Authorization\": f\"Bearer {{os.environ['{token}']}}\"}}, \"timeout\": 10}},\n    cache_tools_list=True,\n)\n\
                 # async with leviathan: agent = Agent(name=\"assistant\", mcp_servers=[leviathan], ...)\n\
                 # Local instead: MCPServerStdio(params={{\"command\": \"{}\", \"args\": {}}})\n",
                ctx.exe(),
                serde_json::to_string(&ctx.stdio_args())?
            ));
        }
        "langchain" => {
            let conn = match &ctx.flags.remote {
                Some(url) => format!(
                    "{{\"transport\": \"streamable_http\", \"url\": \"{url}\", \"headers\": {{\"Authorization\": f\"Bearer {{os.environ['{token}']}}\"}}}}"
                ),
                None => format!(
                    "{{\"transport\": \"stdio\", \"command\": \"{}\", \"args\": {}}}",
                    ctx.exe(),
                    serde_json::to_string(&ctx.stdio_args())?
                ),
            };
            out.push_str(&format!(
                "# pip install langchain-mcp-adapters\nimport os\nfrom langchain_mcp_adapters.client import MultiServerMCPClient\n\n\
                 client = MultiServerMCPClient({{\"leviathan\": {conn}}})\ntools = await client.get_tools()  # pass to create_react_agent / your graph\n\
                 # LlamaIndex (llama-index-tools-mcp), CrewAI (crewai-tools MCPServerAdapter) and AutoGen (autogen-ext mcp) take the same URL or command.\n"
            ));
        }
        "vercel-ai" => {
            let transport = match &ctx.flags.remote {
                Some(url) => format!(
                    "new StreamableHTTPClientTransport(new URL(\"{url}\"), {{ requestInit: {{ headers: {{ Authorization: `Bearer ${{process.env.{token}}}` }} }} }})"
                ),
                None => format!(
                    "new StdioClientTransport({{ command: \"{}\", args: {} }})",
                    ctx.exe(),
                    serde_json::to_string(&ctx.stdio_args())?
                ),
            };
            out.push_str(&format!(
                "// npm i ai @modelcontextprotocol/sdk\nimport {{ experimental_createMCPClient as createMCPClient }} from \"ai\";\n\
                 import {{ StreamableHTTPClientTransport }} from \"@modelcontextprotocol/sdk/client/streamableHttp.js\";\n\
                 import {{ StdioClientTransport }} from \"@modelcontextprotocol/sdk/client/stdio.js\";\n\n\
                 const leviathan = await createMCPClient({{ transport: {transport} }});\nconst tools = await leviathan.tools(); // generateText({{ tools, ... }})\n"
            ));
        }
        "rest" => {
            let base =
                ctx.flags.remote.as_deref().map(base_url).unwrap_or_else(|| "http://127.0.0.1:7777".into());
            if ctx.flags.remote.is_none() {
                out.push_str("# Start the server (the token is created in leviathan.token beside the memory file):\nleviathan serve --http 127.0.0.1:7777 --memory\n");
            }
            out.push_str(&format!(
                "# Every MCP tool is also POST {base}/v1/<tool> with the same JSON arguments; responses are JSON.\n\
                 # OpenAPI 3.1 (for Custom GPT Actions, code generators and agent frameworks): {base}/openapi.json\n\
                 curl -s {base}/v1/recall -H \"Authorization: Bearer ${token}\" -H 'content-type: application/json' -d '{{\"query\": \"deploy\"}}'\n\
                 curl -s {base}/v1/remember -H \"Authorization: Bearer ${token}\" -H 'content-type: application/json' \\\n  \
                 -d '{{\"text\": \"Deploys go through staging first\", \"kind\": \"decision\", \"subject\": \"deploys\", \"key\": \"path\"}}'\n"
            ));
        }
        "openai-tools" => {
            let tools: Vec<Value> = function_tools(ctx)
                .into_iter()
                .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}}))
                .collect();
            out.push_str("# Chat Completions `tools`. Execute each call with POST /v1/<name> (`leviathan wrap rest`) or the CLI.\n");
            out.push_str(&pretty(&Value::Array(tools)));
            out.push('\n');
        }
        "anthropic-tools" => {
            let tools: Vec<Value> = function_tools(ctx)
                .into_iter()
                .map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["inputSchema"]}))
                .collect();
            out.push_str("# Messages API `tools`. Execute each tool_use with POST /v1/<name> (`leviathan wrap rest`) or the CLI.\n");
            out.push_str(&pretty(&Value::Array(tools)));
            out.push('\n');
        }
        "gemini-tools" => {
            let decls: Vec<Value> = function_tools(ctx)
                .into_iter()
                .map(|t| json!({"name": t["name"], "description": t["description"], "parameters": t["inputSchema"]}))
                .collect();
            out.push_str("# Gemini API `tools`. Execute each functionCall with POST /v1/<name> (`leviathan wrap rest`) or the CLI.\n");
            out.push_str(&pretty(&json!([{"functionDeclarations": decls}])));
            out.push('\n');
        }
        "shell" => {
            out.push_str(&format!(
                "# No MCP needed: the agent runs the CLI. Put this in its instructions file\n\
                 # (Aider: CONVENTIONS.md with --read; OpenHands: .openhands/microagents/repo.md; others: AGENTS.md).\n\
                 # Commands use {}; set LEVIATHAN_MEMORY to point every call at one memory file.\n\n{}",
                ctx.exe(),
                rules_body(true)
            ));
        }
        other => bail!("unknown target {other:?}; choose one of: {}", targets().join(", ")),
    }
    Ok(out)
}
