#![cfg(feature = "remote")]
//! `serve --http` end to end: token auth, MCP, REST, OAuth 2.1 with PKCE,
//! and the client helpers.

use std::path::Path;

use base64::Engine;
use leviathan::card::CardOptions;
use leviathan::http::{self, AuthMode, HttpOptions, Running};
use leviathan::mcp::{MemoryOptions, MemoryTools, ServerOptions};
use leviathan::memory::Settings;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn start(dir: &Path, auth: AuthMode, tools: MemoryTools) -> Running {
    http::start(HttpOptions {
        server: ServerOptions {
            index: dir.join("no-index.db"),
            card: CardOptions::default(),
            memory: Some(MemoryOptions { path: dir.join("memory.db"), settings: Settings::default(), tools }),
        },
        addr: "127.0.0.1:0".into(),
        auth,
        token_file: dir.join("leviathan.token"),
        public_url: None,
        allow_origins: vec!["https://app.example".into()],
        threads: 2,
    })
    .unwrap()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into()
}

fn post(url: &str, token: Option<&str>, body: &str) -> (u16, String, Option<String>) {
    let mut rb = agent().post(url).header("Content-Type", "application/json");
    if let Some(t) = token {
        rb = rb.header("Authorization", format!("Bearer {t}"));
    }
    let mut resp = rb.send(body).unwrap();
    let www = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).map(str::to_string);
    (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap(), www)
}

fn get(url: &str) -> (u16, String) {
    let mut resp = agent().get(url).call().unwrap();
    (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap())
}

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("leviathan.token")).unwrap().trim().to_string()
}

#[test]
fn token_auth_guards_mcp_and_rest() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), AuthMode::Token, MemoryTools::All);
    let base = format!("http://{}", server.addr);
    let t = token(dir.path());
    assert!(t.starts_with("lvt_") && t.len() > 40);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("leviathan.token")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let (status, _, www) = post(&server.url, None, list);
    assert_eq!(status, 401);
    assert!(www.unwrap().starts_with("Bearer"));
    assert_eq!(post(&server.url, Some("lvt_wrong-token-of-some-length"), list).0, 401);

    let (status, body, _) = post(&server.url, Some(&t), list);
    assert_eq!(status, 200);
    let names: Vec<String> = serde_json::from_str::<Value>(&body).unwrap()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["recall", "remember", "forget", "memory_describe"]);

    let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    assert_eq!(post(&server.url, Some(&t), note).0, 202);
    assert_eq!(get(&server.url).0, 401);

    // REST: JSON in, JSON out; refused writes are 400s.
    let (status, body, _) = post(
        &format!("{base}/v1/remember"),
        Some(&t),
        r#"{"text":"Builds use the nightly toolchain","subject":"build","key":"toolchain"}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["status"], "created");
    let (status, body, _) =
        post(&format!("{base}/v1/remember"), Some(&t), r#"{"text":"password = hunter2hunter2"}"#);
    assert_eq!(status, 400);
    assert!(body.contains("credential"), "{body}");
    assert_eq!(post(&format!("{base}/v1/nope"), Some(&t), "{}").0, 404);
    assert_eq!(post(&format!("{base}/v1/recall"), Some(&t), "[1]").0, 400);

    // The client helpers: a remote tool call returns the agent-facing text.
    let text = http::call_tool(&server.url, Some(&t), "recall", &json!({"query": "toolchain"})).unwrap();
    assert!(text.contains("Builds use the nightly toolchain"), "{text}");
    let err = http::call_tool(&server.url, Some("lvt_wrong-token-of-some-length"), "recall", &json!({}))
        .unwrap_err();
    assert!(err.to_string().contains("401"));

    // Unauthenticated endpoints reveal nothing about the data.
    let (status, body) = get(&format!("{base}/openapi.json"));
    assert_eq!(status, 200);
    let spec: Value = serde_json::from_str(&body).unwrap();
    assert!(spec["paths"]["/v1/remember"]["post"]["operationId"] == "remember");
    assert!(!body.contains("nightly"));
    assert_eq!(get(&format!("{base}/healthz")).0, 200);
    assert_eq!(get(&format!("{base}/.well-known/oauth-protected-resource")).0, 404);

    // Browser origins: loopback and the allow list pass, others are refused.
    let origin = |o: &str| {
        agent()
            .post(&server.url)
            .header("Origin", o)
            .header("Authorization", format!("Bearer {t}"))
            .send(list)
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(origin("https://evil.example"), 403);
    assert_eq!(origin("https://app.example"), 200);
    assert_eq!(origin("http://localhost:6274"), 200);
    server.stop();
}

#[test]
fn recall_only_servers_refuse_writes() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), AuthMode::Token, MemoryTools::Recall);
    let t = token(dir.path());
    let err = http::call_tool(&server.url, Some(&t), "remember", &json!({"text": "x is y"})).unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    server.stop();
}

#[test]
fn no_auth_is_loopback_only() {
    let dir = tempfile::tempdir().unwrap();
    let err = http::start(HttpOptions {
        server: ServerOptions { index: dir.path().join("x.db"), card: CardOptions::default(), memory: None },
        addr: "0.0.0.0:0".into(),
        auth: AuthMode::None,
        token_file: dir.path().join("t"),
        public_url: None,
        allow_origins: vec![],
        threads: 1,
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("loopback"));
}

fn form(url: &str, body: &str, basic: Option<(&str, &str)>) -> (u16, String, Option<String>) {
    let mut rb = agent().post(url).header("Content-Type", "application/x-www-form-urlencoded");
    if let Some((id, secret)) = basic {
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        rb = rb.header("Authorization", format!("Basic {creds}"));
    }
    let mut resp = rb.send(body).unwrap();
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
    (resp.status().as_u16(), resp.body_mut().read_to_string().unwrap(), location)
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|p| p.strip_prefix(&format!("{key}="))).map(leviathan::oauth::percent_decode)
}

#[test]
fn oauth_authorization_code_with_pkce_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), AuthMode::OAuth, MemoryTools::All);
    let base = format!("http://{}", server.addr);
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    // Discovery: the 401 points at the resource metadata, which points at us.
    let (status, _, www) = post(&server.url, None, list);
    assert_eq!(status, 401);
    assert!(
        www.unwrap().contains(&format!("resource_metadata=\"{base}/.well-known/oauth-protected-resource\""))
    );
    let prm: Value =
        serde_json::from_str(&get(&format!("{base}/.well-known/oauth-protected-resource/mcp")).1).unwrap();
    assert_eq!(prm["resource"], format!("{base}/mcp"));
    let asm: Value =
        serde_json::from_str(&get(&format!("{base}/.well-known/oauth-authorization-server")).1).unwrap();
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));

    // Dynamic client registration.
    let redirect = "https://claude.ai/api/mcp/auth_callback";
    let (status, body, _) = post(
        asm["registration_endpoint"].as_str().unwrap(),
        None,
        &json!({"client_name": "Test client", "redirect_uris": [redirect], "token_endpoint_auth_method": "none"}).to_string(),
    );
    assert_eq!(status, 201, "{body}");
    let client_id = serde_json::from_str::<Value>(&body).unwrap()["client_id"].as_str().unwrap().to_string();
    let (status, _, _) = post(
        &format!("{base}/oauth/register"),
        None,
        &json!({"redirect_uris": ["http://evil.example/cb"]}).to_string(),
    );
    assert_eq!(status, 400);

    // Authorize with PKCE: the page asks for the code printed in the server log.
    let verifier = "a-very-long-pkce-verifier-string-with-enough-entropy-0123456789";
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let enc = leviathan::oauth::percent_encode;
    let authorize = format!(
        "{base}/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={challenge}\
         &code_challenge_method=S256&state=xyz&scope=memory",
        enc(redirect)
    );
    let (status, page) = get(&authorize);
    assert_eq!(status, 200);
    let request_id =
        page.split("name=request value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let code = server.approval_code(&request_id).unwrap();

    let (status, page, _) = form(
        &format!("{base}/oauth/authorize"),
        &format!("request={request_id}&code=0000-0000&action=approve"),
        None,
    );
    assert_eq!(status, 200);
    assert!(page.contains("Wrong code"));
    let (status, _, location) = form(
        &format!("{base}/oauth/authorize"),
        &format!("request={request_id}&code={code}&action=approve"),
        None,
    );
    assert_eq!(status, 302);
    let location = location.unwrap();
    assert!(location.starts_with(redirect));
    assert_eq!(query_param(&location, "state").as_deref(), Some("xyz"));
    let auth_code = query_param(&location, "code").unwrap();

    // Exchange: a wrong verifier fails and burns the code.
    let token_url = format!("{base}/oauth/token");
    let exchange = |verifier: &str, code: &str| {
        form(
            &token_url,
            &format!(
                "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={verifier}&client_id={client_id}",
                enc(code),
                enc(redirect)
            ),
            None,
        )
    };
    let (status, body, _) = exchange(verifier, &auth_code);
    assert_eq!(status, 200, "{body}");
    let tokens: Value = serde_json::from_str(&body).unwrap();
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(exchange(verifier, &auth_code).0, 400, "codes are single-use");

    assert_eq!(post(&server.url, Some(&access), list).0, 200);
    // The static token still works alongside OAuth.
    assert_eq!(post(&server.url, Some(&token(dir.path())), list).0, 200);

    // Refresh rotates: the old refresh token stops working.
    let refresh_with = |r: &str| {
        form(
            &token_url,
            &format!("grant_type=refresh_token&refresh_token={}&client_id={client_id}", enc(r)),
            None,
        )
    };
    let (status, body, _) = refresh_with(&refresh);
    assert_eq!(status, 200, "{body}");
    let rotated: Value = serde_json::from_str(&body).unwrap();
    assert_ne!(rotated["refresh_token"], tokens["refresh_token"]);
    assert_eq!(refresh_with(&refresh).0, 400);
    assert_eq!(post(&server.url, Some(rotated["access_token"].as_str().unwrap()), list).0, 200);

    // A wrong PKCE verifier is refused.
    let (_, page) = get(&authorize);
    let request_id =
        page.split("name=request value=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    let code = server.approval_code(&request_id).unwrap();
    let (_, _, location) = form(
        &format!("{base}/oauth/authorize"),
        &format!("request={request_id}&code={code}&action=approve"),
        None,
    );
    let auth_code = query_param(&location.unwrap(), "code").unwrap();
    let (status, body, _) = exchange("not-the-verifier-not-the-verifier-not-the-verifier", &auth_code);
    assert_eq!(status, 400);
    assert!(body.contains("PKCE"));

    // Browsers apply form-action to the redirect back to the client.
    let resp = agent().get(&authorize).call().unwrap();
    let csp = resp.headers().get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && !csp.contains("form-action"), "{csp}");

    // The auth database never holds a token in the clear.
    let db = std::fs::read(dir.path().join("leviathan-auth.db")).unwrap();
    let wal = std::fs::read(dir.path().join("leviathan-auth.db-wal")).unwrap_or_default();
    for t in [&access, &refresh] {
        let needle = t.as_bytes();
        assert!(!db.windows(needle.len()).any(|w| w == needle));
        assert!(!wal.windows(needle.len()).any(|w| w == needle));
    }
    server.stop();
}

#[test]
fn oauth_endpoints_are_rate_limited_per_caller() {
    let dir = tempfile::tempdir().unwrap();
    let server = start(dir.path(), AuthMode::OAuth, MemoryTools::All);
    let base = format!("http://{}", server.addr);
    let redirect = "https://claude.ai/api/mcp/auth_callback";
    let register =
        || post(&format!("{base}/oauth/register"), None, &json!({"redirect_uris": [redirect]}).to_string());
    let (status, body, _) = register();
    assert_eq!(status, 201);
    let client_id = serde_json::from_str::<Value>(&body).unwrap()["client_id"].as_str().unwrap().to_string();
    let authorize = format!(
        "{base}/oauth/authorize?response_type=code&client_id={client_id}&code_challenge={}&code_challenge_method=S256",
        "a".repeat(43)
    );
    let from = |ip: Option<&str>| {
        let mut rb = agent().get(&authorize);
        if let Some(ip) = ip {
            rb = rb.header("X-Forwarded-For", format!("198.51.100.1, {ip}"));
        }
        rb.call().unwrap().status().as_u16()
    };

    // One caller can hold a few pending approvals, not the whole table.
    for _ in 0..5 {
        assert_eq!(from(None), 200);
    }
    assert_eq!(from(None), 429);
    // A proxy on this machine names the caller in X-Forwarded-For (last hop).
    assert_eq!(from(Some("203.0.113.9")), 200);

    // Registration has its own budget.
    for _ in 0..9 {
        assert_eq!(register().0, 201);
    }
    assert_eq!(register().0, 429);
    server.stop();
}
