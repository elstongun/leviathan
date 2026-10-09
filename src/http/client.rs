//! The client side of `serve --http`: the `mcp --remote` stdio bridge and
//! one-shot tool calls (remote briefings).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub const TOKEN_ENV: &str = "LEVIATHAN_TOKEN";

/// The bearer token for a remote server: the token file, else the
/// environment variable `env` (normally $LEVIATHAN_TOKEN).
pub fn client_token(file: Option<&Path>, env: &str) -> Result<Option<String>> {
    if let Some(path) = file {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read token file {}", path.display()))?;
        return Ok(Some(text.trim().to_string()).filter(|t| !t.is_empty()));
    }
    let token = std::env::var(env).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    // `wrap` writes a `<your token…>` placeholder into configs.
    if token.as_deref().is_some_and(|t| t.starts_with('<')) {
        eprintln!(
            "[leviathan] ${env} is still the placeholder from `leviathan wrap`; put the real token there"
        );
        return Ok(None);
    }
    Ok(token)
}

fn agent(streaming: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(if streaming { None } else { Some(Duration::from_secs(60)) })
        .build()
        .into()
}

struct Posted {
    status: u16,
    session_id: Option<String>,
    messages: Vec<Value>,
    body: String,
}

/// POST one JSON-RPC body; collect the replies whether they come back as
/// JSON or as an SSE stream.
fn post(
    agent: &ureq::Agent,
    url: &str,
    token: Option<&str>,
    session_id: Option<&str>,
    protocol: Option<&str>,
    body: &str,
) -> Result<Posted> {
    let mut rb = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(t) = token {
        rb = rb.header("Authorization", format!("Bearer {t}"));
    }
    if let Some(s) = session_id {
        rb = rb.header("Mcp-Session-Id", s);
    }
    if let Some(p) = protocol {
        rb = rb.header("MCP-Protocol-Version", p);
    }
    let mut resp = rb.send(body).with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let header = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    let session_id = header("mcp-session-id");
    let sse = header("content-type").is_some_and(|c| c.starts_with("text/event-stream"));
    let mut messages = Vec::new();
    let mut text = String::new();
    if sse {
        let reader = BufReader::new(resp.body_mut().as_reader());
        let mut data = String::new();
        let flush = |data: &mut String, messages: &mut Vec<Value>| {
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                messages.push(v);
            }
            data.clear();
        };
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                flush(&mut data, &mut messages);
            } else if let Some(d) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.strip_prefix(' ').unwrap_or(d));
            }
        }
        flush(&mut data, &mut messages);
    } else {
        text = resp.body_mut().read_to_string().unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            messages.push(v);
        }
    }
    Ok(Posted { status, session_id, messages, body: text })
}

/// Call one tool on a remote MCP endpoint and return its text.
pub fn call_tool(url: &str, token: Option<&str>, name: &str, args: &Value) -> Result<String> {
    let agent = agent(false);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let posted = post(&agent, url, token, None, None, &body.to_string())?;
    if posted.status == 401 {
        bail!("{url} refused the token (HTTP 401): set {TOKEN_ENV} or --token-file");
    }
    if !(200..300).contains(&posted.status) {
        bail!("{url}: HTTP {} {}", posted.status, posted.body.chars().take(200).collect::<String>());
    }
    let reply =
        posted.messages.into_iter().find(|m| m.get("id").is_some()).context("no reply from the server")?;
    if let Some(err) = reply.get("error") {
        bail!("{}", err["message"].as_str().unwrap_or("remote error"));
    }
    let result = &reply["result"];
    let text = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if result["isError"].as_bool() == Some(true) {
        bail!("{}", text.trim_start_matches("error: "));
    }
    Ok(text)
}

fn reply_error(out: &mut impl Write, id: Option<&Value>, text: &str) -> Result<()> {
    eprintln!("[leviathan] {text}");
    if let Some(id) = id {
        writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32001, "message": text}}))?;
        out.flush()?;
    }
    Ok(())
}

/// `leviathan mcp --remote URL`: a stdio MCP server that forwards every
/// message to a remote MCP endpoint, for agents that only speak stdio or
/// cannot send headers.
pub fn bridge(url: &str, token: Option<&str>) -> Result<()> {
    let agent = agent(true);
    let mut session_id: Option<String> = None;
    let mut protocol: Option<String> = None;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    eprintln!("[leviathan] bridging stdio to {url}{}", if token.is_some() { " (with token)" } else { "" });
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let id = msg.get("id").filter(|_| msg.get("method").is_some());
        let is_init = msg.get("method").and_then(Value::as_str) == Some("initialize");
        let posted = match post(&agent, url, token, session_id.as_deref(), protocol.as_deref(), &line) {
            Ok(p) => p,
            Err(err) => {
                reply_error(&mut stdout, id, &format!("remote Leviathan unreachable: {err:#}"))?;
                continue;
            }
        };
        if posted.status == 401 {
            reply_error(
                &mut stdout,
                id,
                &format!("{url} refused the token (HTTP 401): set {TOKEN_ENV} or --token-file"),
            )?;
            continue;
        }
        if posted.status == 404 && session_id.is_some() {
            session_id = None;
        }
        if posted.session_id.is_some() {
            session_id = posted.session_id;
        }
        if !(200..300).contains(&posted.status) && posted.messages.is_empty() {
            reply_error(&mut stdout, id, &format!("{url}: HTTP {}", posted.status))?;
            continue;
        }
        for m in posted.messages {
            if is_init && let Some(v) = m.pointer("/result/protocolVersion").and_then(Value::as_str) {
                protocol = Some(v.to_string());
            }
            writeln!(stdout, "{m}")?;
        }
        stdout.flush()?;
    }
    Ok(())
}
