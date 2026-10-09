use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use leviathan::card::CardOptions;
use leviathan::config::{Config, FieldFlags};
use leviathan::index::{self, BuildOptions};
use leviathan::mcp::{MemoryOptions, MemoryTools, ServerOptions};
use leviathan::query::{Scope, SearchRequest, Sort, Status, Store};
use leviathan::{infer, mcp, render, source, wrap};

mod memory_cli;

/// Exit status when the named group is unknown or ambiguous.
const EXIT_GROUP: u8 = 3;
const INFER_SAMPLE: usize = 2000;

#[derive(Parser)]
#[command(
    name = "leviathan",
    version,
    about = "Deep memory for agents: index large datasets once, answer with a few ranked, cited records; remember what agents learn",
    after_help = "Examples:\n  leviathan init ./data                 # propose leviathan.toml from the data\n  leviathan index ./data                # build (uses ./leviathan.toml, else infers)\n  leviathan describe                    # fields, groups, filter values\n  leviathan search \"login loop after reset\" -g acme --where status=open\n  leviathan recent -g acme --since 2024-06\n  sqlite3 -json app.db 'select * from t' | leviathan index -\n  leviathan memory remember -s joshua -k editor \"Uses helix\"\n  leviathan memory recall editor\n  leviathan mcp --memory                # stdio MCP server, data + memory\n  leviathan serve --http 0.0.0.0:7777 --memory   # the same over HTTP\n  leviathan wrap cursor --memory        # print the setup for an agent\n\nExit status: 0 ok (zero hits included), 1 error, 2 usage or refused write, 3 group unknown or ambiguous."
)]
struct Cli {
    /// Index file.
    #[arg(long, global = true, env = "LEVIATHAN_INDEX", default_value = "leviathan.db")]
    index: PathBuf,
    /// Enable the read/write memory store. Bare `--memory` uses the default
    /// (~/.leviathan/memory.db, or [memory].path); `--memory=PATH` picks a file.
    #[arg(
        long,
        global = true,
        env = "LEVIATHAN_MEMORY",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "",
        value_name = "PATH"
    )]
    memory: Option<String>,
    /// Emit JSON instead of compact text.
    #[arg(long, global = true)]
    json: bool,
    /// Character cap per field shown on a result.
    #[arg(long, global = true, env = "LEVIATHAN_MAX_CHARS", default_value_t = CardOptions::default().max_chars)]
    max_chars: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Find {
    /// Group key or name (as configured: customer, machine, repo, ...).
    #[arg(short, long)]
    group: Option<String>,
    /// group (default with -g), others (every other group) or all.
    #[arg(long, value_enum)]
    scope: Option<Scope>,
    /// Exact filter, repeatable; the same field twice means either value.
    #[arg(short = 'w', long = "where", value_name = "FIELD=VALUE")]
    filters: Vec<String>,
    /// Earliest date: YYYY, YYYY-MM or YYYY-MM-DD.
    #[arg(long)]
    since: Option<String>,
    /// Latest date, inclusive.
    #[arg(long)]
    until: Option<String>,
    #[arg(short = 'n', long, default_value_t = 5)]
    limit: usize,
    #[arg(long, default_value_t = 0)]
    offset: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Sample the data and write a commented leviathan.toml proposing a field mapping.
    Init {
        /// Files or directories (JSONL, JSON, CSV/TSV, SQLite; optionally .gz), or `-` for stdin.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Output file, or `-` for stdout.
        #[arg(short, long, default_value = leviathan::config::DEFAULT_FILE)]
        output: PathBuf,
        /// Records to sample.
        #[arg(long, default_value_t = INFER_SAMPLE)]
        sample: usize,
        #[arg(long)]
        force: bool,
        #[arg(long, value_enum)]
        format: Option<leviathan::config::Format>,
        /// SQL query for SQLite sources.
        #[arg(long)]
        sql: Option<String>,
    },
    /// Build the index (atomic; skipped when sources and mapping are unchanged).
    Index {
        /// Files, directories or `-` for stdin [default: source.paths from the config].
        paths: Vec<PathBuf>,
        #[command(flatten)]
        fields: FieldFlags,
        /// Rebuild even when nothing changed.
        #[arg(long)]
        force: bool,
        /// Fail on the first unusable record instead of skipping it.
        #[arg(long)]
        strict: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Insert or replace records by id in an existing index, without a rebuild.
    Upsert {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        strict: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Remove records by id.
    Delete {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Ranked records for free-text words ("exact phrase", -exclude). No words lists newest first.
    Search {
        /// Words; quote the whole query to use -exclusions: "login -sso".
        words: Vec<String>,
        #[command(flatten)]
        find: Find,
        #[arg(long, value_enum, default_value_t = Sort::Relevance)]
        sort: Sort,
        /// Do not add other groups' matches when the group has none.
        #[arg(long)]
        no_fallback: bool,
    },
    /// Newest records, optionally for one group, filtered and date-bounded.
    Recent {
        #[command(flatten)]
        find: Find,
    },
    /// Resolve a group key or name to candidate groups.
    Resolve {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Print complete records by id, exactly as ingested.
    Get {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// What is in the index: fields, groups, filter values, dates, example calls.
    Describe {
        /// Values shown per filter field.
        #[arg(long, default_value_t = 8)]
        top: usize,
    },
    /// Serve the index (and memory, with --memory) over the Model Context Protocol on stdio.
    Mcp {
        /// Bridge stdio to a remote Leviathan (or any Streamable HTTP MCP server) at this URL.
        #[arg(long, value_name = "URL")]
        remote: Option<String>,
        /// Bearer token file for --remote [default: the --token-env variable].
        #[arg(long, env = "LEVIATHAN_TOKEN_FILE", value_name = "FILE")]
        token_file: Option<PathBuf>,
        /// Environment variable holding the bearer token for --remote.
        #[arg(long, default_value = "LEVIATHAN_TOKEN", value_name = "NAME")]
        token_env: String,
        #[arg(long, value_enum, default_value_t)]
        memory_tools: MemoryTools,
    },
    /// Serve MCP over Streamable HTTP, plus REST (/v1/*) and OpenAPI, for remote agents.
    Serve {
        /// Address to listen on.
        #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:7777")]
        http: String,
        /// token: a bearer token; oauth: also OAuth 2.1 for hosted apps; none: loopback only.
        #[arg(long, value_enum, default_value_t = Auth::Token)]
        auth: Auth,
        /// Bearer token file, created (mode 600) when missing [default: leviathan.token beside the memory or index].
        #[arg(long, env = "LEVIATHAN_TOKEN_FILE", value_name = "FILE")]
        token_file: Option<PathBuf>,
        /// Public base URL (https://...) clients reach this server at; required for --auth oauth beyond loopback.
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
        /// Browser origins allowed to call the server (repeatable).
        #[arg(long = "allow-origin", value_name = "ORIGIN")]
        allow_origins: Vec<String>,
        #[arg(long, default_value_t = 4)]
        threads: usize,
        #[arg(long, value_enum, default_value_t)]
        memory_tools: MemoryTools,
    },
    /// Read/write memory: remember, recall, forget, briefing, import/export.
    Memory {
        /// Config file with a [memory] section [default: ./leviathan.toml when present].
        #[arg(short = 'c', long, env = "LEVIATHAN_CONFIG")]
        config: Option<PathBuf>,
        #[command(subcommand)]
        command: memory_cli::MemoryCommand,
    },
    /// Print the setup that registers Leviathan with an agent.
    Wrap {
        /// Omit to list every target.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(wrap::targets()))]
        agent: Option<String>,
        #[command(flatten)]
        opts: wrap::WrapFlags,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Auth {
    Token,
    Oauth,
    None,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("leviathan: {err:#}");
            if err.downcast_ref::<leviathan::memory::Refused>().is_some() {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
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

fn parse_filters(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|f| match f.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
            _ => bail!("--where expects FIELD=VALUE, got {f:?}"),
        })
        .collect()
}

fn request(find: Find, query: String, sort: Sort, fallback: bool) -> Result<SearchRequest> {
    Ok(SearchRequest {
        group: find.group,
        query,
        scope: find.scope,
        filters: parse_filters(&find.filters)?,
        since: find.since,
        until: find.until,
        sort,
        limit: find.limit,
        offset: find.offset,
        fallback,
    })
}

/// Config file (explicit or ./leviathan.toml), then flags on top. Returns
/// whether a mapping was given at all.
fn load_config(flags: &FieldFlags) -> Result<(Config, bool)> {
    let path = flags.config_path();
    let mut cfg = match &path {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    flags.apply(&mut cfg);
    cfg.validate()?;
    Ok((cfg, path.is_some() || !flags.is_empty()))
}

fn sources_for(paths: &[PathBuf], cfg: &Config) -> Result<Vec<source::Source>> {
    let paths: Vec<PathBuf> =
        if paths.is_empty() { cfg.source.paths.iter().map(PathBuf::from).collect() } else { paths.to_vec() };
    if paths.is_empty() {
        bail!(
            "no sources: pass files or directories (or `-` for stdin), or set source.paths in leviathan.toml"
        );
    }
    source::discover(&paths, cfg.source.format)
}

fn search(cli_json: bool, store: &Store, req: &SearchRequest) -> Result<u8> {
    let outcome = store.search(req)?;
    emit(cli_json, &outcome, || render::search(&outcome))?;
    Ok(match outcome.status {
        Status::AmbiguousGroup | Status::UnknownGroup => EXIT_GROUP,
        Status::BadRequest => 2,
        Status::Ok => 0,
    })
}

fn run(cli: Cli) -> Result<u8> {
    let opts = CardOptions { max_chars: cli.max_chars.max(40) };
    let open = || Store::open(&cli.index, opts);
    match cli.command {
        Command::Init { paths, output, sample, force, format, sql } => {
            let to_stdout = output == Path::new("-");
            if !to_stdout && output.exists() && !force {
                bail!("{} exists; pass --force to overwrite or -o - to print", output.display());
            }
            let sources = source::discover(&paths, format.unwrap_or_default())?;
            let profile = infer::profile(&sources, sql.as_deref(), sample.max(1))?;
            let mut proposal = infer::propose(profile, &sources);
            if let Some(f) = format {
                proposal.config.source.format = f;
            }
            proposal.config.source.sql.clone_from(&sql);
            let toml = infer::to_toml(&proposal);
            if cli.json {
                println!("{}", serde_json::to_string(&proposal)?);
            } else if to_stdout {
                print!("{toml}");
            }
            if !to_stdout {
                std::fs::write(&output, &toml).with_context(|| format!("write {}", output.display()))?;
                eprintln!(
                    "wrote {} from {} sampled records; review it, then `leviathan index`",
                    output.display(),
                    proposal.profile.sampled
                );
            }
        }
        Command::Index { paths, fields, force, strict, quiet } => {
            let (mut cfg, mapped) = load_config(&fields)?;
            let sources = sources_for(&paths, &cfg)?;
            let inferred = !mapped;
            if inferred {
                if sources.iter().any(source::Source::is_stdin) {
                    bail!(
                        "stdin needs a mapping (a sample would consume it): pass field flags or --config, \
                         or save to a file and run `leviathan init` on it"
                    );
                }
                let proposal = infer::propose(
                    infer::profile(&sources, cfg.source.sql.as_deref(), INFER_SAMPLE)?,
                    &sources,
                );
                cfg.fields = proposal.config.fields;
                cfg.about = proposal.config.about;
            }
            let report =
                index::build(&cli.index, &sources, &cfg, inferred, BuildOptions { force, strict, quiet })?;
            emit(cli.json, &report, || render::ingest(&report))?;
        }
        Command::Upsert { paths, strict, quiet } => {
            let sources = source::discover(&paths, Default::default())?;
            let report = index::upsert(&cli.index, &sources, BuildOptions { force: true, strict, quiet })?;
            emit(cli.json, &report, || render::ingest(&report))?;
        }
        Command::Delete { ids } => {
            let report = index::delete(&cli.index, &ids)?;
            emit(cli.json, &report, || render::ingest(&report))?;
        }
        Command::Search { words, find, sort, no_fallback } => {
            let req = request(find, words.join(" "), sort, !no_fallback)?;
            return search(cli.json, &open()?, &req);
        }
        Command::Recent { find } => {
            let req = request(find, String::new(), Sort::Newest, false)?;
            return search(cli.json, &open()?, &req);
        }
        Command::Resolve { query, limit } => {
            let query = query.join(" ");
            let store = open()?;
            let found = store.resolve(&query, limit)?;
            let about = &store.config().about;
            let value =
                serde_json::json!({"query": query, "candidates": found, "ambiguous": found.len() > 1});
            emit(cli.json, &value, || render::resolve(&query, &found, &about.record, &about.group))?;
            if found.is_empty() {
                return Ok(EXIT_GROUP);
            }
        }
        Command::Get { ids } => {
            let store = open()?;
            let mut missing = Vec::new();
            for id in &ids {
                match store.get(id)? {
                    Some(doc) if cli.json || ids.len() > 1 => println!("{}", serde_json::to_string(&doc)?),
                    Some(doc) => println!("{}", serde_json::to_string_pretty(&doc)?),
                    None => missing.push(id.as_str()),
                }
            }
            if !missing.is_empty() {
                bail!("no record with id {}", missing.join(", "));
            }
        }
        Command::Describe { top } => {
            let d = open()?.describe(top.clamp(1, 50))?;
            emit(cli.json, &d, || render::describe(&d))?;
        }
        Command::Mcp { remote, token_file, token_env, memory_tools } => {
            if let Some(url) = remote {
                return remote_bridge(&url, token_file.as_deref(), &token_env);
            }
            mcp::serve(server_options(&cli.index, opts, cli.memory.as_deref(), memory_tools)?)?
        }
        Command::Serve { http, auth, token_file, public_url, allow_origins, threads, memory_tools } => {
            let server = server_options(&cli.index, opts, cli.memory.as_deref(), memory_tools)?;
            return serve_http(server, &http, auth, token_file, public_url, allow_origins, threads);
        }
        Command::Memory { config, command } => {
            let cfg = memory_cli::memory_config(config.as_deref())?;
            let path = memory_cli::resolve_path(cli.memory.as_deref(), &cfg);
            return memory_cli::run(command, &path, &cfg, cli.json, &cli.index, opts);
        }
        Command::Wrap { agent: None, .. } => {
            let width = wrap::catalog().iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (name, title) in wrap::catalog() {
                println!("{name:width$}  {title}");
            }
            println!("\nleviathan wrap <target> [--memory] [--remote URL] [--hooks] [--rules] [--apply]");
        }
        Command::Wrap { agent: Some(agent), opts: flags } => {
            let exe = std::env::current_exe()?;
            let index = std::path::absolute(&cli.index)?;
            let cfg = memory_cli::memory_config(None)?;
            let memory = cli.memory.as_deref().map(|m| memory_cli::resolve_path(Some(m), &cfg));
            let memory = memory.map(|m| std::path::absolute(&m)).transpose()?;
            let ctx = wrap::Context { exe, index, memory, flags };
            print!("{}", wrap::run(&agent, &ctx)?);
        }
    }
    Ok(0)
}

/// Memory is on when `--memory` (or LEVIATHAN_MEMORY) is given or the config
/// sets `[memory].path`.
fn server_options(
    index: &Path,
    card: CardOptions,
    flag: Option<&str>,
    tools: MemoryTools,
) -> Result<ServerOptions> {
    let cfg = memory_cli::memory_config(None)?;
    let memory = (flag.is_some() || cfg.path.is_some()).then(|| MemoryOptions {
        path: memory_cli::resolve_path(flag, &cfg),
        settings: leviathan::memory::Settings::from_config(&cfg),
        tools,
    });
    Ok(ServerOptions { index: index.to_path_buf(), card, memory })
}

#[cfg(feature = "remote")]
fn remote_bridge(url: &str, token_file: Option<&Path>, token_env: &str) -> Result<u8> {
    let token = leviathan::http::client_token(token_file, token_env)?;
    leviathan::http::bridge(url, token.as_deref())?;
    Ok(0)
}

#[cfg(not(feature = "remote"))]
fn remote_bridge(_: &str, _: Option<&Path>, _: &str) -> Result<u8> {
    bail!("this build has no `remote` feature; rebuild with default features for --remote")
}

#[cfg(feature = "remote")]
fn serve_http(
    server: ServerOptions,
    addr: &str,
    auth: Auth,
    token_file: Option<PathBuf>,
    public_url: Option<String>,
    allow_origins: Vec<String>,
    threads: usize,
) -> Result<u8> {
    use leviathan::http::{AuthMode, HttpOptions};
    let beside = server.memory.as_ref().map(|m| m.path.clone()).unwrap_or_else(|| server.index.clone());
    let token_file = token_file.unwrap_or_else(|| beside.with_file_name("leviathan.token"));
    let auth = match auth {
        Auth::Token => AuthMode::Token,
        Auth::Oauth => AuthMode::OAuth,
        Auth::None => AuthMode::None,
    };
    leviathan::http::serve(HttpOptions {
        server,
        addr: addr.to_string(),
        auth,
        token_file,
        public_url,
        allow_origins,
        threads: threads.clamp(1, 64),
    })?;
    Ok(0)
}

#[cfg(not(feature = "remote"))]
fn serve_http(
    _: ServerOptions,
    _: &str,
    _: Auth,
    _: Option<PathBuf>,
    _: Option<String>,
    _: Vec<String>,
    _: usize,
) -> Result<u8> {
    bail!("this build has no `remote` feature; rebuild with default features for `serve`")
}
