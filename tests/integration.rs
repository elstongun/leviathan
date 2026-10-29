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
    let sources = source::discover(&paths, cfg.source.format, None).unwrap();
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
    let sources = source::discover(&[delta], Format::Auto, None).unwrap();
    let report = index::upsert(&index, &sources, None, quiet()).unwrap();
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
    let sources = source::discover(&[example("tickets.jsonl")], Format::Auto, None).unwrap();
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
        let sources = source::discover(&[path.to_path_buf()], cfg.source.format, None).unwrap();
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
    let sources = source::discover(&[src], Format::Auto, None).unwrap();
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
    let counted: Value =
        serde_json::from_str(counted["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
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
    let listed = store
        .search(&SearchRequest {
            group: Some("acme".into()),
            filters: vec![("status".into(), "closed".into())],
            limit: 50,
            ..Default::default()
        })
        .unwrap();
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
    std::fs::write(
        &toml,
        "[fields]\nid = \"id\"\ngroup = \"site\"\ndate = \"opened\"\nfilters = [\"kind\"]\n",
    )
    .unwrap();
    let cfg = Config::load(&toml).unwrap();
    let sources = source::discover(&[data], Format::Auto, None).unwrap();
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
    assert_eq!(
        avg.rows.iter().map(|r| (r.key.as_str(), r.value)).collect::<Vec<_>>(),
        [("2025", 1000.0), ("2024", 3.25)]
    );
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

    let by_field = store.aggregate(&agg(Some("site"))).unwrap();
    assert_eq!(
        by_field.rows.iter().map(|r| (r.key.as_str(), r.records)).collect::<Vec<_>>(),
        [("north", 2), ("south", 2)],
        "any field splits, not just filters"
    );
    let mut bad = agg(Some("group,year,month"));
    assert_eq!(store.aggregate(&bad).unwrap().status, Status::BadRequest, "at most two dimensions");
    bad.by = Some("group".into());
    bad.numeric = vec!["hours>>4".into()];
    assert_eq!(store.aggregate(&bad).unwrap().status, Status::BadRequest, "a malformed condition");
    bad.numeric.clear();
    bad.by = None;
    bad.metric = Metric::Sum;
    assert_eq!(store.aggregate(&bad).unwrap().status, Status::BadRequest, "sum needs a field");
}

#[test]
fn unrecognized_non_ascii_dates_render_without_panicking() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("dates.db");
    let src = tmp.path().join("dates.jsonl");
    std::fs::write(&src, "{\"id\":\"1\",\"d\":\"2024年3月11日 午後\",\"t\":\"hello\"}\n").unwrap();
    let built = cli(&index, &["index", "-q", src.to_str().unwrap(), "--id", "id", "--date", "d"]);
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));

    let described = cli(&index, &["describe"]);
    assert!(described.status.success(), "{}", String::from_utf8_lossy(&described.stderr));
    assert!(String::from_utf8_lossy(&described.stdout).contains("dates 2024年3月11日 .. 2024年3月11日"));

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
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(&line).unwrap();
    assert!(
        init["result"]["instructions"].as_str().unwrap().contains("dated 2024年3月11日 to 2024年3月11日")
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

// ---- aggregate against a brute-force count over random data -----------------

/// A small deterministic generator, so failures reproduce.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const WORDS: [&str; 6] = ["pump", "valve", "motor", "belt", "sensor", "gearbox"];
const KINDS: [&str; 4] = ["repair", "service", "inspection", "install"];
const TAGS: [&str; 5] = ["urgent", "warranty", "night", "vendor", "safety"];

struct Row {
    site: Option<String>,
    date: Option<String>,
    kind: Option<&'static str>,
    tags: Vec<&'static str>,
    hours: Vec<f64>,
    hours_raw: Option<Value>,
    words: Vec<&'static str>,
}

fn random_rows(rng: &mut Lcg, n: usize) -> (Vec<Row>, String) {
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for i in 0..n {
        let site = (!rng.chance(5)).then(|| format!("S{}", rng.below(9)));
        let date = (!rng.chance(8))
            .then(|| format!("{}-{:02}-{:02}", 2020 + rng.below(6), 1 + rng.below(12), 1 + rng.below(28)));
        let kind = (!rng.chance(10)).then(|| KINDS[rng.below(4) as usize]);
        let mut tags: Vec<&str> = (0..rng.below(4)).map(|_| TAGS[rng.below(5) as usize]).collect();
        tags.sort_unstable();
        tags.dedup();
        let words: Vec<&str> = (0..1 + rng.below(3)).map(|_| WORDS[rng.below(6) as usize]).collect();
        // Hours in every shape a real export has: numbers, strings with
        // thousands separators, blanks, junk, or missing.
        let (hours_json, hours): (Option<Value>, Vec<f64>) = match rng.below(7) {
            0 => (Some(json!(null)), vec![]),
            1 => (Some(json!("n/a")), vec![]),
            2 => {
                let (whole, cents) = (1000 + rng.below(3000), rng.below(100));
                let text = format!("{},{:03}.{cents:02}", whole / 1000, whole % 1000);
                (Some(json!(text)), vec![whole as f64 + cents as f64 / 100.0])
            }
            3 => (None, vec![]),
            _ => {
                let v = rng.below(4000) as f64 / 4.0;
                (Some(json!(v)), vec![v])
            }
        };
        let mut doc = json!({
            "id": format!("R{i}"),
            "summary": words.join(" "),
            "tags": tags,
        });
        if let Some(s) = &site {
            doc["site"] = json!(s);
            doc["site_name"] = json!(format!("Site {s}"));
        }
        if let Some(d) = &date {
            doc["opened"] = json!(d);
        }
        if let Some(k) = kind {
            doc["kind"] = json!(k);
        }
        if let Some(h) = &hours_json {
            doc["hours"] = h.clone();
        }
        lines.push(doc.to_string());
        rows.push(Row { site, date, kind, tags, hours, hours_raw: hours_json, words });
    }
    (rows, lines.join("\n"))
}

#[test]
fn aggregate_agrees_with_a_brute_force_count_over_random_data() {
    let tmp = tempfile::tempdir().unwrap();
    let seed = std::env::var("LEVIATHAN_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5eed);
    let mut rng = Lcg(seed);
    let (rows, jsonl) = random_rows(&mut rng, 2500);
    let data = tmp.path().join("jobs.jsonl");
    std::fs::write(&data, jsonl).unwrap();
    let toml = tmp.path().join("leviathan.toml");
    std::fs::write(
        &toml,
        "[fields]\nid = \"id\"\ntitle = \"summary\"\ntext = [\"summary\"]\ngroup = \"site\"\ngroup_name = \"site_name\"\n\
         date = \"opened\"\nfilters = [\"kind\", \"tags[]\"]\n",
    )
    .unwrap();
    let cfg = Config::load(&toml).unwrap();
    let index = tmp.path().join("jobs.db");
    let report =
        index::build(&index, &source::discover(&[data], Format::Auto, None).unwrap(), &cfg, false, quiet())
            .unwrap();
    assert_eq!(report.record_count, 2500);
    let store = open(&index);

    let bys = [
        None,
        Some("group"),
        Some("year"),
        Some("month"),
        Some("day"),
        Some("kind"),
        Some("tags[]"),
        Some("site"),
        Some("hours"),
        Some("group,year"),
        Some("kind,tags[]"),
        Some("month,site"),
    ];
    let metrics = [Metric::Count, Metric::Sum, Metric::Avg, Metric::Min, Metric::Max, Metric::Distinct];
    for case in 0..600 {
        let mut req = AggregateRequest { limit: 200, ..Default::default() };
        req.by = bys[rng.below(bys.len() as u64) as usize].map(str::to_string);
        req.metric = metrics[rng.below(6) as usize];
        req.field = match req.metric {
            Metric::Count => None,
            Metric::Distinct => Some(["tags[]", "site", "kind"][rng.below(3) as usize].into()),
            _ => Some("hours".into()),
        };
        let threshold = [0.0, 50.0, 500.0, 1500.0][rng.below(4) as usize];
        let condition = match rng.below(8) {
            0 => Some((format!("hours>{threshold}"), true)),
            1 => Some((format!("hours<={threshold}"), false)),
            _ => None,
        };
        if let Some((c, _)) = &condition {
            req.numeric.push(c.clone());
        }
        if rng.chance(30) {
            req.group = Some(format!("S{}", rng.below(9)));
        }
        if rng.chance(30) {
            req.filters.push(("kind".into(), KINDS[rng.below(4) as usize].into()));
        }
        if rng.chance(20) {
            req.filters.push(("tags[]".into(), TAGS[rng.below(5) as usize].into()));
        }
        if rng.chance(25) {
            req.since = Some(format!("{}-{:02}", 2020 + rng.below(6), 1 + rng.below(12)));
        }
        if rng.chance(25) {
            req.until = Some(format!("{}", 2021 + rng.below(5)));
        }
        let (word, without) = (WORDS[rng.below(6) as usize], WORDS[rng.below(6) as usize]);
        match rng.below(4) {
            0 => req.query = word.into(),
            1 if word != without => req.query = format!("{word} -{without}"),
            _ => {}
        }

        // The same selection, by hand.
        let selected: Vec<&Row> = rows
            .iter()
            .filter(|r| req.group.as_ref().is_none_or(|g| r.site.as_ref() == Some(g)))
            .filter(|r| {
                req.filters.iter().all(|(f, v)| match f.as_str() {
                    "kind" => r.kind == Some(v.as_str()),
                    _ => r.tags.contains(&v.as_str()),
                })
            })
            .filter(|r| {
                req.since.as_ref().is_none_or(|s| r.date.as_ref().is_some_and(|d| d.as_str() >= s.as_str()))
            })
            .filter(|r| {
                req.until
                    .as_ref()
                    .is_none_or(|u| r.date.as_ref().is_some_and(|d| &d[..u.len()] <= u.as_str()))
            })
            .filter(|r| match req.query.split_once(" -") {
                Some((w, not)) => r.words.contains(&w) && !r.words.contains(&not),
                None if !req.query.is_empty() => r.words.contains(&req.query.as_str()),
                None => true,
            })
            .filter(|r| match &condition {
                Some((_, true)) => r.hours.iter().any(|h| *h > threshold),
                Some((_, false)) => r.hours.iter().any(|h| *h <= threshold),
                None => true,
            })
            .collect();
        let dim_keys = |dim: &str, r: &Row| -> Vec<String> {
            let mut keys: Vec<String> = match dim {
                "group" | "site" => r.site.iter().cloned().collect(),
                "year" => r.date.as_deref().map(|d| d[..4].to_string()).into_iter().collect(),
                "month" => r.date.as_deref().map(|d| d[..7].to_string()).into_iter().collect(),
                "day" => r.date.iter().cloned().collect(),
                "kind" => r.kind.iter().map(|k| k.to_string()).collect(),
                "tags[]" => r.tags.iter().map(|t| t.to_string()).collect(),
                _ => match &r.hours_raw {
                    Some(Value::Number(n)) => vec![n.to_string()],
                    Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
                    _ => Vec::new(),
                },
            };
            keys.sort_unstable();
            keys.dedup();
            if keys.is_empty() {
                keys.push(String::new());
            }
            keys
        };
        let keys = |r: &Row| -> Vec<String> {
            let dims: Vec<&str> = req.by.as_deref().map(|b| b.split(',').collect()).unwrap_or_default();
            match dims.as_slice() {
                [] => vec![String::new()],
                [a] => dim_keys(a, r),
                [a, b] => dim_keys(a, r)
                    .iter()
                    .flat_map(|x| dim_keys(b, r).into_iter().map(move |y| format!("{x} · {y}")))
                    .collect(),
                _ => unreachable!(),
            }
        };
        let texts = |r: &Row| -> Vec<String> {
            match req.field.as_deref() {
                Some("tags[]") => r.tags.iter().map(|t| t.to_string()).collect(),
                Some("site") => r.site.iter().cloned().collect(),
                Some("kind") => r.kind.iter().map(|k| k.to_string()).collect(),
                _ => Vec::new(),
            }
        };
        let metric_of =
            |records: usize, values: &[f64], distinct: &std::collections::HashSet<String>| -> f64 {
                let round = |v: f64| (v * 1e6).round() / 1e6;
                match req.metric {
                    Metric::Count => records as f64,
                    Metric::Distinct => distinct.len() as f64,
                    _ if values.is_empty() => 0.0,
                    Metric::Sum => round(values.iter().sum()),
                    Metric::Avg => round(values.iter().sum::<f64>() / values.len() as f64),
                    Metric::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
                    Metric::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                }
            };
        type Bucket = (usize, Vec<f64>, std::collections::HashSet<String>);
        let mut buckets: std::collections::BTreeMap<String, Bucket> = Default::default();
        let numeric = req.metric != Metric::Count && req.metric != Metric::Distinct;
        for r in &selected {
            for k in keys(r) {
                let b = buckets.entry(k).or_default();
                b.0 += 1;
                if numeric {
                    b.1.extend(&r.hours);
                }
                b.2.extend(texts(r));
            }
        }
        let all_values: Vec<f64> = if numeric {
            selected.iter().flat_map(|r| r.hours.iter().copied()).collect()
        } else {
            Vec::new()
        };
        let all_distinct: std::collections::HashSet<String> =
            selected.iter().flat_map(|r| texts(r)).collect();

        let out = store.aggregate(&req).unwrap();
        let context = format!("case {case}: {req:?}");
        assert_eq!(out.status, Status::Ok, "{context}: {:?}", out.notes);
        assert_eq!(out.records, selected.len() as i64, "{context}");
        assert!(
            (out.value - metric_of(selected.len(), &all_values, &all_distinct)).abs() < 2e-6,
            "{context}: total {}",
            out.value
        );
        if req.by.is_some() {
            assert_eq!(out.buckets, buckets.len() as i64, "{context}");
            for row in &out.rows {
                let (n, values, distinct) = &buckets[&row.key];
                assert_eq!(row.records, *n as i64, "{context}: bucket {}", row.key);
                assert!(
                    (row.value - metric_of(*n, values, distinct)).abs() < 2e-6,
                    "{context}: bucket {} = {} expected {}",
                    row.key,
                    row.value,
                    metric_of(*n, values, distinct)
                );
            }
            assert!(out.rows.windows(2).all(|w| w[0].value >= w[1].value), "{context}: largest first");
        }
    }
}

#[test]
fn aggregate_reports_ambiguous_and_unknown_groups_and_truncates_to_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let store = open(&build_tickets(tmp.path()));
    let mut req = agg(Some("group"));
    req.group = Some("no such customer zzz".into());
    assert_eq!(store.aggregate(&req).unwrap().status, Status::UnknownGroup);

    let mut req = agg(Some("month"));
    req.limit = 2;
    let out = store.aggregate(&req).unwrap();
    assert_eq!(out.rows.len(), 2);
    assert!(out.buckets > 2, "buckets count every key before the limit");
    assert!(leviathan::render::aggregate(&out).contains("more (raise limit to see them)"));
}

#[test]
fn aggregate_cli_and_mcp_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let index = build_tickets(tmp.path());
    let out = cli(&index, &["--json", "aggregate", "--by", "group"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let cli_json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(cli_json["records"], 24);
    let text = cli(&index, &["aggregate", "--by", "status"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("distinct · total 24"));
    let unknown = cli(&index, &["aggregate", "--by", "nonsense"]);
    assert!(String::from_utf8_lossy(&unknown.stdout).contains("no values at `nonsense`"));
    let two = cli(&index, &["aggregate", "--by", "group,status", "--numeric", "id!=0"]);
    assert!(two.status.success(), "{}", String::from_utf8_lossy(&two.stderr));
    let limited = Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .env("LEVIATHAN_AGGREGATE_BUCKETS", "2")
        .args(["--index", index.to_str().unwrap(), "aggregate", "--by", "month"])
        .output()
        .unwrap();
    assert_eq!(limited.status.code(), Some(2), "over the bucket limit is refused");
    assert!(String::from_utf8_lossy(&limited.stdout).contains("more than 2 buckets"));

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
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut call = |args: Value| -> Value {
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "aggregate", "arguments": args}})).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let res = call(json!({"by": "group", "format": "json"}));
    let mcp_json: Value =
        serde_json::from_str(res["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_json["rows"], cli_json["rows"], "CLI and MCP compute the same buckets");
    let res = call(json!({"metric": "median"}));
    assert_eq!(res["result"]["isError"], true, "{res}");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

// ---- several kinds of record in one index -----------------------------------

fn write_shop(dir: &Path) {
    std::fs::write(
        dir.join("tickets.csv"),
        "id,customer_id,subject,status,opened\nT-1,C-1,Login loop after reset,open,2024-03-01\n\
         T-2,C-2,Invoice totals wrong,closed,2024-03-02\nT-3,C-1,Export times out,open,2024-03-05\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("customers.json"),
        r#"{"data": {"customers": [{"id": "C-1", "name": "Acme Corp", "plan": "enterprise"},
                                   {"id": "C-2", "name": "Globex", "plan": "starter"}]}}"#,
    )
    .unwrap();
}

#[test]
fn a_configured_index_holds_several_kinds_with_their_own_mappings() {
    let tmp = tempfile::tempdir().unwrap();
    write_shop(tmp.path());
    let toml = tmp.path().join("leviathan.toml");
    std::fs::write(
        &toml,
        r#"
        [[kinds]]
        name = "tickets"
        record = "ticket"
        group = "customer"
        paths = ["tickets.csv"]
        [kinds.fields]
        id = "id"
        title = "subject"
        group = "customer_id"
        date = "opened"
        filters = ["status"]

        [[kinds]]
        name = "customers"
        record = "customer"
        group = "customer"
        paths = ["customers.json"]
        [kinds.fields]
        id = "id"
        title = "name"
        group = "id"
        group_name = "name"
        filters = ["plan"]
        "#,
    )
    .unwrap();
    let index = tmp.path().join("shop.db");
    let built = cli(&index, &["--json", "index", "-q", "--config", toml.to_str().unwrap()]);
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    let report: Value = serde_json::from_slice(&built.stdout).unwrap();
    assert_eq!(report["record_count"], 5);
    assert_eq!(report["kinds"].as_array().unwrap().len(), 2);

    let store = open(&index);
    let d = store.describe(5).unwrap();
    assert_eq!(
        d.kinds.iter().map(|k| (k.name.as_str(), k.records)).collect::<Vec<_>>(),
        [("tickets", 3), ("customers", 2)]
    );
    assert_eq!(d.filters.iter().map(|f| f.field.as_str()).collect::<Vec<_>>(), ["_kind", "status", "plan"]);

    // One group spans kinds, named by the kind that has names.
    let acme = store.search(&req(Some("acme"), "")).unwrap();
    assert_eq!(ids(&acme), ["tickets:T-3", "tickets:T-1", "customers:C-1"]);
    let card = &acme.results[0];
    assert_eq!(
        card.title.as_deref(),
        Some("Export times out"),
        "each record is shown with its own kind's mapping"
    );
    assert_eq!(store.card_by_id("C-1").unwrap().unwrap().title.as_deref(), Some("Acme Corp"));
    assert!(store.get("T-2").unwrap().is_some(), "bare ids resolve");
    assert!(store.get("tickets:T-2").unwrap().is_some());

    // Filters from one kind narrow within it; `_kind` selects kinds.
    let mut open_tickets = req(None, "");
    open_tickets.filters = vec![("status".into(), "open".into())];
    assert_eq!(store.search(&open_tickets).unwrap().total_matches, 2);
    let mut enterprise = req(None, "");
    enterprise.filters = vec![("_kind".into(), "customers".into()), ("plan".into(), "enterprise".into())];
    assert_eq!(ids(&store.search(&enterprise).unwrap()), ["customers:C-1"]);
    let mut per_customer = agg(Some("group"));
    per_customer.filters = vec![("_kind".into(), "tickets".into())];
    let out = store.aggregate(&per_customer).unwrap();
    assert_eq!(
        out.rows.iter().map(|r| (r.key.as_str(), r.name.as_deref(), r.records)).collect::<Vec<_>>(),
        [("C-1", Some("Acme Corp"), 2), ("C-2", Some("Globex"), 1)]
    );

    // Upsert finds the kind from the path, or takes --kind; delete takes bare ids.
    let more = tmp.path().join("more_tickets.csv");
    std::fs::write(&more, "id,customer_id,subject,status,opened\nT-4,C-2,Refund stuck,open,2024-03-09\n")
        .unwrap();
    let unmatched = cli(&index, &["upsert", "-q", more.to_str().unwrap()]);
    assert!(!unmatched.status.success());
    assert!(String::from_utf8_lossy(&unmatched.stderr).contains("--kind"));
    let upserted = cli(&index, &["upsert", "-q", "--kind", "tickets", more.to_str().unwrap()]);
    assert!(upserted.status.success(), "{}", String::from_utf8_lossy(&upserted.stderr));
    std::fs::write(
        tmp.path().join("tickets.csv"),
        "id,customer_id,subject,status,opened\nT-1,C-1,Login loop after reset,closed,2024-03-01\n",
    )
    .unwrap();
    assert!(
        cli(&index, &["upsert", "-q", tmp.path().join("tickets.csv").to_str().unwrap()]).status.success()
    );
    let store = open(&index);
    assert_eq!(store.describe(5).unwrap().record_count, 6);
    assert_eq!(store.get("T-1").unwrap().unwrap()["status"], "closed");
    assert!(cli(&index, &["delete", "T-4", "C-2"]).status.success());
    let store = open(&index);
    assert_eq!(store.describe(5).unwrap().kinds.iter().map(|k| k.records).sum::<i64>(), 4);
}

#[test]
fn init_proposes_kinds_that_index_the_same_as_inference() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    write_shop(&data);
    let toml = tmp.path().join("leviathan.toml");
    let index = tmp.path().join("a.db");
    let init = cli(&index, &["init", data.to_str().unwrap(), "-o", toml.to_str().unwrap()]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(
        text.contains("[[kinds]]")
            && text.contains("name = \"tickets\"")
            && text.contains("name = \"customers\""),
        "{text}"
    );

    let from_config = cli(&index, &["--json", "index", "-q", "--config", toml.to_str().unwrap()]);
    assert!(from_config.status.success(), "{}", String::from_utf8_lossy(&from_config.stderr));
    let inferred_index = tmp.path().join("b.db");
    let inferred = cli(&inferred_index, &["--json", "index", "-q", data.to_str().unwrap()]);
    let (a, b): (Value, Value) = (
        serde_json::from_slice(&from_config.stdout).unwrap(),
        serde_json::from_slice(&inferred.stdout).unwrap(),
    );
    assert_eq!(
        (a["record_count"].clone(), a["group_count"].clone()),
        (b["record_count"].clone(), b["group_count"].clone())
    );
    assert_eq!(
        b["kinds"][0]["fields"]["group"],
        json!("id"),
        "customers link to the tickets' customer_id groups: {b}"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .arg("--index")
        .arg(&inferred_index)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let init: Value = serde_json::from_str(&line).unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("of 2 kinds") && instructions.contains("_kind"), "{instructions}");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
