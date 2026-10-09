use std::path::{Path, PathBuf};

use leviathan::card::CardOptions;
use leviathan::config::Config;
use leviathan::index::{self, BuildOptions};
use leviathan::memory::{
    Forget, Kind, Memory, PruneOptions, RecallRequest, RecallStatus, Remember, Settings, State, WriteStatus,
};
use leviathan::query::Store;
use leviathan::source;

fn open(dir: &Path) -> Memory {
    Memory::open(&dir.join("memory.db"), Settings::default()).unwrap()
}

fn note(text: &str) -> Remember {
    Remember { text: text.into(), ..Default::default() }
}

fn slot(subject: &str, key: &str, text: &str) -> Remember {
    Remember { text: text.into(), subject: Some(subject.into()), key: Some(key.into()), ..Default::default() }
}

fn recall(m: &mut Memory, query: &str) -> Vec<String> {
    let out = m.recall(&RecallRequest { query: query.into(), ..Default::default() }, None).unwrap();
    out.results.into_iter().map(|r| r.memory.text).collect()
}

#[test]
fn a_key_supersedes_the_old_value_and_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    let first = m.remember(&slot("Joshua", "editor", "Uses VS Code for everything")).unwrap();
    assert_eq!(first.status, WriteStatus::Created);
    assert_eq!(first.memory.kind, Kind::Fact);
    let second = m.remember(&slot("joshua", "Editor", "Switched to vim for all editing")).unwrap();
    assert_eq!(second.status, WriteStatus::Replaced);
    assert_eq!(second.replaced.as_ref().unwrap().id, first.memory.id);
    assert_eq!(second.memory.subject.as_deref(), Some("Joshua"), "first spelling of a subject is kept");

    assert_eq!(recall(&mut m, "editor"), ["Switched to vim for all editing"]);
    let history = m.history(Some(&second.memory.id), None, None, None).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].superseded_by.as_deref(), Some(second.memory.id.as_str()));
    let all = m
        .recall(&RecallRequest { query: "editor".into(), history: true, ..Default::default() }, None)
        .unwrap();
    assert_eq!(all.results.len(), 2);
    assert!(all.results.iter().any(|r| r.state == State::Superseded));

    let same = m.remember(&slot("Joshua", "editor", "Switched to vim for all editing")).unwrap();
    assert_eq!(same.status, WriteStatus::Duplicate);
    assert_eq!(m.stats().unwrap().current, 1);
}

#[test]
fn restatements_merge_and_similar_memories_are_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    let a = m.remember(&note("Prefers short commit messages in the imperative mood")).unwrap();
    let b = m.remember(&note("prefers short commit messages in imperative mood")).unwrap();
    assert_eq!(b.status, WriteStatus::Duplicate);
    assert_eq!(b.memory.id, a.memory.id);
    assert_eq!(b.memory.importance, 4, "a restatement makes a memory more important");

    m.remember(&note("Deploys go through the staging cluster before production")).unwrap();
    let c = m.remember(&note("Deploys skip the staging cluster on Fridays")).unwrap();
    assert_eq!(c.status, WriteStatus::Created);
    assert_eq!(c.related.len(), 1);
    assert!(c.render().contains("related"));
    assert_eq!(m.stats().unwrap().current, 3);
}

#[test]
fn secrets_and_malformed_writes_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    for text in [
        "the deploy token is ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5z",
        "db password: correct-horse-battery",
        "postgres://admin:s3cretpw@db.internal/app",
    ] {
        let err = m.remember(&note(text)).unwrap_err();
        assert!(err.downcast_ref::<leviathan::memory::Refused>().is_some(), "{text}");
        assert!(err.to_string().contains("credential"));
    }
    assert!(m.remember(&note(&"word ".repeat(200))).is_err());
    assert!(m.remember(&Remember { importance: Some(9), ..note("x") }).is_err());
    assert!(m.remember(&Remember { ns: Some("bad ns".into()), ..note("x") }).is_err());
    assert_eq!(m.stats().unwrap().current, 0);
}

#[test]
fn namespaces_are_isolated_unless_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    m.remember(&Remember { ns: Some("project:billing".into()), ..note("Invoices are generated nightly") })
        .unwrap();
    m.remember(&note("Invoices should be emailed as PDF")).unwrap();
    assert_eq!(recall(&mut m, "invoices"), ["Invoices should be emailed as PDF"]);
    let all = m.recall(
        &RecallRequest { query: "invoices".into(), ns: Some("*".into()), ..Default::default() },
        None,
    );
    assert_eq!(all.unwrap().results.len(), 2);
    let one = m
        .recall(
            &RecallRequest {
                query: "invoices".into(),
                ns: Some("project:billing".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(one.results[0].memory.text, "Invoices are generated nightly");
}

#[test]
fn recall_packs_into_the_budget_and_says_what_it_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    for i in 0..60 {
        m.remember(&Remember {
            subject: Some(format!("service {}", i % 7)),
            key: Some(format!("owner{i}")),
            ..note(&format!("Service number {i} is owned by team {} and pages the on-call rotation", i % 5))
        })
        .unwrap();
    }
    let out = m
        .recall(
            &RecallRequest { query: "service owned team".into(), budget: Some(150), ..Default::default() },
            None,
        )
        .unwrap();
    assert_eq!(out.total_matches, 60);
    assert!(!out.results.is_empty() && out.results.len() < 60);
    assert!(out.tokens <= 150, "used {} of 150", out.tokens);
    assert!(out.notes.iter().any(|n| n.contains("did not fit")));
    assert!(out.render().contains(&format!("shown {} of 60", out.results.len())));

    let brief = m.briefing(None, Some(400), None).unwrap();
    let subjects: std::collections::HashSet<_> =
        brief.results.iter().take(14).filter_map(|r| r.memory.subject.clone()).collect();
    assert!(subjects.len() >= 5, "briefing spreads across subjects: {subjects:?}");
}

#[test]
fn ranking_prefers_relevant_important_and_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    m.remember(&Remember { importance: Some(1), ..note("The staging database is restored every Sunday") })
        .unwrap();
    m.remember(&Remember {
        importance: Some(5),
        ..note("Never run migrations against the production database by hand")
    })
    .unwrap();
    assert_eq!(recall(&mut m, "database")[0], "Never run migrations against the production database by hand");
    m.remember(&Remember { pinned: true, ..note("Always ask before force-pushing") }).unwrap();
    let brief = m.briefing(None, None, None).unwrap();
    assert_eq!(brief.results[0].memory.text, "Always ask before force-pushing");
    assert!(brief.render().starts_with("Leviathan memory (default)"));
}

#[test]
fn subjects_resolve_by_alias_and_typo_and_ambiguity_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    m.remember(&Remember {
        aliases: vec!["JB".into()],
        ..slot("Joshua Baker", "timezone", "Works in US Central time")
    })
    .unwrap();
    m.remember(&slot("Billing service", "language", "Written in Rust")).unwrap();
    m.remember(&slot("Billing worker", "language", "Written in Go")).unwrap();

    let by_alias =
        m.recall(&RecallRequest { subject: Some("jb".into()), ..Default::default() }, None).unwrap();
    assert_eq!(by_alias.subject.as_deref(), Some("Joshua Baker"));
    assert_eq!(by_alias.results.len(), 1);
    let typo =
        m.recall(&RecallRequest { subject: Some("joshua bakr".into()), ..Default::default() }, None).unwrap();
    assert_eq!(typo.results.len(), 1);
    assert!(typo.notes[0].contains("matched"));
    let ambiguous =
        m.recall(&RecallRequest { subject: Some("billing".into()), ..Default::default() }, None).unwrap();
    assert_eq!(ambiguous.status, RecallStatus::AmbiguousSubject);
    assert!(ambiguous.results.is_empty());
    let alias_write = m.remember(&slot("JB", "timezone", "Works in US Eastern time now")).unwrap();
    assert_eq!(alias_write.status, WriteStatus::Replaced, "an alias writes to the canonical slot");
}

#[test]
fn forget_hides_a_memory_but_history_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    m.remember(&slot("ci", "runner", "CI runs on the self-hosted pool")).unwrap();
    let gone = m
        .forget(&Forget {
            subject: Some("CI".into()),
            key: Some("runner".into()),
            reason: Some("pool retired".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(gone.deleted_at.is_some());
    assert!(recall(&mut m, "runner").is_empty());
    let hist = m
        .recall(&RecallRequest { query: "runner".into(), history: true, ..Default::default() }, None)
        .unwrap();
    assert!(hist.render().contains("FORGOTTEN"));
    assert!(m.forget(&Forget { id: Some(gone.id.clone()), ..Default::default() }).is_err());
    assert_eq!(m.prune(PruneOptions { forgotten: true, ..Default::default() }).unwrap().forgotten, 1);
    assert!(m.get(&gone.id).unwrap().is_none());
}

#[test]
fn as_of_answers_what_was_true_then() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    let rows = [
        r#"{"id":"m0000000000000001","ns":"default","kind":"fact","subject":"api","key":"region","text":"API runs in us-east-1","importance":3,"confidence":1.0,"created_at":"2026-01-10T00:00:00Z","updated_at":"2026-01-10T00:00:00Z","valid_from":"2026-01-10T00:00:00Z","valid_to":"2026-06-01T00:00:00Z","superseded_by":"m0000000000000002"}"#,
        r#"{"id":"m0000000000000002","ns":"default","kind":"fact","subject":"api","key":"region","text":"API runs in eu-west-1","importance":3,"confidence":1.0,"created_at":"2026-06-01T00:00:00Z","updated_at":"2026-06-01T00:00:00Z","valid_from":"2026-06-01T00:00:00Z"}"#,
    ];
    let report = m.import_jsonl(rows.join("\n").as_bytes()).unwrap();
    assert_eq!(report.created, 2);
    let at = |m: &mut Memory, day: &str| -> Vec<String> {
        let out = m.recall(
            &RecallRequest { query: "api region".into(), as_of: Some(day.into()), ..Default::default() },
            None,
        );
        out.unwrap().results.into_iter().map(|r| r.memory.text).collect()
    };
    assert_eq!(at(&mut m, "2026-03"), ["API runs in us-east-1"]);
    assert_eq!(at(&mut m, "2026-06-01"), ["API runs in eu-west-1"]);
    assert_eq!(at(&mut m, "2025"), Vec::<String>::new());
    assert_eq!(recall(&mut m, "region"), ["API runs in eu-west-1"]);
}

#[test]
fn export_import_round_trips_and_markdown_imports_as_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = open(tmp.path());
    a.remember(&slot("joshua", "editor", "vim")).unwrap();
    a.remember(&slot("joshua", "editor", "helix")).unwrap();
    a.remember(&Remember {
        tags: vec!["Infra".into()],
        refs: vec!["T-1001".into()],
        ..note("Staging is rebuilt nightly")
    })
    .unwrap();
    let mut buf = Vec::new();
    assert_eq!(a.export(&mut buf).unwrap(), 3);

    let mut b = Memory::open(&tmp.path().join("restored.db"), Settings::default()).unwrap();
    let report = b.import_jsonl(buf.as_slice()).unwrap();
    assert_eq!((report.created, report.skipped), (3, 0));
    assert_eq!(b.import_jsonl(buf.as_slice()).unwrap().skipped, 3, "importing twice is harmless");
    assert_eq!(recall(&mut b, "editor"), ["helix"]);
    assert_eq!(b.stats().unwrap().superseded, 1);

    let md = "---\ntitle: notes\n- not an item\n---\n# Joshua\n- Prefers dark mode\n- Prefers dark mode\n\n```bash\n- not an item either\n```\n## Release process\n1. Tag after CI is green\n* [ ] Write the changelog first\nplain prose is ignored\n";
    let r = b.import_markdown(md, None, "MEMORY.md").unwrap();
    assert_eq!((r.read, r.created, r.merged), (4, 3, 1));
    let tagged = b
        .recall(&RecallRequest { subject: Some("release process".into()), ..Default::default() }, None)
        .unwrap();
    assert_eq!(tagged.results.len(), 2);
    assert_eq!(tagged.results[0].memory.source.as_deref(), Some("MEMORY.md"));
}

#[test]
fn expired_memories_drop_out_and_prune_removes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = open(tmp.path());
    let row = r#"{"id":"m0000000000000009","ns":"default","kind":"task","text":"Rotate the staging certificate","importance":3,"confidence":1.0,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","valid_from":"2026-01-01T00:00:00Z","expires_at":"2026-02-01T00:00:00Z"}"#;
    m.import_jsonl(row.as_bytes()).unwrap();
    assert!(recall(&mut m, "certificate").is_empty());
    assert_eq!(m.stats().unwrap().expired, 1);
    let dry = m.prune(PruneOptions { dry_run: true, ..Default::default() }).unwrap();
    assert_eq!(dry.expired, 1);
    assert_eq!(m.prune(PruneOptions::default()).unwrap().expired, 1);
    assert_eq!(m.stats().unwrap().expired, 0);
    let soon = m
        .remember(&Remember { expires: Some("2d".into()), ..note("Freeze deploys until the audit ends") })
        .unwrap();
    assert!(soon.memory.expires_at.is_some());
}

#[test]
fn recall_follows_refs_into_the_data_index() {
    let tmp = tempfile::tempdir().unwrap();
    let example = |f: &str| Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/tickets").join(f);
    let cfg = Config::load(&example("leviathan.toml")).unwrap();
    let paths: Vec<PathBuf> = cfg.source.paths.iter().map(PathBuf::from).collect();
    let sources = source::discover(&paths, cfg.source.format).unwrap();
    let index = tmp.path().join("tickets.db");
    index::build(&index, &sources, &cfg, false, BuildOptions { quiet: true, ..Default::default() }).unwrap();
    let store = Store::open(&index, CardOptions::default()).unwrap();

    let mut m = open(tmp.path());
    m.remember(&Remember {
        refs: vec!["T-1001".into(), "NOPE-1".into()],
        ..slot("Acme", "login.fix", "Acme login loops were fixed by clearing the stale SSO session")
    })
    .unwrap();
    let out = m
        .recall(
            &RecallRequest { query: "acme login".into(), with_records: true, ..Default::default() },
            Some(&store),
        )
        .unwrap();
    assert_eq!(out.records.len(), 1);
    assert_eq!(out.records[0].id, "T-1001");
    assert!(out.render().contains("linked records:"));
}

#[test]
fn a_data_index_is_not_mistaken_for_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("other.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
    drop(conn);
    assert!(Memory::open(&path, Settings::default()).is_err());
}
