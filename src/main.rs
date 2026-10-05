use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use leviathan::card::CardOptions;
use leviathan::config::{Config, FieldFlags};
use leviathan::index::{self, BuildOptions};
use leviathan::query::{Scope, SearchRequest, Sort, Status, Store};
use leviathan::{infer, mcp, render, source, wrap};

/// Exit status when the named group is unknown or ambiguous.
const EXIT_GROUP: u8 = 3;
const INFER_SAMPLE: usize = 2000;

#[derive(Parser)]
#[command(
    name = "leviathan",
    version,
    about = "Deep memory for agents: index large datasets once, answer with a few ranked, cited records",
    after_help = "Examples:\n  leviathan init ./data                 # propose leviathan.toml from the data\n  leviathan index ./data                # build (uses ./leviathan.toml, else infers)\n  leviathan describe                    # fields, groups, filter values\n  leviathan search \"login loop after reset\" -g acme --where status=open\n  leviathan recent -g acme --since 2024-06\n  sqlite3 -json app.db 'select * from t' | leviathan index -\n  leviathan mcp                         # stdio MCP server\n  leviathan wrap cursor                 # print MCP config for an agent\n\nExit status: 0 ok (zero hits included), 1 error, 2 usage, 3 group unknown or ambiguous."
)]
struct Cli {
    /// Index file.
    #[arg(long, global = true, env = "LEVIATHAN_INDEX", default_value = "leviathan.db")]
    index: PathBuf,
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
    /// Serve the index over the Model Context Protocol (stdio).
    Mcp,
    /// Print the config that registers Leviathan with an agent.
    Wrap {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(wrap::AGENTS))]
        agent: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("leviathan: {err:#}");
            ExitCode::FAILURE
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
        Command::Mcp => mcp::serve(&cli.index, opts)?,
        Command::Wrap { agent } => {
            let exe = std::env::current_exe()?;
            let index = std::path::absolute(&cli.index)?;
            print!("{}", wrap::recipe(&agent, &exe, &index)?);
        }
    }
    Ok(0)
}
