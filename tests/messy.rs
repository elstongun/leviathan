//! Index a directory of random, messy data the way an agent finds it: several
//! kinds of record spread over JSON Lines, gzip, JSON arrays, wrapped and
//! pretty-printed JSON, header-row JSON, CSV with commas, semicolons, pipes,
//! byte-order marks and Latin-1, TSV and SQLite tables, next to empty, broken
//! and unrelated files. Every record must be indexed exactly once, under one
//! kind per real kind, and be findable, countable and fetchable.
//!
//! `LEVIATHAN_TEST_SEED=<n>` reruns with another dataset.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use flate2::{Compression, write::GzEncoder};
use leviathan::card::CardOptions;
use leviathan::query::{AggregateRequest, Metric, SearchRequest, Store};
use serde_json::{Map, Value, json};

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

const PREFIXES: [&str; 8] =
    ["ticket", "customer", "shipment", "sensor", "invoice", "employee", "asset", "visit"];
const WORDS: [&str; 10] =
    ["alpha", "bravo", "delta", "lima", "oscar", "sierra", "tango", "victor", "whiskey", "zulu"];
const STATES: [&str; 4] = ["open", "closed", "pending", "void"];

struct Kind {
    prefix: &'static str,
    has_id: bool,
    records: Vec<Map<String, Value>>,
}

fn generate(rng: &mut Lcg) -> Vec<Kind> {
    let n_kinds = 3 + rng.below(4) as usize;
    let mut prefixes: Vec<&str> = PREFIXES.to_vec();
    let mut kinds = Vec::new();
    for g in 0..n_kinds {
        let prefix = prefixes.remove(rng.below(prefixes.len() as u64) as usize);
        let has_id = rng.chance(75);
        let n = 30 + rng.below(250) as usize;
        let mut records = Vec::new();
        for i in 0..n {
            let mut r = Map::new();
            if has_id {
                r.insert("id".into(), json!(format!("{}-{i}", &prefix[..3].to_uppercase())));
            }
            let words: Vec<&str> = (0..2 + rng.below(4)).map(|_| rng.pick(&WORDS)).collect();
            // A word no other record has, to find this record by.
            r.insert(format!("{prefix}_memo"), json!(format!("{} u{g}x{i}q", words.join(" "))));
            r.insert(format!("{prefix}_status"), json!(rng.pick(&STATES)));
            r.insert(format!("{prefix}_owner"), json!(format!("owner {}", rng.below(12))));
            if rng.chance(85) {
                r.insert(format!("{prefix}_amount"), json!(rng.below(400) as f64 / 4.0));
            }
            if rng.chance(90) {
                let date =
                    format!("{}-{:02}-{:02}", 2019 + rng.below(7), 1 + rng.below(12), 1 + rng.below(28));
                r.insert(format!("{prefix}_when"), json!(date));
            }
            if rng.chance(30) {
                r.insert(format!("{prefix}_comment"), json!("Café, \"quoted\"; with | separators"));
            }
            records.push(r);
        }
        kinds.push(Kind { prefix, has_id, records });
    }
    kinds
}

fn header(rows: &[Map<String, Value>]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for r in rows {
        for k in r.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }
    keys
}

fn cell(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn delimited(rows: &[Map<String, Value>], delimiter: u8) -> Vec<u8> {
    let keys = header(rows);
    let mut w = csv::WriterBuilder::new().delimiter(delimiter).from_writer(Vec::new());
    w.write_record(&keys).unwrap();
    for r in rows {
        w.write_record(keys.iter().map(|k| cell(r.get(k)))).unwrap();
    }
    w.into_inner().unwrap()
}

/// Write one chunk of a kind's records in a random format.
fn write_chunk(
    rng: &mut Lcg,
    dir: &Path,
    db: &rusqlite::Connection,
    name: &str,
    rows: &[Map<String, Value>],
) -> &'static str {
    let objects: Vec<Value> = rows.iter().cloned().map(Value::Object).collect();
    let lines = || objects.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
    match rng.below(11) {
        0 => {
            std::fs::write(dir.join(format!("{name}.jsonl")), lines() + "\n").unwrap();
            "jsonl"
        }
        1 => {
            let mut enc = GzEncoder::new(
                std::fs::File::create(dir.join(format!("{name}.ndjson.gz"))).unwrap(),
                Compression::fast(),
            );
            enc.write_all(lines().as_bytes()).unwrap();
            enc.finish().unwrap();
            "ndjson.gz"
        }
        2 => {
            std::fs::write(dir.join(format!("{name}.json")), Value::Array(objects).to_string()).unwrap();
            "json array"
        }
        3 => {
            let doc =
                json!({"meta": {"exported": "2026-01-01", "count": rows.len()}, "data": {"rows": objects}});
            std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&doc).unwrap())
                .unwrap();
            "wrapped pretty json"
        }
        4 => {
            std::fs::write(dir.join(format!("{name}.csv")), delimited(rows, b',')).unwrap();
            "csv"
        }
        5 => {
            let mut bytes = b"\xEF\xBB\xBF".to_vec();
            bytes.extend(delimited(rows, b';'));
            std::fs::write(dir.join(format!("{name}.csv")), bytes).unwrap();
            "csv ; bom"
        }
        6 => {
            std::fs::write(dir.join(format!("{name}.tsv")), delimited(rows, b'\t')).unwrap();
            "tsv"
        }
        7 => {
            std::fs::write(dir.join(format!("{name}.csv")), delimited(rows, b'|')).unwrap();
            "csv |"
        }
        8 => {
            let utf8 = String::from_utf8(delimited(rows, b',')).unwrap();
            let latin1: Vec<u8> = utf8.chars().map(|c| c as u32 as u8).collect();
            std::fs::write(dir.join(format!("{name}.csv")), latin1).unwrap();
            "csv latin-1"
        }
        9 => {
            let keys = header(rows);
            let cols: Vec<String> = keys.iter().map(|k| format!("\"{k}\"")).collect();
            db.execute_batch(&format!("CREATE TABLE \"{name}\" ({})", cols.join(", "))).unwrap();
            let marks: Vec<String> = (1..=keys.len()).map(|i| format!("?{i}")).collect();
            let mut insert =
                db.prepare(&format!("INSERT INTO \"{name}\" VALUES ({})", marks.join(", "))).unwrap();
            for r in rows {
                let values: Vec<rusqlite::types::Value> = keys
                    .iter()
                    .map(|k| match r.get(k) {
                        Some(Value::Number(n)) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
                        Some(Value::String(s)) => rusqlite::types::Value::Text(s.clone()),
                        _ => rusqlite::types::Value::Null,
                    })
                    .collect();
                insert.execute(rusqlite::params_from_iter(values)).unwrap();
            }
            "sqlite table"
        }
        _ => {
            let keys = header(rows);
            let mut table = vec![Value::Array(keys.iter().map(|k| json!(k)).collect())];
            table.extend(rows.iter().map(|r| {
                Value::Array(keys.iter().map(|k| r.get(k).cloned().unwrap_or(Value::Null)).collect())
            }));
            std::fs::write(dir.join(format!("{name}.json")), Value::Array(table).to_string()).unwrap();
            "header-row json"
        }
    }
}

fn run_seed(seed: u64) {
    let mut rng = Lcg(seed);
    let tmp = tempfile::tempdir().unwrap();
    // LEVIATHAN_TEST_KEEP=1 leaves the generated directory for inspection.
    let (_guard, root) = if std::env::var_os("LEVIATHAN_TEST_KEEP").is_some() {
        let root = tmp.keep();
        eprintln!("kept {}", root.display());
        (None, root)
    } else {
        let root = tmp.path().to_path_buf();
        (Some(tmp), root)
    };
    let data = root.join("workspace");
    std::fs::create_dir_all(data.join("exports/2026")).unwrap();
    let db = rusqlite::Connection::open(data.join("warehouse.sqlite")).unwrap();
    let kinds = generate(&mut rng);
    let mut layout = Vec::new();
    for (g, kind) in kinds.iter().enumerate() {
        let chunks = 1 + rng.below(4) as usize;
        let size = kind.records.len().div_ceil(chunks);
        for (c, rows) in kind.records.chunks(size).enumerate() {
            let dir = if rng.chance(30) { data.join("exports/2026") } else { data.clone() };
            let name = format!("{}_export_{g}{c}", kind.prefix);
            let format = write_chunk(&mut rng, &dir, &db, &name, rows);
            layout.push(format!("{name}: {} records as {format}", rows.len()));
        }
    }
    drop(db);
    // What else lies around in a real folder.
    std::fs::write(data.join("empty.csv"), "").unwrap();
    std::fs::write(data.join("empty.jsonl"), "\n\n").unwrap();
    std::fs::write(data.join("blank.json"), "{}").unwrap();
    std::fs::write(data.join("broken.json"), "[{\"id\": 1}, {\"id\": 2").unwrap();
    std::fs::write(data.join("fake.db"), "not a database").unwrap();
    std::fs::write(data.join("notes.txt"), "free text, not data").unwrap();
    let context = format!("seed {seed}:\n  {}", layout.join("\n  "));

    let index = root.join("index.db");
    let out = Command::new(env!("CARGO_BIN_EXE_leviathan"))
        .args(["--json", "--index"])
        .arg(&index)
        .args(["index", "-q"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(out.status.success(), "{context}\n{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let total: usize = kinds.iter().map(|k| k.records.len()).sum();
    assert_eq!(report["record_count"], json!(total), "{context}\nevery record once: {report}");

    let store = Store::open(&index, CardOptions::default()).unwrap();
    let description = store.describe(5).unwrap();
    let counts: HashMap<String, i64> =
        description.kinds.iter().map(|k| (k.name.clone(), k.records)).collect();
    let mut seen_index_kinds = HashSet::new();
    for (g, kind) in kinds.iter().enumerate() {
        // Every sampled record is found by its own word, under one index kind.
        let mut index_kinds = HashSet::new();
        for i in (0..kind.records.len()).step_by(1 + kind.records.len() / 40) {
            let found = store
                .search(&SearchRequest { query: format!("u{g}x{i}q"), limit: 5, ..Default::default() })
                .unwrap();
            assert_eq!(
                found.total_matches,
                1,
                "{context}\nrecord {g}/{i}: {:?}",
                found.results.iter().map(|c| &c.id).collect::<Vec<_>>()
            );
            let id = &found.results[0].id;
            index_kinds.insert(id.split_once(':').map(|(k, _)| k.to_string()).unwrap_or_default());
            if kind.has_id {
                let bare = kind.records[i]["id"].as_str().unwrap();
                assert_eq!(
                    id.split_once(':').map(|(_, rest)| rest),
                    Some(bare),
                    "{context}\nthe id field is the id"
                );
                let doc = store.get(bare).unwrap().unwrap_or_else(|| panic!("{context}\nget {bare}"));
                assert_eq!(
                    cell(doc.get(format!("{}_memo", kind.prefix).as_str())),
                    cell(kind.records[i].get(&format!("{}_memo", kind.prefix)))
                );
            }
        }
        assert_eq!(index_kinds.len(), 1, "{context}\nkind {} split across {index_kinds:?}", kind.prefix);
        let name = index_kinds.into_iter().next().unwrap();
        assert!(seen_index_kinds.insert(name.clone()), "{context}\ntwo kinds merged into {name}");
        assert_eq!(
            counts.get(&name).copied(),
            Some(kind.records.len() as i64),
            "{context}\nkind {name} count"
        );

        // Exact totals over a kind's numbers, whatever format they came in.
        let req = AggregateRequest {
            filters: vec![("_kind".into(), name.clone())],
            metric: Metric::Sum,
            field: Some(format!("{}_amount", kind.prefix)),
            limit: 10,
            ..Default::default()
        };
        let expected: f64 =
            kind.records.iter().filter_map(|r| r.get(&format!("{}_amount", kind.prefix))?.as_f64()).sum();
        let sum = store.aggregate(&req).unwrap();
        assert_eq!(sum.records, kind.records.len() as i64, "{context}");
        assert!(
            (sum.value - expected).abs() < 1e-6,
            "{context}\nsum of {name} amounts {} != {expected}",
            sum.value
        );
    }
    assert_eq!(description.kinds.len(), kinds.len(), "{context}\nkinds: {:?}", counts);

    // Counting per kind agrees with the kinds.
    let per_kind = store
        .aggregate(&AggregateRequest { by: Some("_kind".into()), limit: 50, ..Default::default() })
        .unwrap();
    let rows: BTreeMap<String, i64> = per_kind.rows.iter().map(|r| (r.key.clone(), r.records)).collect();
    assert_eq!(rows, counts.into_iter().collect::<BTreeMap<_, _>>(), "{context}");
}

#[test]
fn random_messy_directories_index_every_record_once_under_the_right_kind() {
    match std::env::var("LEVIATHAN_TEST_SEED").ok().and_then(|s| s.parse().ok()) {
        Some(seed) => run_seed(seed),
        None => (1..=4).for_each(|i| run_seed(i * 7919)),
    }
}
