use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use leviathan::card::CardOptions;
use leviathan::config::{Config, Format};
use leviathan::index::{self, BuildOptions};
use leviathan::query::{Scope, SearchOutcome, SearchRequest, Sort, Status, Store};
use leviathan::source;
use serde_json::{Value, json};

fn example(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/tickets").join(file)
}

fn quiet() -> BuildOptions {
    BuildOptions { quiet: true, ..Default::default() }
}

fn build_tickets(dir: &Path) -> PathBuf {
    let cfg = Config::load(&example("leviathan.toml")).unwrap();
    let paths: Vec<PathBuf> = cfg.source.paths.iter().map(PathBuf::from).collect();
    let sources = source::discover(&paths, cfg.source.format).unwrap();
    let index = dir.join("tickets.db");
    let report = index::build(&index, &sources, &cfg, false, quiet()).unwrap();
    assert!(report.built);
    assert_eq!((report.record_count, report.group_count, report.skipped_lines), (24, 4, 0));
    index
}

fn open(index: &Path) -> Store {
    Store::open(index, CardOptions::default()).unwrap()
}

fn req(group: Option<&str>, query: &str) -> SearchRequest {
    SearchRequest {
        group: group.map(str::to_string),
        query: query.into(),
        limit: 5,
        fallback: true,
        ..Default::default()
    }
}

fn ids(out: &SearchOutcome) -> Vec<&str> {
    out.results.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn group_scoped_search_ranks_the_right_record_first() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let out = store.search(&req(Some("Acme Corp"), "login loops after password reset")).unwrap();
    assert_eq!(out.status, Status::Ok);
    assert_eq!(out.group.as_ref().unwrap().key, "C-ACME");
    assert_eq!(ids(&out)[0], "T-1001");
    assert!(out.results.iter().all(|c| c.group.as_deref() == Some("C-ACME")));
    let card = &out.results[0];
    assert_eq!(card.title.as_deref(), Some("Login loops back to sign-in page after password reset"));
    assert_eq!(card.fields["status"], "closed");
}

#[test]
fn groups_resolve_by_key_name_or_typo_and_ambiguity_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    assert_eq!(store.resolve("c-globex", 5).unwrap()[0].key, "C-GLOBEX");
    let fuzzy = store.resolve("umbrela", 5).unwrap();
    assert_eq!((fuzzy.len(), fuzzy[0].key.as_str(), fuzzy[0].match_type), (1, "C-UMBRELLA", "fuzzy"));
    let out = store.search(&req(Some("C-"), "reset")).unwrap();
    assert_eq!(out.status, Status::AmbiguousGroup);
    assert_eq!(out.candidates.len(), 4);
    assert!(out.results.is_empty());
    assert_eq!(store.search(&req(Some("Wayne Enterprises"), "reset")).unwrap().status, Status::UnknownGroup);
}

#[test]
fn filters_dates_phrases_and_exclusions_narrow_results() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));

    let mut r = req(None, "sso");
    r.filters = vec![("tags[]".into(), "SSO".into())];
    r.since = Some("2024-03".into());
    r.until = Some("2024-10".into());
    let out = store.search(&r).unwrap();
    let mut got = ids(&out);
    got.sort();
    assert_eq!(got, ["T-1006", "T-1020"], "until is inclusive of the whole month");

    let mut r = req(None, "");
    r.filters = vec![
        ("status".into(), "open".into()),
        ("priority".into(), "high".into()),
        ("priority".into(), "normal".into()),
    ];
    let out = store.search(&r).unwrap();
    let mut got = ids(&out);
    got.sort();
    assert_eq!(got, ["T-1019", "T-1022"], "same field ORs, different fields AND");
    assert_eq!(out.total_matches, 2);

    let phrase = store.search(&req(None, "\"leading zeros\"")).unwrap();
    assert_eq!(ids(&phrase), ["T-1018"]);
    let without = store.search(&req(None, "login -sso")).unwrap();
    assert!(!without.results.is_empty());
    assert!(without.results.iter().all(|c| c.id != "T-1001" && c.id != "T-1006"));

    let mut bad = req(None, "x");
    bad.filters = vec![("nope".into(), "1".into())];
    assert_eq!(store.search(&bad).unwrap().status, Status::BadRequest);
    let mut bad = req(None, "x");
    bad.since = Some("last tuesday".into());
    assert_eq!(store.search(&bad).unwrap().status, Status::BadRequest);
}

#[test]
fn placeholders_are_ignored_and_boosts_prefer_resolved_records() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let out = store.search(&req(None, "api 500 filter")).unwrap();
    let card = out.results.iter().find(|c| c.id == "T-1019").unwrap();
    assert!(!card.fields.contains_key("resolution"), "\"see notes\" is configured as empty");
}

#[test]
fn empty_group_falls_back_to_labeled_other_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let out = store.search(&req(Some("globex"), "password")).unwrap();
    assert_eq!((out.status, out.total_matches), (Status::Ok, 0));
    assert!(!out.other_groups.is_empty());
    assert!(out.other_groups.iter().all(|c| c.other_group && c.group.as_deref() != Some("C-GLOBEX")));

    let mut no_fallback = req(Some("globex"), "password");
    no_fallback.fallback = false;
    assert!(store.search(&no_fallback).unwrap().other_groups.is_empty());

    let mut others = req(Some("globex"), "login");
    others.scope = Some(Scope::Others);
    let out = store.search(&others).unwrap();
    assert!(!out.results.is_empty() && out.results.iter().all(|c| c.group.as_deref() != Some("C-GLOBEX")));
}

#[test]
fn browse_lists_newest_first_with_paging() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let mut r = req(Some("initech"), "");
    r.sort = Sort::Newest;
    r.limit = 2;
    let page1 = store.search(&r).unwrap();
    assert_eq!((ids(&page1), page1.total_matches), (vec!["T-1024", "T-1020"], 6));
    r.offset = 2;
    assert_eq!(ids(&store.search(&r).unwrap()), ["T-1016", "T-1012"]);

    let mut newest = req(None, "login");
    newest.sort = Sort::Newest;
    let dates: Vec<String> =
        store.search(&newest).unwrap().results.iter().filter_map(|c| c.date.clone()).collect();
    assert!(dates.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn describe_reports_fields_filters_and_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let d = open(&build_tickets(tmp.path())).describe(3).unwrap();
    assert_eq!((d.record_count, d.group_count), (24, 4));
    assert_eq!(d.about.record, "ticket");
    assert_eq!(d.date_min.as_deref().map(|s| &s[..10]), Some("2024-01-08"));
    let status = d.filters.iter().find(|f| f.field == "status").unwrap();
    assert_eq!(status.top, [("closed".to_string(), 21), ("open".to_string(), 3)]);
    let text = leviathan::render::describe(&d);
    assert!(text.contains("24 tickets · 4 customers"));
}

#[test]
fn upsert_and_delete_keep_counts_and_facets_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_tickets(tmp.path());
    let delta = tmp.path().join("delta.jsonl");
    let changed = json!({"ticket_id": "T-1001", "subject": "Login loop fixed by clearing cookies", "customer": {"id": "C-ACME", "name": "Acme Corp"},
        "status": "open", "priority": "high", "created_at": "2024-01-08T09:12:00Z", "body": "Reopened: the cookie fix regressed."});
    let added = json!({"ticket_id": "T-2000", "subject": "Data residency question", "customer": {"id": "C-WAYNE", "name": "Wayne Enterprises"},
        "status": "open", "priority": "low", "created_at": "2025-01-02T10:00:00Z", "body": "Where is our data stored?"});
    std::fs::write(&delta, format!("{changed}\n{added}\n")).unwrap();
    let sources = source::discover(&[delta], Format::Auto).unwrap();
    let report = index::upsert(&index, &sources, quiet()).unwrap();
    assert_eq!((report.inserted, report.updated, report.record_count, report.group_count), (1, 1, 25, 5));

    let store = open(&index);
    assert!(
        store
            .search(&req(Some("acme"), "endless redirect chrome edge"))
            .unwrap()
            .results
            .iter()
            .all(|c| c.id != "T-1001")
    );
    assert_eq!(ids(&store.search(&req(Some("acme"), "regressed")).unwrap()), ["T-1001"]);
    let open_count = |s: &Store| {
        s.describe(5)
            .unwrap()
            .filters
            .into_iter()
            .find(|f| f.field == "status")
            .unwrap()
            .top
            .into_iter()
            .find(|(v, _)| v == "open")
            .unwrap()
            .1
    };
    assert_eq!(open_count(&store), 5);
    drop(store);

    let report = index::delete(&index, &["T-2000".into(), "missing".into()]).unwrap();
    assert_eq!((report.deleted, report.record_count, report.group_count), (1, 24, 4));
    assert_eq!(open_count(&open(&index)), 4);
}

#[test]
fn unchanged_sources_skip_and_a_mapping_change_rebuilds() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_tickets(tmp.path());
    let mut cfg = Config::load(&example("leviathan.toml")).unwrap();
    let sources = source::discover(&[example("tickets.jsonl")], Format::Auto).unwrap();
    assert!(!index::build(&index, &sources, &cfg, false, quiet()).unwrap().built);
    cfg.fields.filters.push("comments[].author".into());
    assert!(index::build(&index, &sources, &cfg, false, quiet()).unwrap().built);
}

#[test]
fn csv_sqlite_and_gzip_sources_index_the_same_way() {
    use flate2::{Compression, write::GzEncoder};
    let tmp = tempfile::tempdir().unwrap();
    let csv = tmp.path().join("orders.csv");
    std::fs::write(
        &csv,
        "Order ID,Customer,Status,Placed,Notes\n\
         O1,Acme,shipped,3/4/2024 2:15 PM,\"Box arrived crushed, replacement sent\"\n\
         O2,Acme,new,2024/03/09,Customer asked for gift wrap\n\
         O3,Globex,shipped,1712000000,Courier left the parcel at the wrong address\n",
    )
    .unwrap();
    let cfg: Config = toml::from_str(
        "[fields]\nid = \"Order ID\"\ngroup = \"Customer\"\ndate = \"Placed\"\nfilters = [\"Status\"]\ntext = [\"Notes\"]\n",
    )
    .unwrap();
    let build = |path: &Path, cfg: &Config| {
        let sources = source::discover(&[path.to_path_buf()], cfg.source.format).unwrap();
        let index = path.with_extension("db");
        index::build(&index, &sources, cfg, false, quiet()).unwrap();
        open(&index)
    };
    let store = build(&csv, &cfg);
    let out = store.search(&req(Some("acme"), "crushed box")).unwrap();
    assert_eq!(ids(&out), ["O1"]);
    assert_eq!(out.results[0].date.as_deref(), Some("2024-03-04 14:15"));
    let mut r = req(None, "");
    r.since = Some("2024-04".into());
    assert_eq!(ids(&store.search(&r).unwrap()), ["O3"], "epoch seconds are dates");

    let gz = tmp.path().join("tickets.jsonl.gz");
    let mut enc = GzEncoder::new(std::fs::File::create(&gz).unwrap(), Compression::fast());
    enc.write_all(&std::fs::read(example("tickets.jsonl")).unwrap()).unwrap();
    enc.finish().unwrap();
    let mut tcfg = Config::load(&example("leviathan.toml")).unwrap();
    tcfg.source.paths.clear();
    assert_eq!(build(&gz, &tcfg).describe(1).unwrap().record_count, 24);

    let db = tmp.path().join("app.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, host TEXT, level TEXT, at TEXT, msg TEXT, ctx TEXT);
             INSERT INTO events VALUES (1, 'web-1', 'error', '2024-05-01T10:00:00Z', 'disk full on /var', '{\"mount\": \"/var\"}');
             INSERT INTO events VALUES (2, 'web-2', 'warn', '2024-05-02T10:00:00Z', 'slow query detected', '{\"ms\": 1200}');
             INSERT INTO events VALUES (3, 'db-1', 'error', '2024-05-03T10:00:00Z', 'replication lag high', '{}');",
        )
        .unwrap();
    }
    let mut scfg: Config = toml::from_str(
        "[fields]\nid = \"id\"\ngroup = \"host\"\ndate = \"at\"\nfilters = [\"level\"]\ntext = [\"msg\", \"ctx.mount\"]\n",
    )
    .unwrap();
    scfg.source.sql = Some("SELECT * FROM events WHERE level = 'error'".into());
    let store = build(&db, &scfg);
    assert_eq!(store.describe(1).unwrap().record_count, 2, "--sql selects rows");
    assert_eq!(
        ids(&store.search(&req(None, "var")).unwrap()),
        ["1"],
        "JSON text cells are searchable by path"
    );
}

#[test]
fn bad_records_are_counted_or_fatal_in_strict_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("mixed.jsonl");
    std::fs::write(&src, "{\"id\":\"1\",\"t\":\"ok\"}\nnot json\n{\"t\":\"no id\"}\n").unwrap();
    let cfg: Config = toml::from_str("[fields]\nid = \"id\"\n").unwrap();
    let sources = source::discover(&[src], Format::Auto).unwrap();
    let report = index::build(&tmp.path().join("a.db"), &sources, &cfg, false, quiet()).unwrap();
    assert_eq!((report.record_count, report.skipped_lines), (1, 2));
    let strict = index::build(
        &tmp.path().join("b.db"),
        &sources,
        &cfg,
        false,
        BuildOptions { strict: true, ..quiet() },
    );
    assert!(strict.is_err());
    assert!(!tmp.path().join("b.db").exists(), "a failed build leaves no index behind");
}

fn cli(index: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .env_remove("LEVIATHAN_INDEX")
        .env_remove("LEVIATHAN_CONFIG")
        .arg("--index")
        .arg(index)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_infers_a_mapping_and_uses_documented_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("cli.db");
    let data = example("tickets.jsonl");
    let built = cli(&index, &["index", "-q", data.to_str().unwrap()]);
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    assert!(String::from_utf8_lossy(&built.stdout).contains("mapping inferred"));

    let ok = cli(&index, &["search", "-g", "acme", "login", "loop"]);
    assert!(ok.status.success());
    assert!(String::from_utf8_lossy(&ok.stdout).contains("[1] T-1001"));
    assert_eq!(cli(&index, &["search", "-g", "C-", "reset"]).status.code(), Some(3));
    assert_eq!(cli(&index, &["search", "x", "--where", "nope=1"]).status.code(), Some(2));
    let json_out = cli(&index, &["--json", "search", "leading", "zeros", "--where", "status=closed"]);
    let parsed: Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(parsed["results"][0]["id"], "T-1018");
    let recent = cli(&index, &["recent", "-g", "initech", "-n", "1"]);
    assert!(String::from_utf8_lossy(&recent.stdout).contains("T-1024"));

    let toml_path = tmp.path().join("leviathan.toml");
    let init = cli(&index, &["init", data.to_str().unwrap(), "-o", toml_path.to_str().unwrap()]);
    assert!(init.status.success());
    let proposed = Config::load(&toml_path).unwrap();
    assert_eq!(proposed.fields.id.as_deref(), Some("ticket_id"));
    assert_eq!(
        cli(&index, &["init", data.to_str().unwrap(), "-o", toml_path.to_str().unwrap()]).status.code(),
        Some(1)
    );
}

#[test]
fn mcp_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_tickets(tmp.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .arg("--index")
        .arg(&index)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |msg: Value| -> Option<Value> {
        writeln!(stdin, "{msg}").unwrap();
        msg.get("id")?;
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        Some(serde_json::from_str(&line).unwrap())
    };

    let init = rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}))
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("24 tickets across 4 customers"));
    rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let tools = rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
    let names: Vec<&str> =
        tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["search", "resolve_group", "get", "describe", "aggregate"]);

    let counted = rpc(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "aggregate", "arguments": {"by": "group", "format": "json"}}}))
    .unwrap();
    let counted: Value = serde_json::from_str(counted["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((counted["records"].as_i64(), counted["buckets"].as_i64()), (Some(24), Some(4)));

    let hit = rpc(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "search", "arguments": {"group": "Acme", "query": "sso", "where": {"priority": ["urgent", "high"]}}}}))
    .unwrap();
    assert_eq!(hit["result"]["isError"], false);
    assert!(hit["result"]["content"][0]["text"].as_str().unwrap().contains("[1] T-1006"));

    let bad = rpc(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "get", "arguments": {"id": "nope"}}}))
    .unwrap();
    assert_eq!(bad["result"]["isError"], true);

    let unknown = rpc(json!({"jsonrpc": "2.0", "id": 5, "method": "does/not/exist"})).unwrap();
    assert_eq!(unknown["error"]["code"], -32601);

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

// ---- aggregate ----------------------------------------------------------------

use leviathan::query::{AggregateRequest, Metric};

fn agg(by: Option<&str>) -> AggregateRequest {
    AggregateRequest { by: by.map(str::to_string), limit: 50, ..Default::default() }
}

#[test]
fn aggregate_counts_every_record_by_group_and_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let all = store.aggregate(&agg(None)).unwrap();
    assert_eq!((all.status, all.records, all.value), (Status::Ok, 24, 24.0));

    let by_group = store.aggregate(&agg(Some("group"))).unwrap();
    assert_eq!(by_group.buckets, 4);
    assert_eq!(by_group.rows.iter().map(|r| r.records).sum::<i64>(), 24);
    assert!(by_group.rows.windows(2).all(|w| w[0].value >= w[1].value), "largest first");
    let acme = by_group.rows.iter().find(|r| r.key == "C-ACME").unwrap();
    assert_eq!(acme.name.as_deref(), Some("Acme Corp"));

    // Filters and scoping narrow the same way search does.
    let mut open_acme = agg(None);
    open_acme.group = Some("acme".into());
    open_acme.filters = vec![("status".into(), "closed".into())];
    let closed = store.aggregate(&open_acme).unwrap();
    let listed = store.search(&SearchRequest {
        group: Some("acme".into()), filters: vec![("status".into(), "closed".into())], limit: 50, ..Default::default()
    }).unwrap();
    assert_eq!(closed.records, listed.total_matches);

    // A multi-valued filter puts a record in several buckets but counts it once.
    let tags = store.aggregate(&agg(Some("tags[]"))).unwrap();
    assert_eq!(tags.records, 24);
    assert!(tags.rows.iter().map(|r| r.records).sum::<i64>() >= 24);
}

#[test]
fn aggregate_matches_group_names_by_whole_words() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let mut req = agg(Some("group"));
    req.group_contains = Some("Acme".into());
    let out = store.aggregate(&req).unwrap();
    assert_eq!(out.rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), ["C-ACME"]);
    req.group_contains = Some("Acm".into());
    assert_eq!(store.aggregate(&req).unwrap().buckets, 0, "part of a word is not a word");
    req.group_contains = Some("c-ac".into());
    assert_eq!(store.aggregate(&req).unwrap().buckets, 1, "a key prefix matches");
}

#[test]
fn aggregate_sums_averages_and_ranges_a_numeric_field() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("jobs.jsonl");
    std::fs::write(
        &data,
        [
            r#"{"id":"j1","site":"north","kind":"repair","opened":"2024-01-05","hours":2.5,"labor":[{"h":1},{"h":1.5}]}"#,
            r#"{"id":"j2","site":"north","kind":"repair","opened":"2024-02-01","hours":4,"labor":[{"h":4}]}"#,
            r#"{"id":"j3","site":"south","kind":"service","opened":"2025-03-01","hours":"1,000","labor":[]}"#,
            r#"{"id":"j4","site":"south","kind":"repair","opened":"2025-03-09","hours":null}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let toml = tmp.path().join("leviathan.toml");
    std::fs::write(&toml, "[fields]\nid = \"id\"\ngroup = \"site\"\ndate = \"opened\"\nfilters = [\"kind\"]\n").unwrap();
    let cfg = Config::load(&toml).unwrap();
    let sources = source::discover(&[data], Format::Auto).unwrap();
    let index = tmp.path().join("jobs.db");
    index::build(&index, &sources, &cfg, false, quiet()).unwrap();
    let store = open(&index);

    let mut req = agg(Some("group"));
    req.metric = Metric::Sum;
    req.field = Some("hours".into());
    let sum = store.aggregate(&req).unwrap();
    assert_eq!(sum.value, 1006.5, "strings with thousands separators count, nulls do not");
    assert_eq!(sum.rows[0].key, "south");
    req.metric = Metric::Avg;
    req.by = Some("year".into());
    let avg = store.aggregate(&req).unwrap();
    assert_eq!(avg.rows.iter().map(|r| (r.key.as_str(), r.value)).collect::<Vec<_>>(), [("2025", 1000.0), ("2024", 3.25)]);
    req.by = None;
    req.metric = Metric::Max;
    req.field = Some("labor[].h".into());
    req.filters = vec![("kind".into(), "repair".into())];
    assert_eq!(store.aggregate(&req).unwrap().value, 4.0);
    req.metric = Metric::Min;
    req.since = Some("2024-02".into());
    req.until = Some("2024-02".into());
    let min = store.aggregate(&req).unwrap();
    assert_eq!((min.records, min.value), (1, 4.0));

    let mut bad = agg(Some("site"));
    assert_eq!(store.aggregate(&bad).unwrap().status, Status::BadRequest, "by must be group, a date unit, or a filter");
    bad.by = None;
    bad.metric = Metric::Sum;
    assert_eq!(store.aggregate(&bad).unwrap().status, Status::BadRequest, "sum needs a field");
}
