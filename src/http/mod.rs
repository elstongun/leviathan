//! `leviathan serve --http`: the same tools over the network, for agents on
//! other machines and hosted apps.
//!
//! - `POST /mcp`: MCP Streamable HTTP with JSON responses. The server never
//!   pushes, so `GET /mcp` is 405, as the spec allows.
//! - `POST /v1/<tool>` and `GET /openapi.json`: plain REST for
//!   function-calling frameworks and ChatGPT GPT Actions.
//! - `GET /healthz`.
//! - OAuth 2.1 endpoints with `--auth oauth` (see `oauth.rs`).
//!
//! Plain HTTP only: put TLS in front (Caddy, a tunnel) for anything beyond
//! localhost or a private network.

mod client;

use std::io::{Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

pub use client::{TOKEN_ENV, bridge, call_tool, client_token};

use crate::mcp::{self, ServerOptions, Session};
use crate::oauth::{self, Limit, OAuth, Reply};

const MAX_BODY: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// A bearer token from the token file.
    Token,
    /// OAuth 2.1 for hosted apps; the token file's token works too.
    OAuth,
    /// No auth: loopback binds only.
    None,
}

#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub server: ServerOptions,
    pub addr: String,
    pub auth: AuthMode,
    pub token_file: PathBuf,
    /// The URL clients reach this server at (required for OAuth).
    pub public_url: Option<String>,
    /// Extra browser origins allowed to call (loopback origins always are).
    pub allow_origins: Vec<String>,
    pub threads: usize,
}

struct Shared {
    auth: AuthMode,
    token: Option<String>,
    base_url: String,
    allow_origins: Vec<String>,
    openapi: String,
    oauth: Option<(PathBuf, oauth::Shared)>,
}

/// A running server: `wait` blocks (the CLI), `stop` shuts it down (tests).
pub struct Running {
    pub addr: SocketAddr,
    /// The MCP endpoint.
    pub url: String,
    server: Arc<Server>,
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl Running {
    /// The approval code for a pending OAuth request (what the log prints).
    pub fn approval_code(&self, request_id: &str) -> Option<String> {
        self.shared.oauth.as_ref()?.1.approval_code(request_id)
    }

    pub fn wait(self) {
        for w in self.workers {
            let _ = w.join();
        }
    }

    pub fn stop(self) {
        for _ in &self.workers {
            self.server.unblock();
        }
        self.wait();
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The host of `scheme://host[:port]/...`, without the port.
fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => host.to_string(),
    })
}

/// `scheme://host[:port]` of a URL, lowercased.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

/// Read the token file, creating it (mode 600) with a fresh token if missing.
pub fn ensure_token(path: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let token = text.trim().to_string();
        if token.len() < 16 {
            bail!("{} holds a token shorter than 16 characters; delete it to generate one", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(path)
                && meta.permissions().mode() & 0o077 != 0
            {
                eprintln!("[leviathan] warning: {} is readable by other users; chmod 600 it", path.display());
            }
        }
        return Ok(token);
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let token = format!("lvt_{}", oauth::random_token(32));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("create {}", path.display()))?;
    writeln!(f, "{token}")?;
    eprintln!(
        "[leviathan] created token file {} (clients send it as `Authorization: Bearer <token>` or set {TOKEN_ENV})",
        path.display()
    );
    Ok(token)
}

/// The OpenAPI document for `/v1/*`. Built without the dataset summary: it
/// is served without auth, so it must not describe the data.
pub fn openapi(server: &ServerOptions, base_url: &str) -> Value {
    let data = Session::new(server.clone()).data_enabled();
    let memory = server.memory.as_ref().map(|m| (m.tools, m.settings.namespace.as_str()));
    let tools = mcp::tool_definitions(None, memory, data);
    let mut paths = serde_json::Map::new();
    for tool in tools.as_array().into_iter().flatten() {
        let name = tool["name"].as_str().unwrap_or_default();
        // GPT Actions caps descriptions at 300 characters.
        let description: String =
            tool["description"].as_str().unwrap_or_default().chars().take(300).collect();
        let summary = description.split(". ").next().unwrap_or(name).chars().take(120).collect::<String>();
        paths.insert(
            format!("/v1/{name}"),
            json!({"post": {
                "operationId": name,
                "summary": summary,
                "description": description,
                "requestBody": {"required": true, "content": {"application/json": {"schema": tool["inputSchema"]}}},
                "responses": {
                    "200": {"description": "The tool result as JSON", "content": {"application/json": {"schema": {"type": "object"}}}},
                    "400": {"description": "Bad arguments or a refused write",
                            "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Error"}}}},
                    "401": {"description": "Missing or wrong bearer token"}
                }
            }}),
        );
    }
    json!({
        "openapi": "3.1.0",
        "info": {"title": "Leviathan", "version": env!("CARGO_PKG_VERSION"),
                 "description": "Search an index of records and read and write agent memory."},
        "servers": [{"url": base_url}],
        "paths": paths,
        "components": {
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
            "schemas": {"Error": {"type": "object", "properties": {"error": {"type": "string"}}, "required": ["error"]}}
        },
        "security": [{"bearer": []}]
    })
}

/// Bind and start the worker threads.
pub fn start(opts: HttpOptions) -> Result<Running> {
    let addr: SocketAddr = opts
        .addr
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .with_context(|| format!("bad --http address {:?}; use host:port", opts.addr))?;
    let loopback = addr.ip().is_loopback();
    if opts.auth == AuthMode::None && !loopback {
        bail!("--auth none is only allowed on a loopback address (127.0.0.1 or ::1), not {addr}");
    }
    let public_url = opts.public_url.as_deref().map(|u| u.trim_end_matches('/').to_string());
    if let Some(u) = &public_url {
        let ok = u.starts_with("https://")
            || (u.starts_with("http://") && host_of(u).is_some_and(|h| is_loopback_host(&h)));
        if !ok {
            bail!("--public-url must be https:// (or http://localhost for testing), got {u}");
        }
    }
    if opts.auth == AuthMode::OAuth && public_url.is_none() && !loopback {
        bail!("--auth oauth needs --public-url: the https:// URL clients reach this server at");
    }
    let token = match opts.auth {
        AuthMode::None => None,
        _ => Some(ensure_token(&opts.token_file)?),
    };
    let server = Server::http(addr).map_err(|e| anyhow::anyhow!("listen on {addr}: {e}"))?;
    let addr = server.server_addr().to_ip().unwrap_or(addr);
    let base_url = public_url.unwrap_or_else(|| format!("http://{addr}"));
    let oauth = (opts.auth == AuthMode::OAuth).then(|| {
        let db = opts.token_file.with_file_name("leviathan-auth.db");
        (db, oauth::Shared::new(&base_url))
    });
    if let Some((db, shared)) = &oauth {
        OAuth::open(db, shared.clone())?;
    }
    for line in Session::new(opts.server.clone()).warm_up() {
        eprintln!("[leviathan] http {line}");
    }
    let shared = Arc::new(Shared {
        auth: opts.auth,
        token,
        openapi: openapi(&opts.server, &base_url).to_string(),
        base_url: base_url.clone(),
        allow_origins: opts
            .allow_origins
            .iter()
            .map(|o| o.trim_end_matches('/').to_ascii_lowercase())
            .collect(),
        oauth,
    });
    let server = Arc::new(server);
    let workers = (0..opts.threads.clamp(1, 64))
        .map(|_| {
            let (server, shared, options) = (Arc::clone(&server), Arc::clone(&shared), opts.server.clone());
            std::thread::spawn(move || worker(&server, &shared, options))
        })
        .collect();
    let running_shared = Arc::clone(&shared);
    let auth = match opts.auth {
        AuthMode::Token => format!("bearer token in {}", opts.token_file.display()),
        AuthMode::OAuth => {
            format!("OAuth 2.1 (issuer {base_url}) or the token in {}", opts.token_file.display())
        }
        AuthMode::None => "none (loopback only)".into(),
    };
    eprintln!("[leviathan] MCP at {base_url}/mcp, REST at {base_url}/v1/<tool>; auth: {auth}");
    if !loopback && !base_url.starts_with("https://") {
        eprintln!("[leviathan] warning: listening beyond loopback over plain HTTP; put TLS in front of it");
    }
    Ok(Running { addr, url: format!("{base_url}/mcp"), server, shared: running_shared, workers })
}

pub fn serve(opts: HttpOptions) -> Result<()> {
    start(opts)?.wait();
    Ok(())
}

fn worker(server: &Server, shared: &Shared, options: ServerOptions) {
    let mut session = Session::new(options);
    let oauth = shared.oauth.as_ref().and_then(|(db, s)| match OAuth::open(db, s.clone()) {
        Ok(o) => Some(o),
        Err(err) => {
            eprintln!("[leviathan] oauth database: {err:#}");
            None
        }
    });
    while let Ok(req) = server.recv() {
        handle(req, shared, &mut session, oauth.as_ref());
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn hdr(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("ASCII header")
}

struct Out {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, String)>,
}

impl Out {
    fn json(status: u16, v: &Value) -> Self {
        Out { status, content_type: "application/json", body: v.to_string(), headers: Vec::new() }
    }

    fn text(status: u16, body: &str) -> Self {
        Out { status, content_type: "text/plain; charset=utf-8", body: body.to_string(), headers: Vec::new() }
    }

    fn empty(status: u16) -> Self {
        Self::text(status, "")
    }

    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, &json!({"error": msg}))
    }
}

impl From<Reply> for Out {
    fn from(r: Reply) -> Self {
        Out { status: r.status, content_type: r.content_type, body: r.body, headers: r.headers }
    }
}

fn origin_allowed(origin: &str, shared: &Shared) -> bool {
    let o = origin.trim_end_matches('/').to_ascii_lowercase();
    shared.allow_origins.iter().any(|a| a == "*" || *a == o)
        || origin_of(&shared.base_url).is_some_and(|b| b == o)
        || host_of(&o).is_some_and(|h| is_loopback_host(&h))
}

/// `Authorization: Bearer …` matches the static token or a live OAuth token.
fn authorized(req: &Request, shared: &Shared, oauth: Option<&OAuth>) -> bool {
    if shared.auth == AuthMode::None {
        return true;
    }
    let token = header(req, "Authorization").and_then(|v| {
        let (scheme, rest) = v.trim().split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
    });
    let Some(token) = token else { return false };
    shared.token.as_deref().is_some_and(|t| oauth::same(t, token))
        || oauth.is_some_and(|o| o.check_access(token))
}

fn unauthorized(oauth: Option<&OAuth>) -> Out {
    let mut out = Out::error(401, "missing or invalid bearer token");
    let challenge = match oauth {
        Some(o) => o.www_authenticate(),
        None => "Bearer realm=\"leviathan\"".into(),
    };
    out.headers.push(("WWW-Authenticate", challenge));
    out
}

fn basic_auth(req: &Request) -> Option<(String, String)> {
    let (scheme, rest) = header(req, "Authorization")?.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (id, secret) = text.split_once(':')?;
    Some((oauth::percent_decode(id), oauth::percent_decode(secret)))
}

fn handle(mut req: Request, shared: &Shared, session: &mut Session, oauth: Option<&OAuth>) {
    let origin = header(&req, "Origin").map(str::to_string);
    let mut body = String::new();
    let unreadable = req.as_reader().take(MAX_BODY + 1).read_to_string(&mut body).is_err();
    let out = if origin.as_deref().is_some_and(|o| !origin_allowed(o, shared)) {
        Out::error(403, "origin not allowed (see --allow-origin)")
    } else if unreadable || body.len() as u64 > MAX_BODY {
        Out::error(413, "request body too large or not UTF-8")
    } else {
        route(&req, &body, shared, session, oauth)
    };
    let mut resp = Response::from_string(out.body)
        .with_status_code(out.status)
        .with_header(hdr("Content-Type", out.content_type));
    for (k, v) in &out.headers {
        resp = resp.with_header(hdr(k, v));
    }
    if let Some(o) = origin.filter(|o| origin_allowed(o, shared)) {
        resp = resp
            .with_header(hdr("Access-Control-Allow-Origin", &o))
            .with_header(hdr("Vary", "Origin"))
            .with_header(hdr("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS"))
            .with_header(hdr(
                "Access-Control-Allow-Headers",
                "authorization, content-type, accept, mcp-session-id, mcp-protocol-version, last-event-id",
            ))
            .with_header(hdr("Access-Control-Expose-Headers", "mcp-session-id, www-authenticate"));
    }
    let _ = req.respond(resp);
}

/// Who is asking, for OAuth rate limits: the peer address, or the address a
/// proxy on this machine (Caddy, cloudflared, tailscale) appended last to
/// `X-Forwarded-For`. IPv6 counts per /64.
fn caller(req: &Request) -> String {
    let peer = req.remote_addr().map(|a| a.ip());
    let forwarded =
        || header(req, "X-Forwarded-For")?.rsplit(',').next()?.trim().parse::<std::net::IpAddr>().ok();
    let ip = match peer {
        Some(ip) if ip.is_loopback() => forwarded().unwrap_or(ip),
        Some(ip) => ip,
        None => return "unknown".into(),
    };
    match ip {
        std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().expect("checked").to_string(),
        v4 => v4.to_string(),
    }
}

fn oauth_only(oauth: Option<&OAuth>, f: impl FnOnce(&OAuth) -> Reply) -> Out {
    match oauth {
        Some(o) => f(o).into(),
        None => Out::error(404, "OAuth is off (start the server with --auth oauth)"),
    }
}

fn route(req: &Request, body: &str, shared: &Shared, session: &mut Session, oauth: Option<&OAuth>) -> Out {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
    let method = req.method().clone();
    let protected = path == "/mcp" || path.starts_with("/v1/");
    if protected && method != Method::Options && !authorized(req, shared, oauth) {
        return unauthorized(oauth);
    }
    match (&method, path) {
        (Method::Options, _) => Out::empty(204),
        (Method::Get, "/") => Out::text(
            200,
            "Leviathan: MCP at POST /mcp, REST at POST /v1/<tool>, schema at GET /openapi.json\n",
        ),
        (Method::Get, "/healthz") => {
            Out::json(200, &json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
        }
        (Method::Get, "/openapi.json") => Out {
            status: 200,
            content_type: "application/json",
            body: shared.openapi.clone(),
            headers: Vec::new(),
        },
        (Method::Get, p) if p.starts_with("/.well-known/oauth-protected-resource") => {
            oauth_only(oauth, |o| Reply::json(200, o.protected_resource_metadata()))
        }
        (Method::Get, p)
            if p.starts_with("/.well-known/oauth-authorization-server")
                || p.starts_with("/.well-known/openid-configuration") =>
        {
            oauth_only(oauth, |o| Reply::json(200, o.authorization_server_metadata()))
        }
        (Method::Post, "/oauth/register") => oauth_only(oauth, |o| {
            o.limited(Limit::Register, &caller(req)).unwrap_or_else(|| o.register(body))
        }),
        (Method::Get, "/oauth/authorize") => oauth_only(oauth, |o| {
            let who = caller(req);
            o.limited(Limit::Authorize, &who)
                .unwrap_or_else(|| o.authorize_page(&oauth::parse_form(query), &who))
        }),
        (Method::Post, "/oauth/authorize") => oauth_only(oauth, |o| {
            o.limited(Limit::Approve, &caller(req))
                .unwrap_or_else(|| o.authorize_submit(&oauth::parse_form(body)))
        }),
        (Method::Post, "/oauth/token") => oauth_only(oauth, |o| {
            o.limited(Limit::Token, &caller(req))
                .unwrap_or_else(|| o.token(&oauth::parse_form(body), basic_auth(req)))
        }),
        (Method::Post, "/mcp") => {
            if let Some(v) = header(req, "MCP-Protocol-Version")
                && !mcp::PROTOCOL_VERSIONS.contains(&v.trim())
            {
                return Out::json(
                    400,
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("unsupported MCP-Protocol-Version {v}")}}),
                );
            }
            match session.handle_body(body) {
                Some(reply) => Out::json(200, &reply),
                None => Out::empty(202),
            }
        }
        (Method::Get | Method::Delete, "/mcp") => {
            let mut out = Out::error(405, "this server answers POST /mcp only");
            out.headers.push(("Allow", "POST".into()));
            out
        }
        (Method::Post, p) if p.starts_with("/v1/") => rest(&p[4..], query, body, session),
        _ => Out::error(404, "not found"),
    }
}

/// `POST /v1/<tool>`: JSON arguments in, the tool's JSON out (`?format=text`
/// for the agent-facing text).
fn rest(tool: &str, query: &str, body: &str, session: &mut Session) -> Out {
    let mut args = if body.trim().is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(body) {
            Ok(v @ Value::Object(_)) => v,
            _ => return Out::error(400, "the body must be a JSON object of tool arguments"),
        }
    };
    let text_out = oauth::parse_form(query).get("format").map(String::as_str) == Some("text");
    if !text_out {
        args["format"] = "json".into();
    }
    match session.call(tool, &args) {
        Ok(s) if text_out => Out::text(200, &s),
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => Out::json(200, &v),
            Err(_) => Out::json(200, &json!({"text": s})),
        },
        Err(err) => {
            let msg = format!("{err:#}");
            Out::error(if msg.starts_with("unknown tool") { 404 } else { 400 }, &msg)
        }
    }
}
