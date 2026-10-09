//! `leviathan wrap`: every target prints a usable setup, JSON configs parse,
//! tokens never land in output, `--apply` merges and is idempotent, and
//! every briefing hook format is what its agent reads.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const SECRET: &str = "lvt_this-must-never-appear-in-any-output-0123456789";

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LEVIATHAN_HOME", home.join(".leviathan"))
        .env("LEVIATHAN_TOKEN", SECRET)
        .env_remove("LEVIATHAN_MEMORY")
        .env_remove("LEVIATHAN_CONFIG")
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn targets(home: &Path) -> Vec<String> {
    let out = run(home, home, &["wrap"]);
    assert!(out.status.success());
    stdout(&out)
        .lines()
        .take_while(|l| !l.is_empty())
        .map(|l| l.split_whitespace().next().unwrap().to_string())
        .collect()
}

/// The first top-level JSON object after `## MCP server`.
fn server_json(text: &str) -> Option<Value> {
    let after = text.split("## MCP server").nth(1)?;
    let start = after.find("\n{")? + 1;
    let end = after[start..].find("\n}\n")? + start + 2;
    Some(
        serde_json::from_str(&after[start..end])
            .unwrap_or_else(|e| panic!("bad JSON ({e}):\n{}", &after[start..end])),
    )
}

#[test]
fn every_target_prints_a_setup_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let all = targets(home);
    assert!(all.len() >= 30, "{all:?}");
    for must in [
        "claude",
        "codex",
        "cursor",
        "vscode",
        "gemini",
        "windsurf",
        "cline",
        "zed",
        "claude-ai",
        "chatgpt",
        "openai-tools",
    ] {
        assert!(all.iter().any(|t| t == must), "missing {must}");
    }
    let hosted = ["claude-ai", "chatgpt", "anthropic-api", "openai-api", "openai-agents"];
    for t in &all {
        for flags in [
            &[][..],
            &["--memory"],
            &["--memory", "--hooks", "--rules"],
            &["--remote", "https://mem.example.com/mcp", "--hooks", "--rules"],
        ] {
            let mut args = vec!["wrap", t.as_str()];
            args.extend_from_slice(flags);
            let out = run(home, home, &args);
            let text = stdout(&out);
            let label = format!("wrap {t} {}", flags.join(" "));
            if hosted.contains(&t.as_str()) && !flags.contains(&"--remote") {
                assert!(!out.status.success(), "{label} should need --remote");
                continue;
            }
            assert!(out.status.success(), "{label}: {}", String::from_utf8_lossy(&out.stderr));
            assert!(!text.is_empty(), "{label}");
            assert!(!text.contains(SECRET), "{label} leaked the token");
            if text.contains("## MCP server") && text.contains("\n{") && !text.contains("[mcp_servers") {
                server_json(&text);
            }
            if flags.contains(&"--memory") {
                assert!(text.contains("--memory=") || text.contains("memory"), "{label}");
            }
        }
    }
}

#[test]
fn function_schema_exports_are_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    for t in ["openai-tools", "anthropic-tools", "gemini-tools"] {
        let out = run(dir.path(), dir.path(), &["--memory", "wrap", t]);
        assert!(out.status.success());
        let text = stdout(&out);
        let start = text.find(['[', '{']).unwrap();
        let v: Value =
            serde_json::from_str(text[start..].trim()).unwrap_or_else(|e| panic!("{t}: {e}\n{text}"));
        let s = v.to_string();
        for tool in ["remember", "recall", "forget"] {
            assert!(s.contains(tool), "{t} lacks {tool}");
        }
    }
}

#[test]
fn apply_merges_backs_up_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(home.join(".cursor/mcp.json"), r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
    std::fs::write(proj.join("AGENTS.md"), "# Project\n\nKeep this.\n").unwrap();

    let args = ["--memory", "wrap", "cursor", "--hooks", "--rules", "--apply"];
    let first = run(&home, &proj, &args);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap()).unwrap();
    assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
    assert!(mcp["mcpServers"]["leviathan"]["args"].to_string().contains("--memory="));
    assert!(home.join(".cursor/mcp.json.bak").exists());
    let hooks: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".cursor/hooks.json")).unwrap()).unwrap();
    assert!(hooks["hooks"]["sessionStart"][0]["command"].as_str().unwrap().ends_with("--format cursor"));
    assert!(proj.join(".cursor/rules/leviathan-memory.mdc").exists());

    let again = run(&home, &proj, &args);
    assert!(again.status.success());
    assert!(!stdout(&again).contains("updated") && !stdout(&again).contains("created"), "{}", stdout(&again));

    // Block rules keep the rest of the file, and re-applying does not duplicate.
    for _ in 0..2 {
        assert!(run(&home, &proj, &["--memory", "wrap", "codex", "--rules", "--apply"]).status.success());
    }
    let agents = std::fs::read_to_string(proj.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Project\n\nKeep this.\n"));
    assert_eq!(agents.matches("<!-- leviathan:memory -->").count(), 1);
    let toml = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert_eq!(toml.matches("[mcp_servers.leviathan]").count(), 1);

    // Remote configs carry the variable name, never the token.
    let remote =
        ["wrap", "claude", "--remote", "https://mem.example.com/mcp", "--hooks", "--apply", "--project"];
    assert!(run(&home, &proj, &remote).status.success());
    let mcp = std::fs::read_to_string(proj.join(".mcp.json")).unwrap();
    assert!(mcp.contains("${LEVIATHAN_TOKEN}") && !mcp.contains(SECRET), "{mcp}");

    // Files that are not plain JSON are refused, not rewritten.
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(home.join(".gemini/settings.json"), "{ // comment\n}").unwrap();
    let out = run(&home, &proj, &["--memory", "wrap", "gemini", "--apply"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap(), "{ // comment\n}");
}

#[cfg(unix)]
#[test]
fn apply_refuses_project_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    let theirs = r#"{"mcpServers":{"mine":{"command":"x"}}}"#;
    std::fs::write(home.join(".cursor/mcp.json"), theirs).unwrap();
    // A repository whose project config points into the user's own config.
    std::os::unix::fs::symlink(home.join(".cursor"), proj.join(".cursor")).unwrap();

    let out = run(&home, &proj, &["--memory", "wrap", "cursor", "--rules", "--apply", "--project"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("symlink"));
    assert_eq!(std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap(), theirs);
    assert!(!home.join(".cursor/rules").exists() && !home.join(".cursor/mcp.json.bak").exists());
}

#[test]
fn briefing_hook_formats_match_each_agent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let mem = format!("--memory={}", home.join("m.db").display());
    assert!(
        run(home, home, &[&mem, "memory", "remember", "Uses pnpm, not npm", "-s", "repo", "-k", "pm"])
            .status
            .success()
    );
    let brief = |format: &str| {
        let out = run(home, home, &[&mem, "memory", "briefing", "--format", format]);
        assert!(out.status.success());
        stdout(&out)
    };
    assert!(brief("text").contains("Uses pnpm"));
    let field = |format: &str, pointer: &str| {
        let v: Value = serde_json::from_str(brief(format).trim()).unwrap();
        v.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{format}: no {pointer} in {v}"))
            .to_string()
    };
    assert!(field("claude", "/hookSpecificOutput/additionalContext").contains("Uses pnpm"));
    assert_eq!(field("claude", "/hookSpecificOutput/hookEventName"), "SessionStart");
    assert!(field("gemini", "/hookSpecificOutput/additionalContext").contains("Uses pnpm"));
    assert!(field("cursor", "/additional_context").contains("Uses pnpm"));
    assert!(field("copilot", "/additionalContext").contains("Uses pnpm"));
    assert!(field("cline", "/contextModification").contains("Uses pnpm"));

    // A broken memory never breaks the agent's session start.
    let bad = format!("--memory={}", home.display());
    let out = run(home, home, &[&bad, "memory", "briefing", "--format", "claude"]);
    assert!(out.status.success());
    assert!(stdout(&out).is_empty());
}
