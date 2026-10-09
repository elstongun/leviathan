//! A minimal single-user OAuth 2.1 authorization server, so hosted apps
//! (claude.ai connectors, ChatGPT apps) can connect to `serve --http`.
//!
//! What MCP clients use and nothing more: authorization-server and
//! protected-resource metadata (RFC 8414, RFC 9728), dynamic client
//! registration (RFC 7591), authorization code with PKCE S256 only, and
//! rotating refresh tokens. Approving a connection takes a one-time code
//! that the server prints in its own log, so only whoever runs the server
//! can say yes. Tokens are stored as SHA-256 hashes, never in the clear.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 30 * 86_400;
const CODE_TTL: i64 = 300;
const APPROVAL_TTL: i64 = 600;
const MAX_CLIENTS: i64 = 200;
/// Registered clients that never got a token are dropped after this long
/// once the client table is full.
const UNUSED_CLIENT_TTL: i64 = 86_400;
const MAX_PENDING: usize = 50;
const MAX_PENDING_PER_CALLER: usize = 5;
pub const SCOPES: &[&str] = &["memory", "offline_access"];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS clients (
    client_id     TEXT PRIMARY KEY,
    secret_hash   TEXT,
    name          TEXT NOT NULL,
    redirect_uris TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS codes (
    code_hash    TEXT PRIMARY KEY,
    client_id    TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    challenge    TEXT NOT NULL,
    scope        TEXT NOT NULL,
    resource     TEXT,
    expires_at   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tokens (
    token_hash TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    client_id  TEXT NOT NULL,
    scope      TEXT NOT NULL,
    resource   TEXT,
    expires_at INTEGER NOT NULL
);
";

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn pkce_s256(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Equal-time comparison for secrets.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// An authorization request waiting for the approval code.
#[derive(Clone)]
struct Pending {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    challenge: String,
    scope: String,
    resource: Option<String>,
    state: Option<String>,
    approval: String,
    attempts: u32,
    expires_at: i64,
    caller: String,
}

/// Request budgets for the unauthenticated OAuth endpoints, per caller
/// (client address): requests per window of seconds.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Limit {
    Register,
    Authorize,
    Approve,
    Token,
}

impl Limit {
    fn budget(self) -> (u32, i64) {
        match self {
            Limit::Register => (10, 3600),
            Limit::Authorize => (30, 600),
            Limit::Approve => (30, 600),
            Limit::Token => (120, 600),
        }
    }
}

/// Window start and request count, per limit and caller.
type Windows = HashMap<(Limit, String), (i64, u32)>;

/// Shared across server threads: pending approvals and rate-limit windows
/// live in memory, the rest in the auth database (one connection per thread).
#[derive(Clone)]
pub struct Shared {
    pub issuer: String,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    windows: Arc<Mutex<Windows>>,
}

impl Shared {
    pub fn new(issuer: &str) -> Self {
        Self {
            issuer: issuer.trim_end_matches('/').to_string(),
            pending: Default::default(),
            windows: Default::default(),
        }
    }

    /// Count one request against `caller`'s budget; false once it is spent.
    pub fn allow(&self, limit: Limit, caller: &str) -> bool {
        let (max, window) = limit.budget();
        let now = unix_now();
        let Ok(mut map) = self.windows.lock() else { return false };
        if map.len() > 10_000 {
            map.retain(|(l, _), (start, _)| now - *start < l.budget().1);
        }
        let entry = map.entry((limit, caller.to_string())).or_insert((now, 0));
        if now - entry.0 >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= max
    }

    /// The approval code of a pending request: what the server log shows.
    pub fn approval_code(&self, request_id: &str) -> Option<String> {
        self.pending.lock().ok()?.get(request_id).map(|p| p.approval.clone())
    }
}

pub struct OAuth {
    conn: Connection,
    shared: Shared,
}

/// A response for the HTTP layer: status, content type, body, extra headers.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub headers: Vec<(&'static str, String)>,
}

impl Reply {
    pub fn json(status: u16, v: Value) -> Self {
        Reply {
            status,
            content_type: "application/json",
            body: v.to_string(),
            headers: vec![("Cache-Control", "no-store".into())],
        }
    }

    fn error(status: u16, code: &str, description: &str) -> Self {
        Self::json(status, json!({"error": code, "error_description": description}))
    }

    fn html(status: u16, body: String) -> Self {
        Reply {
            status,
            content_type: "text/html; charset=utf-8",
            body,
            headers: vec![
                ("Cache-Control", "no-store".into()),
                ("X-Frame-Options", "DENY".into()),
                // No form-action: browsers apply it to the redirect back to the client.
                ("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'".into()),
            ],
        }
    }

    fn redirect(location: String) -> Self {
        Reply {
            status: 302,
            content_type: "text/plain",
            body: String::new(),
            headers: vec![("Location", location)],
        }
    }
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `a=1&b=two` (query strings and form bodies).
pub fn parse_form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

fn with_params(base: &str, params: &[(&str, Option<&str>)]) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    let q: Vec<String> =
        params.iter().filter_map(|(k, v)| v.map(|v| format!("{k}={}", percent_encode(v)))).collect();
    format!("{base}{sep}{}", q.join("&"))
}

fn redirect_allowed(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return true;
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = host
            .rsplit_once(':')
            .map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host });
        return matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    }
    // Native apps use private schemes (cursor://, vscode://); never script or file URLs.
    match lower.split_once(':') {
        Some((scheme, _)) => {
            !matches!(scheme, "javascript" | "data" | "file" | "vbscript" | "about" | "blob")
                && scheme.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && scheme.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c))
        }
        None => false,
    }
}

impl OAuth {
    pub fn open(path: &Path, shared: Shared) -> Result<Self> {
        let fresh = !path.exists();
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        if fresh {
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, shared })
    }

    pub fn authorization_server_metadata(&self) -> Value {
        let i = &self.shared.issuer;
        json!({
            "issuer": i,
            "authorization_endpoint": format!("{i}/oauth/authorize"),
            "token_endpoint": format!("{i}/oauth/token"),
            "registration_endpoint": format!("{i}/oauth/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_basic", "client_secret_post"],
            "scopes_supported": SCOPES,
            "authorization_response_iss_parameter_supported": true,
            "service_documentation": "https://github.com/elstongun/leviathan/blob/main/docs/REMOTE.md"
        })
    }

    pub fn protected_resource_metadata(&self) -> Value {
        let i = &self.shared.issuer;
        json!({
            "resource": format!("{i}/mcp"),
            "authorization_servers": [i],
            "scopes_supported": ["memory"],
            "bearer_methods_supported": ["header"],
            "resource_name": "Leviathan"
        })
    }

    pub fn www_authenticate(&self) -> String {
        format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"memory\"",
            self.shared.issuer
        )
    }

    /// Is this an unexpired access token we issued?
    pub fn check_access(&self, token: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM tokens WHERE token_hash = ?1 AND kind = 'access' AND expires_at > ?2",
                params![sha256_hex(token), unix_now()],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn register(&self, body: &str) -> Reply {
        let Ok(req) = serde_json::from_str::<Value>(body) else {
            return Reply::error(400, "invalid_client_metadata", "body must be a JSON object");
        };
        let uris: Vec<String> = req
            .get("redirect_uris")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if uris.is_empty() || uris.len() > 10 {
            return Reply::error(400, "invalid_redirect_uri", "redirect_uris: one to ten URIs");
        }
        if let Some(bad) = uris.iter().find(|u| !redirect_allowed(u) || u.len() > 500) {
            return Reply::error(400, "invalid_redirect_uri", &format!("not allowed: {bad}"));
        }
        let count =
            || -> i64 { self.conn.query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0)).unwrap_or(0) };
        if count() >= MAX_CLIENTS {
            let _ = self.conn.execute(
                "DELETE FROM clients WHERE created_at < ?1 \
                 AND client_id NOT IN (SELECT client_id FROM tokens) AND client_id NOT IN (SELECT client_id FROM codes)",
                [unix_now() - UNUSED_CLIENT_TTL],
            );
        }
        if count() >= MAX_CLIENTS {
            return Reply::error(
                400,
                "invalid_client_metadata",
                "too many registered clients; restart with a fresh auth database",
            );
        }
        let method = req.get("token_endpoint_auth_method").and_then(Value::as_str).unwrap_or("none");
        let confidential = matches!(method, "client_secret_basic" | "client_secret_post");
        if !confidential && method != "none" {
            return Reply::error(
                400,
                "invalid_client_metadata",
                "token_endpoint_auth_method: none, client_secret_basic or client_secret_post",
            );
        }
        let name: String = req
            .get("client_name")
            .and_then(Value::as_str)
            .unwrap_or("MCP client")
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect();
        let client_id = format!("lvc_{}", random_token(16));
        let secret = confidential.then(|| random_token(32));
        let now = unix_now();
        if let Err(err) = self.conn.execute(
            "INSERT INTO clients (client_id, secret_hash, name, redirect_uris, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![client_id, secret.as_deref().map(sha256_hex), name, serde_json::to_string(&uris).unwrap_or_default(), now],
        ) {
            return Reply::error(500, "server_error", &err.to_string());
        }
        let mut out = json!({
            "client_id": client_id,
            "client_id_issued_at": now,
            "client_name": name,
            "redirect_uris": uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": method,
            "scope": SCOPES.join(" ")
        });
        if let Some(s) = secret {
            out["client_secret"] = s.into();
            out["client_secret_expires_at"] = 0.into();
        }
        eprintln!("[leviathan] oauth: registered client {name:?}");
        Reply::json(201, out)
    }

    fn client(&self, client_id: &str) -> Option<(Option<String>, String, Vec<String>)> {
        self.conn
            .query_row(
                "SELECT secret_hash, name, redirect_uris FROM clients WHERE client_id = ?1",
                [client_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()
            .ok()
            .flatten()
            .map(|(s, n, u)| (s, n, serde_json::from_str(&u).unwrap_or_default()))
    }

    fn page(&self, title: &str, inner: &str) -> String {
        format!(
            "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
             <title>{t}</title><style>body{{font:15px/1.5 ui-monospace,monospace;background:#0d1117;color:#e6edf3;\
             max-width:34em;margin:4em auto;padding:0 1em}}input{{font:inherit;padding:.4em;width:12em}}\
             button{{font:inherit;padding:.4em 1em;margin-right:.5em}}.muted{{color:#8b949e}}.err{{color:#f0705a}}</style>\
             </head><body><h2>{t}</h2>{inner}</body></html>",
            t = escape_html(title)
        )
    }

    /// A 429 once `caller` has spent its budget for `limit`.
    pub fn limited(&self, limit: Limit, caller: &str) -> Option<Reply> {
        if self.shared.allow(limit, caller) {
            return None;
        }
        let mut reply = if limit == Limit::Authorize {
            Reply::html(
                429,
                self.page("Too many requests", "<p>Too many requests; try again in a few minutes.</p>"),
            )
        } else {
            Reply::error(429, "slow_down", "too many requests; try again in a few minutes")
        };
        reply.headers.push(("Retry-After", limit.budget().1.to_string()));
        Some(reply)
    }

    /// GET /oauth/authorize: validate, then ask for the approval code.
    pub fn authorize_page(&self, query: &HashMap<String, String>, caller: &str) -> Reply {
        let get = |k: &str| query.get(k).map(String::as_str).filter(|v| !v.is_empty());
        let Some(client_id) = get("client_id") else {
            return Reply::html(400, self.page("Invalid request", "<p>Missing client_id.</p>"));
        };
        let Some((_, name, uris)) = self.client(client_id) else {
            return Reply::html(
                400,
                self.page("Unknown client", "<p>This client is not registered with this server.</p>"),
            );
        };
        let redirect_uri = match get("redirect_uri") {
            Some(r) if uris.iter().any(|u| u == r) => r.to_string(),
            None if uris.len() == 1 => uris[0].clone(),
            _ => {
                return Reply::html(
                    400,
                    self.page(
                        "Invalid redirect",
                        "<p>The redirect URI is not registered for this client.</p>",
                    ),
                );
            }
        };
        let state = get("state").map(str::to_string);
        let fail = |code: &str, msg: &str| {
            Reply::redirect(with_params(
                &redirect_uri,
                &[
                    ("error", Some(code)),
                    ("error_description", Some(msg)),
                    ("state", state.as_deref()),
                    ("iss", Some(&self.shared.issuer)),
                ],
            ))
        };
        if get("response_type") != Some("code") {
            return fail("unsupported_response_type", "only response_type=code");
        }
        let Some(challenge) = get("code_challenge").filter(|c| (43..=128).contains(&c.len())) else {
            return fail("invalid_request", "PKCE code_challenge is required");
        };
        if get("code_challenge_method") != Some("S256") {
            return fail("invalid_request", "code_challenge_method must be S256");
        }
        let scope = get("scope").unwrap_or("memory").to_string();
        if scope.split_whitespace().any(|s| !SCOPES.contains(&s)) {
            return fail("invalid_scope", "scopes: memory, offline_access");
        }
        let mut digits = [0u8; 4];
        let _ = getrandom::fill(&mut digits);
        let n = u32::from_le_bytes(digits) % 100_000_000;
        let approval = format!("{:04}-{:04}", n / 10_000, n % 10_000);
        let request_id = random_token(18);
        let pending = Pending {
            client_id: client_id.to_string(),
            client_name: name.clone(),
            redirect_uri,
            challenge: challenge.to_string(),
            scope,
            resource: get("resource").map(str::to_string),
            state,
            approval: approval.clone(),
            attempts: 0,
            expires_at: unix_now() + APPROVAL_TTL,
            caller: caller.to_string(),
        };
        {
            let mut map = self.shared.pending.lock().expect("pending lock");
            let now = unix_now();
            map.retain(|_, p| p.expires_at > now);
            let mine = map.values().filter(|p| p.caller == caller).count();
            if map.len() >= MAX_PENDING || mine >= MAX_PENDING_PER_CALLER {
                return Reply::html(
                    429,
                    self.page(
                        "Too many requests",
                        "<p>Too many pending approvals; try again in a few minutes.</p>",
                    ),
                );
            }
            map.insert(request_id.clone(), pending);
        }
        eprintln!(
            "[leviathan] oauth: {name:?} asks to connect. Approval code: {approval} (valid 10 minutes)"
        );
        Reply::html(200, self.approval_form(&request_id, &name, None))
    }

    fn approval_form(&self, request_id: &str, client: &str, error: Option<&str>) -> String {
        let err = error.map(|e| format!("<p class=err>{}</p>", escape_html(e))).unwrap_or_default();
        self.page(
            "Connect to Leviathan",
            &format!(
                "<p><b>{c}</b> wants to read and write memory on <span class=muted>{i}</span>.</p>\
                 <p>Enter the approval code printed in the Leviathan server's log.</p>{err}\
                 <form method=post action=\"/oauth/authorize\"><input type=hidden name=request value=\"{r}\">\
                 <p><input name=code autocomplete=off autofocus placeholder=\"0000-0000\" required></p>\
                 <p><button name=action value=approve>Approve</button><button name=action value=deny>Deny</button></p></form>",
                c = escape_html(client),
                i = escape_html(&self.shared.issuer),
                r = escape_html(request_id),
            ),
        )
    }

    /// POST /oauth/authorize: check the approval code, then redirect with a code.
    pub fn authorize_submit(&self, form: &HashMap<String, String>) -> Reply {
        let request_id = form.get("request").cloned().unwrap_or_default();
        let mut map = self.shared.pending.lock().expect("pending lock");
        let Some(p) = map.get_mut(&request_id).filter(|p| p.expires_at > unix_now()) else {
            map.remove(&request_id);
            return Reply::html(
                400,
                self.page("Expired", "<p>This request expired. Start the connection again from the app.</p>"),
            );
        };
        let iss = self.shared.issuer.clone();
        if form.get("action").map(String::as_str) == Some("deny") {
            let p = map.remove(&request_id).expect("present");
            return Reply::redirect(with_params(
                &p.redirect_uri,
                &[("error", Some("access_denied")), ("state", p.state.as_deref()), ("iss", Some(&iss))],
            ));
        }
        let entered: String =
            form.get("code").map(|c| c.chars().filter(char::is_ascii_digit).collect()).unwrap_or_default();
        let expected: String = p.approval.chars().filter(char::is_ascii_digit).collect();
        if !same(&entered, &expected) {
            p.attempts += 1;
            if p.attempts >= 5 {
                let name = p.client_name.clone();
                map.remove(&request_id);
                eprintln!("[leviathan] oauth: too many wrong approval codes for {name:?}; request cancelled");
                return Reply::html(
                    403,
                    self.page("Locked", "<p>Too many wrong codes. Start again from the app.</p>"),
                );
            }
            let (client, left) = (p.client_name.clone(), 5 - p.attempts);
            return Reply::html(
                200,
                self.approval_form(&request_id, &client, Some(&format!("Wrong code; {left} tries left."))),
            );
        }
        let p = map.remove(&request_id).expect("present");
        drop(map);
        let code = random_token(32);
        if let Err(err) = self.conn.execute(
            "INSERT INTO codes (code_hash, client_id, redirect_uri, challenge, scope, resource, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                sha256_hex(&code),
                p.client_id,
                p.redirect_uri,
                p.challenge,
                p.scope,
                p.resource,
                unix_now() + CODE_TTL
            ],
        ) {
            return Reply::html(500, self.page("Error", &escape_html(&err.to_string())));
        }
        eprintln!("[leviathan] oauth: approved {:?}", p.client_name);
        Reply::redirect(with_params(
            &p.redirect_uri,
            &[("code", Some(&code)), ("state", p.state.as_deref()), ("iss", Some(&iss))],
        ))
    }

    /// POST /oauth/token.
    pub fn token(&self, form: &HashMap<String, String>, basic: Option<(String, String)>) -> Reply {
        let get = |k: &str| form.get(k).map(String::as_str).filter(|v| !v.is_empty());
        let (client_id, secret) = match &basic {
            Some((id, s)) => (Some(id.as_str()), Some(s.as_str())),
            None => (get("client_id"), get("client_secret")),
        };
        let Some(client_id) = client_id else {
            return Reply::error(401, "invalid_client", "client_id is required");
        };
        let Some((secret_hash, _, _)) = self.client(client_id) else {
            return Reply::error(401, "invalid_client", "unknown client");
        };
        if let Some(hash) = secret_hash
            && !secret.is_some_and(|s| same(&sha256_hex(s), &hash))
        {
            return Reply::error(401, "invalid_client", "client authentication failed");
        }
        let now = unix_now();
        let (scope, resource) = match get("grant_type") {
            Some("authorization_code") => {
                let (Some(code), Some(verifier)) = (get("code"), get("code_verifier")) else {
                    return Reply::error(400, "invalid_request", "code and code_verifier are required");
                };
                let hash = sha256_hex(code);
                let row: Option<(String, String, String, String, Option<String>, i64)> = self
                    .conn
                    .query_row(
                        "SELECT client_id, redirect_uri, challenge, scope, resource, expires_at FROM codes WHERE code_hash = ?1",
                        [&hash],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let _ = self.conn.execute(
                    "DELETE FROM codes WHERE code_hash = ?1 OR expires_at <= ?2",
                    params![hash, now],
                );
                let Some((cid, redirect, challenge, scope, resource, expires)) = row else {
                    return Reply::error(400, "invalid_grant", "unknown or used code");
                };
                if cid != client_id || expires <= now {
                    return Reply::error(400, "invalid_grant", "code expired or issued to another client");
                }
                if get("redirect_uri").is_some_and(|r| r != redirect) {
                    return Reply::error(400, "invalid_grant", "redirect_uri does not match");
                }
                if !same(&pkce_s256(verifier), &challenge) {
                    return Reply::error(400, "invalid_grant", "PKCE verification failed");
                }
                (scope, resource)
            }
            Some("refresh_token") => {
                let Some(refresh) = get("refresh_token") else {
                    return Reply::error(400, "invalid_request", "refresh_token is required");
                };
                let hash = sha256_hex(refresh);
                let row: Option<(String, String, Option<String>, i64)> = self
                    .conn
                    .query_row(
                        "SELECT client_id, scope, resource, expires_at FROM tokens WHERE token_hash = ?1 AND kind = 'refresh'",
                        [&hash],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let _ = self.conn.execute(
                    "DELETE FROM tokens WHERE token_hash = ?1 OR expires_at <= ?2",
                    params![hash, now],
                );
                match row {
                    Some((cid, scope, resource, expires)) if cid == client_id && expires > now => {
                        (scope, resource)
                    }
                    _ => return Reply::error(400, "invalid_grant", "unknown, used or expired refresh token"),
                }
            }
            _ => return Reply::error(400, "unsupported_grant_type", "authorization_code or refresh_token"),
        };
        let access = random_token(32);
        let refresh = random_token(32);
        for (token, kind, ttl) in [(&access, "access", ACCESS_TTL), (&refresh, "refresh", REFRESH_TTL)] {
            if let Err(err) = self.conn.execute(
                "INSERT INTO tokens (token_hash, kind, client_id, scope, resource, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![sha256_hex(token), kind, client_id, scope, resource, now + ttl],
            ) {
                return Reply::error(500, "server_error", &err.to_string());
            }
        }
        Reply::json(
            200,
            json!({"access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL, "refresh_token": refresh, "scope": scope}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(percent_decode("a%20b+c%2F"), "a b c/");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_allowed("http://localhost:3333/cb") && redirect_allowed("http://127.0.0.1/cb"));
        assert!(redirect_allowed("cursor://anysphere.cursor-retrieval/oauth/callback"));
        assert!(!redirect_allowed("http://evil.example/cb") && !redirect_allowed("javascript:alert(1)"));
        assert!(!redirect_allowed("http://localhost.evil.example/cb"));
    }
}
