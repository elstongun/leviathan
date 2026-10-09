//! Model Context Protocol server (JSON-RPC 2.0). [`Session`] holds the tools;
//! [`serve`] runs it over stdio (one message per line) and `http` serves the
//! same session over Streamable HTTP.
//!
//! The data tools are read-only: fixed query shapes over the index, with no
//! SQL passthrough and no file access. Writes exist only for the opt-in
//! memory store (`--memory`), whose own file is the only thing they touch.
//! stdout carries protocol messages only; diagnostics go to stderr.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::card::CardOptions;
use crate::config::plural;
use crate::memory::{Forget, Kind, Memory, RecallRequest, Remember, Settings};
use crate::query::{Scope, SearchRequest, Sort, Store};
use crate::render;

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const DATA_INSTRUCTIONS: &str = "Leviathan searches a large indexed dataset and returns a few ranked, cited records instead of raw data. \
Call `describe` once to learn the fields, groups and filter values. Then use `search`: words for ranked matches, \
`group` to scope to one entity, `where` for exact filters, `since`/`until` for dates, no words for newest first. \
If a group is ambiguous, show the candidates and ask; never guess. Results marked OTHER come from a different group: say so. \
Use `get` only when the full record is needed.";

const MEMORY_INSTRUCTIONS: &str = "Leviathan memory: call `recall` with no arguments once at the start of a session, and with \
words or a `subject` before acting on anything you may have learned before. `remember` durable facts, preferences, \
decisions (with the reason) and lessons, one claim each; give anything that can change a `key` and a `subject` so the \
next write replaces it. Never store secrets, credentials or transient state. Superseded memories are history, not truth.";

const RECALL_ONLY_INSTRUCTIONS: &str = "Leviathan memory (read-only here): call `recall` with no arguments once at the start \
of a session, and with words or a `subject` before acting on anything you may have learned before.";

/// Which memory tools a client gets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum MemoryTools {
    /// remember, recall, forget, memory_describe.
    #[default]
    All,
    /// recall and memory_describe only.
    Recall,
}

#[derive(Debug, Clone)]
pub struct MemoryOptions {
    pub path: PathBuf,
    pub settings: Settings,
    pub tools: MemoryTools,
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub index: PathBuf,
    pub card: CardOptions,
    pub memory: Option<MemoryOptions>,
}

/// One line summarizing the open index, appended to tool descriptions so the
/// agent knows the data's shape before its first call.
fn dataset_summary(store: &Store) -> Option<String> {
    let d = store.describe(5).ok()?;
    let (record, group) = (&d.about.record, &d.about.group);
    let mut s = format!(
        "This index: {}{} {}",
        d.about.name.as_deref().map(|n| format!("{n}, ")).unwrap_or_default(),
        render::thousands(d.record_count),
        plural(record)
    );
    if let Some(g) = &d.fields.group {
        s.push_str(&format!(" across {} {} (group = {g})", render::thousands(d.group_count), plural(group)));
    }
    if let (Some(a), Some(b)) = (&d.date_min, &d.date_max) {
        s.push_str(&format!(", dated {} to {}", &a[..a.len().min(10)], &b[..b.len().min(10)]));
    }
    if !d.filters.is_empty() {
        let parts: Vec<String> = d
            .filters
            .iter()
            .map(|f| {
                let vals: Vec<&str> = f.top.iter().take(4).map(|(v, _)| v.as_str()).collect();
                let more = if f.distinct > vals.len() as i64 { ", ..." } else { "" };
                format!("{} ({}{more})", f.field, vals.join(", "))
            })
            .collect();
        s.push_str(&format!(". Filters: {}", parts.join("; ")));
    }
    s.push('.');
    Some(s)
}

fn format_param() -> Value {
    json!({"type": "string", "enum": ["text", "json"], "description": "text (default, fewest tokens) or json"})
}

fn data_tools(store: Option<&Store>) -> Vec<Value> {
    let format = format_param();
    let mut search_desc = "Ranked records for free-text words (quote \"exact phrases\", -exclude words), optionally scoped \
        to one group, filtered by exact field values and a date range. With no words, lists newest first. Resolves the group \
        itself and returns candidates instead of guessing when it is ambiguous."
        .to_string();
    if let Some(summary) = store.and_then(dataset_summary) {
        search_desc = format!("{search_desc} {summary}");
    }
    vec![
        json!({
            "name": "search",
            "description": search_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to rank by. Omit to list newest first."},
                    "group": {"type": "string", "description": "Group key or name, as the user said it"},
                    "scope": {"type": "string", "enum": ["group", "others", "all"], "description": "With a group: group (default), others = every other group"},
                    "where": {"type": "object", "additionalProperties": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]}, "description": "Exact filters, e.g. {\"status\": \"open\"}; an array means any of"},
                    "where_list": {"type": "array", "items": {"type": "string"}, "description": "Same filters as field=value strings, e.g. [\"status=open\"]"},
                    "since": {"type": "string", "description": "YYYY, YYYY-MM or YYYY-MM-DD"},
                    "until": {"type": "string", "description": "Inclusive, same formats"},
                    "sort": {"type": "string", "enum": ["relevance", "newest"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 25, "description": "Default 5"},
                    "offset": {"type": "integer", "minimum": 0},
                    "format": format
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "resolve_group",
            "description": "Group key or name to candidate groups (key, name, record count, how it matched).",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 25}, "format": format},
                "required": ["query"]
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "get",
            "description": "One complete record by id, exactly as ingested.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
        json!({
            "name": "describe",
            "description": "The dataset's fields, groups, filter values with counts, date range and example calls.",
            "inputSchema": {"type": "object", "properties": {"format": format}},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }),
    ]
}

/// Memory tool schemas are flat (no anyOf, no free-form objects) so every
/// client's schema dialect accepts them, Gemini and OpenAI strict included.
/// Memory tools. Schemas list what agents use; `limit`, `until` and
/// `format` are accepted without being advertised, to keep every session
/// cheap.
fn memory_tools(tools: MemoryTools, namespace: &str, data: bool) -> Vec<Value> {
    let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    let mut recall_props = json!({
        "query": {"type": "string", "description": "Words to rank by; omit for the briefing"},
        "subject": {"type": "string", "description": "Only about this person, project or thing"},
        "kind": {"type": "string", "description": format!("Comma list: {}", Kind::NAMES)},
        "ns": {"type": "string", "description": "Namespaces, comma list, or *"},
        "since": {"type": "string", "description": "YYYY, YYYY-MM or YYYY-MM-DD"},
        "as_of": {"type": "string", "description": "What was true at the end of this date"},
        "history": {"type": "boolean", "description": "Include replaced and forgotten memories"},
        "budget": {"type": "integer", "description": "Tokens; default 800"}
    });
    if data {
        recall_props["with_records"] =
            json!({"type": "boolean", "description": "Add the indexed records memories reference"});
    }
    let mut out = vec![json!({
        "name": "recall",
        "description": "Current memories ranked by relevance, recency and importance, within a token budget. No arguments at \
            session start; words or a subject before acting on anything you may know.",
        "inputSchema": {"type": "object", "properties": recall_props},
        "annotations": {"readOnlyHint": true, "openWorldHint": false}
    })];
    if tools == MemoryTools::All {
        let mut props = json!({
            "text": {"type": "string", "description": "One claim, in a sentence"},
            "kind": {"type": "string", "enum": kinds},
            "subject": {"type": "string", "description": "Who or what it is about"},
            "key": {"type": "string", "description": "The slot that can change: editor, deploy.target, owner"},
            "importance": {"type": "integer", "minimum": 1, "maximum": 5, "description": "Default 3"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "source": {"type": "string", "description": "user, observed, a document"},
            "pinned": {"type": "boolean", "description": "Lead every briefing"},
            "expires": {"type": "string", "description": "A date, or 12h, 30d, 6w"},
            "aliases": {"type": "array", "items": {"type": "string"}, "description": "Other names for the subject"},
            "ns": {"type": "string", "description": format!("Default {namespace}")}
        });
        if data {
            props["refs"] =
                json!({"type": "array", "items": {"type": "string"}, "description": "Related record ids"});
        }
        out.push(json!({
            "name": "remember",
            "description": "Store one durable fact, preference, decision (with why), lesson, event or task. Give anything that \
                can change a subject and key, so the next write replaces it. Restatements merge; secrets are refused.",
            "inputSchema": {"type": "object", "properties": props, "required": ["text"]},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
        }));
        out.push(json!({
            "name": "forget",
            "description": "Mark a memory no longer true (kept in history): its id, or a subject and key.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "subject": {"type": "string"},
                    "key": {"type": "string"},
                    "ns": {"type": "string"},
                    "reason": {"type": "string"}
                }
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false}
        }));
    }
    out.push(json!({
        "name": "memory_describe",
        "description": "Memory counts by kind, namespace and subject.",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": true, "openWorldHint": false}
    }));
    out
}

/// Tool list for a server with or without data and memory. Used by `wrap`
/// (to estimate schema cost) and the function-calling exports.
pub fn tool_definitions(store: Option<&Store>, memory: Option<(MemoryTools, &str)>, data: bool) -> Value {
    let mut tools = if data { data_tools(store) } else { Vec::new() };
    if let Some((which, ns)) = memory {
        tools.extend(memory_tools(which, ns, data));
    }
    Value::Array(tools)
}

/// Tool state for one client connection.
pub struct Session {
    opts: ServerOptions,
    store: Option<Store>,
    memory: Option<Memory>,
}

fn text_arg(args: &Value, k: &str) -> Option<String> {
    args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn int_arg(args: &Value, k: &str) -> Option<usize> {
    args.get(k)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .map(|v| v as usize)
}

fn bool_arg(args: &Value, k: &str) -> bool {
    match args.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim(), "true" | "1" | "yes"),
        _ => false,
    }
}

/// An array of strings, or one comma-separated string.
fn list_arg(args: &Value, k: &str) -> Vec<String> {
    match args.get(k) {
        Some(Value::Array(items)) => {
            items.iter().filter_map(scalar).filter(|s| !s.trim().is_empty()).collect()
        }
        Some(Value::String(s)) => {
            s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
        }
        _ => Vec::new(),
    }
}

fn kinds_arg(args: &Value) -> Result<Vec<Kind>> {
    list_arg(args, "kind")
        .iter()
        .map(|k| Kind::parse(k).ok_or_else(|| anyhow::anyhow!("unknown kind {k:?}; use {}", Kind::NAMES)))
        .collect()
}

impl Session {
    pub fn new(opts: ServerOptions) -> Self {
        Self { opts, store: None, memory: None }
    }

    /// Data tools are offered unless this is a memory server without an index.
    pub fn data_enabled(&self) -> bool {
        self.opts.memory.is_none() || self.opts.index.exists()
    }

    pub fn memory_tools(&self) -> Option<MemoryTools> {
        self.opts.memory.as_ref().map(|m| m.tools)
    }

    fn ensure_store(&mut self) -> Result<()> {
        match &mut self.store {
            Some(store) => {
                store.reload_if_replaced()?;
            }
            None => self.store = Some(Store::open(&self.opts.index, self.opts.card)?),
        }
        Ok(())
    }

    fn store(&mut self) -> Result<&Store> {
        self.ensure_store()?;
        Ok(self.store.as_ref().expect("store opened above"))
    }

    fn ensure_memory(&mut self) -> Result<()> {
        if self.memory.is_none() {
            let Some(m) = &self.opts.memory else {
                bail!("memory is not enabled on this server (start it with --memory)")
            };
            self.memory = Some(Memory::open(&m.path, m.settings.clone())?);
        }
        Ok(())
    }

    /// Open both stores now so startup problems show up in the log.
    pub fn warm_up(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.data_enabled() {
            match self.store().and_then(|s| s.describe(1)) {
                Ok(d) => {
                    lines.push(format!("index {} ({} records)", self.opts.index.display(), d.record_count))
                }
                Err(err) => lines.push(format!("no index yet: {err:#}")),
            }
        }
        if self.opts.memory.is_some() {
            match self.ensure_memory().and_then(|_| self.memory.as_ref().expect("opened").stats()) {
                Ok(s) => lines.push(format!(
                    "memory {} ({} current, namespace {})",
                    s.path.display(),
                    s.current,
                    s.namespace
                )),
                Err(err) => lines.push(format!("memory unavailable: {err:#}")),
            }
        }
        lines
    }

    pub fn tools(&mut self) -> Value {
        let data = self.data_enabled();
        let opened = data && self.ensure_store().is_ok();
        let store = if opened { self.store.as_ref() } else { None };
        let memory = self.opts.memory.as_ref().map(|m| (m.tools, m.settings.namespace.as_str()));
        tool_definitions(store, memory, data)
    }

    pub fn instructions(&mut self) -> String {
        let mut parts = Vec::new();
        if self.data_enabled() {
            parts.push(DATA_INSTRUCTIONS.to_string());
            if let Some(summary) = self.store().ok().and_then(dataset_summary) {
                parts.push(summary);
            }
        }
        match self.memory_tools() {
            Some(MemoryTools::All) => parts.push(MEMORY_INSTRUCTIONS.into()),
            Some(MemoryTools::Recall) => parts.push(RECALL_ONLY_INSTRUCTIONS.into()),
            None => {}
        }
        parts.join(" ")
    }

    /// Run one tool. Text by default; `format: json` for structured output.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let text = |k: &str| text_arg(args, k);
        let int = |k: &str| int_arg(args, k);
        let json_out = text("format").as_deref() == Some("json");
        let memory_tool = matches!(name, "remember" | "recall" | "forget" | "memory_describe");
        if memory_tool {
            if self.opts.memory.is_none() {
                bail!("memory is not enabled on this server (start it with --memory)");
            }
            if self.memory_tools() == Some(MemoryTools::Recall) && matches!(name, "remember" | "forget") {
                bail!("this server exposes memory read-only (--memory-tools recall)");
            }
        } else if !self.data_enabled() && matches!(name, "search" | "resolve_group" | "get" | "describe") {
            bail!("no data index on this server; it serves memory only");
        }

        match name {
            "search" => {
                let scope = match text("scope").as_deref() {
                    None => None,
                    Some("group") => Some(Scope::Group),
                    Some("others") => Some(Scope::Others),
                    Some("all") => Some(Scope::All),
                    Some(other) => bail!("unknown scope {other:?}"),
                };
                let sort = match text("sort").as_deref() {
                    None | Some("relevance") => Sort::Relevance,
                    Some("newest") => Sort::Newest,
                    Some(other) => bail!("unknown sort {other:?}"),
                };
                let mut filters = Vec::new();
                if let Some(Value::Object(map)) = args.get("where") {
                    for (field, v) in map {
                        match v {
                            Value::Array(items) => {
                                filters.extend(items.iter().filter_map(scalar).map(|s| (field.clone(), s)))
                            }
                            other => filters.extend(scalar(other).map(|s| (field.clone(), s))),
                        }
                    }
                }
                for f in list_arg(args, "where_list") {
                    match f.split_once('=') {
                        Some((k, v)) if !k.trim().is_empty() => {
                            filters.push((k.trim().into(), v.trim().into()))
                        }
                        _ => bail!("where_list items look like field=value, got {f:?}"),
                    }
                }
                let req = SearchRequest {
                    group: text("group"),
                    query: text("query").unwrap_or_default(),
                    scope,
                    filters,
                    since: text("since"),
                    until: text("until"),
                    sort,
                    limit: int("limit").unwrap_or(5).clamp(1, 25),
                    offset: int("offset").unwrap_or(0),
                    fallback: true,
                };
                let outcome = self.store()?.search(&req)?;
                Ok(if json_out { serde_json::to_string(&outcome)? } else { render::search(&outcome) })
            }
            "resolve_group" => {
                let query = text("query").ok_or_else(|| anyhow::anyhow!("`query` is required"))?;
                let store = self.store()?;
                let found = store.resolve(&query, int("limit").unwrap_or(10).clamp(1, 25))?;
                let about = &store.config().about;
                Ok(if json_out {
                    serde_json::to_string(
                        &json!({"query": query, "candidates": found, "ambiguous": found.len() > 1}),
                    )?
                } else {
                    render::resolve(&query, &found, &about.record, &about.group)
                })
            }
            "get" => {
                let id = text("id").ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
                match self.store()?.get(&id)? {
                    Some(doc) => Ok(serde_json::to_string(&doc)?),
                    None => bail!("no record with id {id:?}"),
                }
            }
            "describe" => {
                let d = self.store()?.describe(8)?;
                Ok(if json_out { serde_json::to_string(&d)? } else { render::describe(&d) })
            }
            "remember" => {
                let req = Remember {
                    text: text("text")
                        .ok_or_else(|| anyhow::anyhow!("`text` is required: the one thing to remember"))?,
                    kind: match text("kind") {
                        Some(k) => Some(
                            Kind::parse(&k)
                                .ok_or_else(|| anyhow::anyhow!("unknown kind {k:?}; use {}", Kind::NAMES))?,
                        ),
                        None => None,
                    },
                    subject: text("subject"),
                    key: text("key"),
                    tags: list_arg(args, "tags"),
                    importance: int("importance").map(|i| i.min(255) as u8),
                    confidence: args.get("confidence").and_then(Value::as_f64),
                    source: text("source"),
                    refs: list_arg(args, "refs"),
                    pinned: bool_arg(args, "pinned"),
                    expires: text("expires"),
                    aliases: list_arg(args, "aliases"),
                    ns: text("ns"),
                };
                self.ensure_memory()?;
                let out = self.memory.as_mut().expect("opened").remember(&req)?;
                Ok(if json_out { serde_json::to_string(&out)? } else { out.render() })
            }
            "recall" => {
                let req = RecallRequest {
                    query: text("query").unwrap_or_default(),
                    ns: text("ns"),
                    subject: text("subject"),
                    kinds: kinds_arg(args)?,
                    since: text("since"),
                    until: text("until"),
                    as_of: text("as_of"),
                    history: bool_arg(args, "history"),
                    budget: int("budget"),
                    limit: int("limit"),
                    with_records: bool_arg(args, "with_records"),
                    briefing: text("query").is_none()
                        && text("subject").is_none()
                        && args.get("kind").is_none(),
                };
                if req.with_records && self.data_enabled() {
                    let _ = self.ensure_store();
                }
                self.ensure_memory()?;
                let store = self.store.as_ref();
                let out = self.memory.as_mut().expect("opened").recall(&req, store)?;
                Ok(if json_out { serde_json::to_string(&out)? } else { out.render() })
            }
            "forget" => {
                let f = Forget {
                    id: text("id"),
                    subject: text("subject"),
                    key: text("key"),
                    ns: text("ns"),
                    reason: text("reason"),
                };
                self.ensure_memory()?;
                let m = self.memory.as_mut().expect("opened").forget(&f)?;
                Ok(if json_out {
                    serde_json::to_string(&m)?
                } else {
                    format!("forgot {}\n  {}\n", m.id, m.line(&crate::now_rfc3339(), false))
                })
            }
            "memory_describe" => {
                self.ensure_memory()?;
                let s = self.memory.as_ref().expect("opened").stats()?;
                Ok(if json_out { serde_json::to_string(&s)? } else { render::memory_stats(&s) })
            }
            other => bail!("unknown tool {other:?}"),
        }
    }

    /// One JSON-RPC message in, at most one reply out (none for notifications).
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str)?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
                let version =
                    PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "leviathan", "title": "Leviathan", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": self.instructions()
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools()})),
            "resources/list" => Ok(json!({"resources": []})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(match self.call(&name, &args) {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}], "isError": false}),
                    Err(err) => {
                        json!({"content": [{"type": "text", "text": format!("error: {err:#}")}], "isError": true})
                    }
                })
            }
            _ if id.is_none() => return None,
            other => Err((-32601, format!("method not found: {other}"))),
        };
        let id = id?;
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// A raw request body: one message or a batch. `None` when nothing needs
    /// a reply (notifications only).
    pub fn handle_body(&mut self, body: &str) -> Option<Value> {
        match serde_json::from_str::<Value>(body) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => self.handle(&msg),
            Err(err) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {err}")}}),
            ),
        }
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn serve(opts: ServerOptions) -> Result<()> {
    let mut session = Session::new(opts);
    for line in session.warm_up() {
        eprintln!("[leviathan] mcp {line}");
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = session.handle_body(&line) {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
