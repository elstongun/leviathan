//! Minimal Model Context Protocol server over stdio (JSON-RPC 2.0, one
//! message per line). Read-only: every tool is a fixed query shape over the
//! index; there is no SQL passthrough, no file access and no write path.
//! stdout carries protocol messages only; diagnostics go to stderr.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::card::CardOptions;
use crate::config::plural;
use crate::query::{Scope, SearchRequest, Sort, Store};
use crate::render;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Leviathan searches a large indexed dataset and returns a few ranked, cited records instead of raw data. \
Call `describe` once to learn the fields, groups and filter values. Then use `search`: words for ranked matches, \
`group` to scope to one entity, `where` for exact filters, `since`/`until` for dates, no words for newest first. \
If a group is ambiguous, show the candidates and ask; never guess. Results marked OTHER come from a different group: say so. \
Use `get` only when the full record is needed.";

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

pub fn tool_definitions(store: Option<&Store>) -> Value {
    let format = json!({"type": "string", "enum": ["text", "json"], "description": "text (default, fewest tokens) or json"});
    let mut search_desc = "Ranked records for free-text words (quote \"exact phrases\", -exclude words), optionally scoped \
        to one group, filtered by exact field values and a date range. With no words, lists newest first. Resolves the group \
        itself and returns candidates instead of guessing when it is ambiguous."
        .to_string();
    if let Some(summary) = store.and_then(dataset_summary) {
        search_desc = format!("{search_desc} {summary}");
    }
    json!([
        {
            "name": "search",
            "description": search_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to rank by. Omit to list newest first."},
                    "group": {"type": "string", "description": "Group key or name, as the user said it"},
                    "scope": {"type": "string", "enum": ["group", "others", "all"], "description": "With a group: group (default), others = every other group"},
                    "where": {"type": "object", "additionalProperties": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]}, "description": "Exact filters, e.g. {\"status\": \"open\"}; an array means any of"},
                    "since": {"type": "string", "description": "YYYY, YYYY-MM or YYYY-MM-DD"},
                    "until": {"type": "string", "description": "Inclusive, same formats"},
                    "sort": {"type": "string", "enum": ["relevance", "newest"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 25, "description": "Default 5"},
                    "offset": {"type": "integer", "minimum": 0},
                    "format": format
                }
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "resolve_group",
            "description": "Group key or name to candidate groups (key, name, record count, how it matched).",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "minimum": 1, "maximum": 25}, "format": format},
                "required": ["query"]
            },
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "get",
            "description": "One complete record by id, exactly as ingested.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        },
        {
            "name": "describe",
            "description": "The dataset's fields, groups, filter values with counts, date range and example calls.",
            "inputSchema": {"type": "object", "properties": {"format": format}},
            "annotations": {"readOnlyHint": true, "openWorldHint": false}
        }
    ])
}

struct Server {
    index: PathBuf,
    opts: CardOptions,
    store: Option<Store>,
}

impl Server {
    fn store(&mut self) -> Result<&Store> {
        match &mut self.store {
            Some(store) => {
                store.reload_if_replaced()?;
            }
            None => self.store = Some(Store::open(&self.index, self.opts)?),
        }
        Ok(self.store.as_ref().expect("store opened above"))
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let text = |k: &str| {
            args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        };
        let int = |k: &str| args.get(k).and_then(Value::as_u64).map(|v| v as usize);
        let json_out = text("format").as_deref() == Some("json");

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
            other => bail!("unknown tool {other:?}"),
        }
    }

    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str)?;
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
                let version =
                    PROTOCOL_VERSIONS.iter().find(|v| **v == asked).unwrap_or(&PROTOCOL_VERSIONS[0]);
                let summary = self.store().ok().and_then(dataset_summary);
                let instructions = match summary {
                    Some(s) => format!("{INSTRUCTIONS} {s}"),
                    None => INSTRUCTIONS.to_string(),
                };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "leviathan", "title": "Leviathan", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": instructions
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let store = self.store().ok();
                Ok(json!({"tools": tool_definitions(store)}))
            }
            "resources/list" => Ok(json!({"resources": []})),
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
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn serve(index: &Path, opts: CardOptions) -> Result<()> {
    let mut server = Server { index: index.to_path_buf(), opts, store: None };
    match server.store() {
        Ok(store) => {
            let d = store.describe(1)?;
            eprintln!("[leviathan] mcp serving {} ({} records)", index.display(), d.record_count);
        }
        Err(err) => eprintln!("[leviathan] mcp started without an index: {err:#}"),
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| server.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => server.handle(&msg),
            Err(err) => Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {err}")}}),
            ),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
