//! `leviathan wrap <agent>`: print the configuration snippet that registers
//! Leviathan with an agent. It prints; it never edits anyone's config.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;

pub const AGENTS: &[&str] = &["claude", "codex", "cursor", "vscode", "gemini", "windsurf", "generic"];

pub fn recipe(agent: &str, exe: &Path, index: &Path) -> Result<String> {
    let exe = exe.display().to_string();
    let index = index.display().to_string();
    let args = json!(["mcp", "--index", index]);
    let stanza = |key: &str, extra: Option<(&str, &str)>| {
        let mut server = json!({"command": exe, "args": args});
        if let Some((k, v)) = extra {
            server[k] = json!(v);
        }
        serde_json::to_string_pretty(&json!({ key: {"leviathan": server} })).unwrap()
    };
    let cli_first = format!(
        "# CLI first: an agent with a shell needs no registration at all.\n\
         #   {exe} --index {index} describe\n\
         #   {exe} --index {index} search \"<words>\" [-g <group>] [--where field=value]\n\
         # Install skills/leviathan/SKILL.md so the agent knows when to call it.\n\
         # MCP (optional) costs ~{} tokens of tool schema in every session.\n\n",
        crate::mcp::tool_definitions(None).to_string().len() / 4
    );
    let body = match agent {
        "claude" => format!("claude mcp add leviathan -- {exe} mcp --index {index}\n"),
        "codex" => format!(
            "# ~/.codex/config.toml\n[mcp_servers.leviathan]\ncommand = {exe:?}\nargs = [\"mcp\", \"--index\", {index:?}]\n"
        ),
        "cursor" => format!(
            "// .cursor/mcp.json (project) or ~/.cursor/mcp.json (global)\n{}\n",
            stanza("mcpServers", None)
        ),
        "vscode" => format!("// .vscode/mcp.json\n{}\n", stanza("servers", Some(("type", "stdio")))),
        "gemini" => format!("// ~/.gemini/settings.json\n{}\n", stanza("mcpServers", None)),
        "windsurf" => format!("// ~/.codeium/windsurf/mcp_config.json\n{}\n", stanza("mcpServers", None)),
        "generic" => format!("// any MCP client that speaks stdio\n{}\n", stanza("mcpServers", None)),
        other => bail!("unknown agent {other:?}; choose one of: {}", AGENTS.join(", ")),
    };
    Ok(cli_first + &body)
}
