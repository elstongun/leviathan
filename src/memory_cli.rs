//! `leviathan memory ...`: the CLI face of the memory store, for people,
//! scripts, session hooks and agents that have a shell but no MCP.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use leviathan::card::CardOptions;
use leviathan::config::{Config, DEFAULT_FILE, MemoryConfig};
use leviathan::memory::{
    self, Forget, Kind, Memory, PruneOptions, RecallRequest, RecallStatus, Remember, Settings,
};
use leviathan::query::Store;

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Create the memory database (it is also created on first write).
    Init,
    /// Store one memory. With --key, replaces that slot's current value.
    Remember {
        /// The claim, in a sentence.
        #[arg(required = true)]
        text: Vec<String>,
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        /// Who or what it is about.
        #[arg(short, long)]
        subject: Option<String>,
        /// The slot that can change (editor, deploy.target, owner).
        #[arg(short, long)]
        key: Option<String>,
        #[arg(short, long = "tag")]
        tags: Vec<String>,
        /// 1 (trivia) to 5 (critical).
        #[arg(short, long)]
        importance: Option<u8>,
        #[arg(long)]
        confidence: Option<f64>,
        /// Where it came from (user, observed, a document).
        #[arg(long)]
        source: Option<String>,
        /// Related record id, path or URL (repeatable).
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// Always lead briefings with it.
        #[arg(long)]
        pin: bool,
        /// A date, or a duration from now (12h, 30d, 6w).
        #[arg(long)]
        expires: Option<String>,
        /// Another name for the subject (repeatable).
        #[arg(long = "alias")]
        aliases: Vec<String>,
        #[arg(long)]
        ns: Option<String>,
    },
    /// Ranked memories within a token budget. No words: a briefing.
    Recall {
        words: Vec<String>,
        #[arg(short, long)]
        subject: Option<String>,
        /// Kinds, comma separated.
        #[arg(long, value_enum, value_delimiter = ',')]
        kind: Vec<Kind>,
        /// Namespaces, comma separated, or `*`.
        #[arg(long)]
        ns: Option<String>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// What was true at the end of this date.
        #[arg(long)]
        as_of: Option<String>,
        /// Include superseded and forgotten memories.
        #[arg(long)]
        history: bool,
        /// Token budget.
        #[arg(short, long)]
        budget: Option<usize>,
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Also show the indexed records (--index) that memories reference.
        #[arg(long)]
        with_records: bool,
    },
    /// The session-start briefing, formatted for an agent's hook. Never fails the hook.
    Briefing {
        #[arg(long)]
        ns: Option<String>,
        #[arg(short, long)]
        budget: Option<usize>,
        #[arg(long, value_enum, default_value_t = BriefingFormat::Text)]
        format: BriefingFormat,
        /// Read the briefing from a remote Leviathan's MCP URL instead of a local file.
        #[arg(long, value_name = "URL")]
        remote: Option<String>,
        /// Bearer token file for --remote [default: the --token-env variable].
        #[arg(long, env = "LEVIATHAN_TOKEN_FILE", value_name = "FILE")]
        token_file: Option<PathBuf>,
        /// Environment variable holding the bearer token for --remote.
        #[arg(long, default_value = "LEVIATHAN_TOKEN", value_name = "NAME")]
        token_env: String,
    },
    /// Mark a memory as no longer true (kept as history until `prune`).
    Forget {
        id: Option<String>,
        #[arg(short, long)]
        subject: Option<String>,
        #[arg(short, long)]
        key: Option<String>,
        #[arg(long)]
        ns: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Newest memories first.
    List {
        #[arg(long)]
        ns: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        #[arg(short, long)]
        subject: Option<String>,
        /// Include superseded, forgotten and expired.
        #[arg(long)]
        all: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// Every version of a slot, oldest first.
    History {
        id: Option<String>,
        #[arg(short, long)]
        subject: Option<String>,
        #[arg(short, long)]
        key: Option<String>,
        #[arg(long)]
        ns: Option<String>,
    },
    /// Counts by kind, namespace and subject.
    Stats,
    /// Every memory, history included, as JSON lines.
    Export {
        #[arg(short, long, default_value = "-")]
        output: PathBuf,
    },
    /// Restore an export, or turn a markdown memory file (MEMORY.md, notes) into memories.
    Import {
        /// JSONL from `export`, or markdown (.md); `-` for stdin.
        path: PathBuf,
        /// Treat the input as markdown (default for .md files).
        #[arg(long)]
        markdown: bool,
        /// Namespace for markdown imports.
        #[arg(long)]
        ns: Option<String>,
    },
    /// Drop expired memories; optionally forgotten, superseded or unused ones.
    Prune {
        #[arg(long)]
        forgotten: bool,
        #[arg(long)]
        superseded: bool,
        /// Forget unpinned memories of importance 1-2 never recalled in this many days.
        #[arg(long, value_name = "DAYS")]
        unused: Option<u32>,
        #[arg(long)]
        dry_run: bool,
    },
}

/// Output shapes for session-start hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BriefingFormat {
    /// Plain text: Claude Code, Codex and Kiro add stdout to context.
    Text,
    /// The full recall outcome.
    Json,
    /// hookSpecificOutput.additionalContext with hookEventName (Claude Code JSON, VS Code Copilot).
    Claude,
    /// Gemini CLI: JSON only, hookSpecificOutput.additionalContext.
    Gemini,
    /// Cursor sessionStart: additional_context.
    Cursor,
    /// GitHub Copilot CLI sessionStart: additionalContext.
    Copilot,
    /// Cline TaskStart: contextModification.
    Cline,
}

/// `[memory]` from an explicit config, else ./leviathan.toml when present.
pub fn memory_config(explicit: Option<&Path>) -> Result<MemoryConfig> {
    let path = explicit.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("LEVIATHAN_CONFIG")
            .map(PathBuf::from)
            .or_else(|| Path::new(DEFAULT_FILE).exists().then(|| PathBuf::from(DEFAULT_FILE)))
    });
    let cfg = match path {
        Some(p) => Config::load(&p)?.memory,
        None => MemoryConfig::default(),
    };
    Ok(cfg)
}

/// `--memory=PATH`, else `[memory].path`, else the default location.
pub fn resolve_path(flag: Option<&str>, cfg: &MemoryConfig) -> PathBuf {
    match flag.map(str::trim).filter(|f| !f.is_empty()) {
        Some(p) => memory::expand_home(p),
        None => cfg.path.as_deref().map(memory::expand_home).unwrap_or_else(memory::default_path),
    }
}

fn emit<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        print!("{}", text());
    }
    Ok(())
}

fn data_store(index: &Path, opts: CardOptions) -> Option<Store> {
    index.exists().then(|| Store::open(index, opts).ok()).flatten()
}

pub fn run(
    cmd: MemoryCommand,
    path: &Path,
    cfg: &MemoryConfig,
    json: bool,
    index: &Path,
    card: CardOptions,
) -> Result<u8> {
    let settings = Settings::from_config(cfg);
    if let MemoryCommand::Briefing { ns, budget, format, remote, token_file, token_env } = &cmd {
        // A hook must never break the agent's session start.
        let text = match remote {
            Some(url) => remote_briefing(url, (token_file.as_deref(), token_env), ns.as_deref(), *budget),
            None => briefing(path, settings, ns.as_deref(), *budget, index, card),
        };
        return match text.and_then(|t| hook_output(&t, *format)) {
            Ok(text) => {
                print!("{text}");
                Ok(0)
            }
            Err(err) => {
                eprintln!("leviathan: briefing unavailable: {err:#}");
                Ok(0)
            }
        };
    }
    let mut m = Memory::open(path, settings)?;
    let now = leviathan::now_rfc3339();
    match cmd {
        MemoryCommand::Init => {
            let s = m.stats()?;
            emit(json, &s, || {
                format!(
                    "memory ready at {} ({} current)\n  next: `leviathan wrap <agent> --memory` prints the agent setup\n",
                    s.path.display(),
                    s.current
                )
            })?;
        }
        MemoryCommand::Remember {
            text,
            kind,
            subject,
            key,
            tags,
            importance,
            confidence,
            source,
            refs,
            pin,
            expires,
            aliases,
            ns,
        } => {
            let req = Remember {
                text: text.join(" "),
                kind,
                subject,
                key,
                tags,
                importance,
                confidence,
                source,
                refs,
                pinned: pin,
                expires,
                aliases,
                ns,
            };
            let out = m.remember(&req)?;
            emit(json, &out, || out.render())?;
        }
        MemoryCommand::Recall {
            words,
            subject,
            kind,
            ns,
            since,
            until,
            as_of,
            history,
            budget,
            limit,
            with_records,
        } => {
            let briefing = words.is_empty() && subject.is_none() && kind.is_empty();
            let req = RecallRequest {
                query: words.join(" "),
                ns,
                subject,
                kinds: kind,
                since,
                until,
                as_of,
                history,
                budget,
                limit,
                with_records,
                briefing,
            };
            let store = if with_records { data_store(index, card) } else { None };
            let out = m.recall(&req, store.as_ref())?;
            emit(json, &out, || out.render())?;
            if out.status != RecallStatus::Ok {
                return Ok(3);
            }
        }
        MemoryCommand::Briefing { .. } => unreachable!("handled above"),
        MemoryCommand::Forget { id, subject, key, ns, reason } => {
            let row = m.forget(&Forget { id, subject, key, ns, reason })?;
            emit(json, &row, || format!("forgot {}\n  {}\n", row.id, row.line(&now, false)))?;
        }
        MemoryCommand::List { ns, kind, subject, all, limit } => {
            let namespaces = m.read_namespaces(ns.as_deref())?;
            let rows = m.list(&namespaces, kind, subject.as_deref(), all, limit)?;
            emit(json, &rows, || {
                let mut out = format!("{} memories, newest first ({})\n", rows.len(), namespaces.join(", "));
                for r in &rows {
                    out.push_str(&r.line(&now, namespaces.len() > 1));
                    out.push('\n');
                }
                out
            })?;
        }
        MemoryCommand::History { id, subject, key, ns } => {
            let rows = m.history(id.as_deref(), subject.as_deref(), key.as_deref(), ns.as_deref())?;
            if rows.is_empty() {
                bail!("no memories in that slot");
            }
            emit(json, &rows, || rows.iter().map(|r| format!("{}\n", r.line(&now, false))).collect())?;
        }
        MemoryCommand::Stats => {
            let s = m.stats()?;
            emit(json, &s, || leviathan::render::memory_stats(&s))?;
        }
        MemoryCommand::Export { output } => {
            let n = if output == Path::new("-") {
                let mut out = std::io::stdout().lock();
                let n = m.export(&mut out)?;
                out.flush()?;
                n
            } else {
                let mut f = std::io::BufWriter::new(
                    std::fs::File::create(&output).with_context(|| format!("create {}", output.display()))?,
                );
                let n = m.export(&mut f)?;
                f.flush()?;
                n
            };
            eprintln!("exported {n} memories");
        }
        MemoryCommand::Import { path, markdown, ns } => {
            let mut text = String::new();
            if path == Path::new("-") {
                std::io::stdin().read_to_string(&mut text)?;
            } else {
                std::fs::File::open(&path)
                    .with_context(|| format!("open {}", path.display()))?
                    .read_to_string(&mut text)?;
            }
            let is_md = markdown || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md"));
            let report = if is_md {
                let source =
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or("stdin".into());
                m.import_markdown(&text, ns.as_deref(), &source)?
            } else {
                m.import_jsonl(BufReader::new(text.as_bytes()))?
            };
            emit(json, &report, || {
                let mut out = format!(
                    "imported {}: {} created, {} replaced, {} merged, {} skipped\n",
                    report.read, report.created, report.replaced, report.merged, report.skipped
                );
                for e in &report.errors {
                    out.push_str(&format!("  {e}\n"));
                }
                out
            })?;
        }
        MemoryCommand::Prune { forgotten, superseded, unused, dry_run } => {
            let r = m.prune(PruneOptions { forgotten, superseded, unused_days: unused, dry_run })?;
            emit(json, &r, || {
                format!(
                    "{}{} expired, {} forgotten, {} superseded removed; {} unused forgotten\n",
                    if r.dry_run { "dry run: " } else { "" },
                    r.expired,
                    r.forgotten,
                    r.superseded,
                    r.unused
                )
            })?;
        }
    }
    Ok(0)
}

const EMPTY: &str = "Leviathan memory is empty. `remember` durable facts, preferences, decisions (with why) and \
    lessons as you learn them; give anything that can change a `key`. Never store secrets.\n";

/// The briefing as text (`json` format: the full outcome as JSON).
fn briefing(
    path: &Path,
    settings: Settings,
    ns: Option<&str>,
    budget: Option<usize>,
    index: &Path,
    card: CardOptions,
) -> Result<String> {
    let mut m = Memory::open(path, settings)?;
    let store = data_store(index, card);
    let out = m.briefing(ns, budget, store.as_ref())?;
    Ok(if out.results.is_empty() { EMPTY.to_string() } else { out.render() })
}

#[cfg(feature = "remote")]
fn remote_briefing(
    url: &str,
    token: (Option<&Path>, &str),
    ns: Option<&str>,
    budget: Option<usize>,
) -> Result<String> {
    let token = leviathan::http::client_token(token.0, token.1)?;
    let mut args = serde_json::json!({});
    if let Some(ns) = ns {
        args["ns"] = ns.into();
    }
    if let Some(b) = budget {
        args["budget"] = b.into();
    }
    let text = leviathan::http::call_tool(url, token.as_deref(), "recall", &args)?;
    Ok(if text.contains("0 of 0 current") { EMPTY.to_string() } else { text })
}

#[cfg(not(feature = "remote"))]
fn remote_briefing(_: &str, _: (Option<&Path>, &str), _: Option<&str>, _: Option<usize>) -> Result<String> {
    bail!("this build has no `remote` feature")
}

fn hook_output(text: &str, format: BriefingFormat) -> Result<String> {
    use serde_json::json;
    let session = |field: &str| json!({"hookSpecificOutput": {"hookEventName": "SessionStart", field: text}});
    let line = |v: serde_json::Value| format!("{v}\n");
    Ok(match format {
        BriefingFormat::Text => text.to_string(),
        BriefingFormat::Json => line(json!({"briefing": text})),
        BriefingFormat::Claude => line(session("additionalContext")),
        BriefingFormat::Gemini => line(json!({"hookSpecificOutput": {"additionalContext": text}})),
        BriefingFormat::Cursor => line(json!({"additional_context": text})),
        BriefingFormat::Copilot => line(json!({"additionalContext": text})),
        BriefingFormat::Cline => line(json!({"cancel": false, "contextModification": text})),
    })
}
