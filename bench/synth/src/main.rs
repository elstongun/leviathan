//! Deterministic synthetic maintenance log for benchmarking: an invented
//! facility, its machines, and years of service records.
//!
//! Writes, into `--out`:
//! - `corpus/maintenance_log.jsonl`: one service record per line, with
//!   nested objects, arrays, empty placeholders and missing fields, like a
//!   real operational export
//! - `queries.jsonl`: questions with gold labels. A record is relevant to a
//!   query when it is on the same machine, records the same failure mode, and
//!   says what was done.
//! - `manifest.json`: generation parameters and counts
//!
//! Same arguments, same bytes.

mod catalog;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use catalog::{CONTEXT, CREWS, GENERIC, MISC, Mode, NOTES, STUBS, TECHS, TYPES};
use serde_json::{Map, Value, json};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        self.f() < p
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
}

struct Args {
    records: usize,
    machines: usize,
    queries: usize,
    seed: u64,
    out: PathBuf,
}

fn parse_args() -> Args {
    let mut args = Args {
        records: 100_000,
        machines: 1_500,
        queries: 200,
        seed: 42,
        out: PathBuf::from("bench/data/synth"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--records" | "-n" => args.records = value().replace('_', "").parse().expect("count"),
            "--machines" => args.machines = value().parse().expect("count"),
            "--queries" => args.queries = value().parse().expect("count"),
            "--seed" => args.seed = value().parse().expect("seed"),
            "--out" => args.out = PathBuf::from(value()),
            "--help" | "-h" => {
                println!("leviathan-synth [--records N] [--machines M] [--queries Q] [--seed S] [--out DIR]");
                std::process::exit(0);
            }
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

/// Index into a flat list of every failure mode: generic modes first.
fn mode(global: usize) -> &'static Mode {
    if global < GENERIC.len() {
        return &GENERIC[global];
    }
    let mut i = global - GENERIC.len();
    for ty in TYPES {
        if i < ty.modes.len() {
            return &ty.modes[i];
        }
        i -= ty.modes.len();
    }
    unreachable!("mode index out of range")
}

fn type_mode_base(ty: usize) -> usize {
    GENERIC.len() + TYPES[..ty].iter().map(|t| t.modes.len()).sum::<usize>()
}

struct Machine {
    id: String,
    name: String,
    ty: usize,
    /// Global mode indexes, chronic modes repeated so they recur.
    mode_bag: Vec<usize>,
    records: usize,
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn stamp(minutes: i64) -> String {
    let (y, m, d) = civil(minutes.div_euclid(1440));
    let rem = minutes.rem_euclid(1440);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:00", rem / 60, rem % 60)
}

fn fill(template: &str, rng: &mut Rng) -> String {
    let sides = ["infeed", "discharge", "operator side", "drive side", "rear"];
    let shifts = ["1st", "2nd", "3rd"];
    let n = rng.range(1, 24);
    template
        .replace("{n2}", &((n % 24) + 1).to_string())
        .replace("{n}", &n.to_string())
        .replace("{side}", rng.pick(&sides))
        .replace("{shift}", rng.pick(&shifts))
        .replace("{f}", &rng.range(1, 99).to_string())
}

fn collapse(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn hall(i: usize) -> String {
    let letters = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    if i < letters.len() {
        (letters[i] as char).to_string()
    } else {
        format!("{}{}", letters[i / letters.len() - 1] as char, letters[i % letters.len()] as char)
    }
}

fn main() -> std::io::Result<()> {
    let args = parse_args();
    let mut rng = Rng(args.seed);
    fs::create_dir_all(&args.out)?;

    // ---- machines ----------------------------------------------------------
    let total_weight: u32 = TYPES.iter().map(|t| t.weight).sum();
    let halls = (args.machines / 60).max(4);
    let mut per_slot: HashMap<(String, &str), usize> = HashMap::new();
    let mut machines: Vec<Machine> = Vec::with_capacity(args.machines);
    for _ in 0..args.machines {
        let mut roll = rng.below(total_weight as usize) as u32;
        let ty = TYPES
            .iter()
            .position(|t| {
                if roll < t.weight {
                    true
                } else {
                    roll -= t.weight;
                    false
                }
            })
            .unwrap();
        let t = &TYPES[ty];
        let h = hall(rng.below(halls));
        let nn = per_slot.entry((h.clone(), t.code)).or_insert(0);
        *nn += 1;
        let base = type_mode_base(ty);
        let mut bag: Vec<usize> = (0..t.modes.len()).map(|m| base + m).collect();
        for _ in 0..2 {
            let chronic = base + rng.below(t.modes.len());
            bag.extend([chronic; 3]);
        }
        machines.push(Machine {
            id: format!("{}-{h}{:02}", t.code, nn),
            name: format!("Hall {h} {} {:02}", t.name, nn),
            ty,
            mode_bag: bag,
            records: 0,
        });
    }
    // Heavy-tailed workload: a few machines carry most of the history.
    let mut order: Vec<usize> = (0..machines.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i + 1));
    }
    let mut cumulative = Vec::with_capacity(machines.len());
    let mut acc = 0.0;
    for (rank, _) in order.iter().enumerate() {
        acc += 1.0 / ((rank + 1) as f64).powf(0.75);
        cumulative.push(acc);
    }
    let pick_machine = |rng: &mut Rng| -> usize {
        let r = rng.f() * acc;
        order[cumulative.partition_point(|c| *c < r).min(order.len() - 1)]
    };

    // ---- records ------------------------------------------------------------
    let start = 16_071i64 * 1440; // 2014-01-01
    let span = 4_655i64 * 1440; // through 2026-09-30
    fs::create_dir_all(args.out.join("corpus"))?;
    let path = args.out.join("corpus").join("maintenance_log.jsonl");
    let mut out = BufWriter::with_capacity(8 << 20, File::create(&path)?);
    let mut relevant: HashMap<(usize, usize), Vec<String>> = HashMap::new();
    let mut eligible: Vec<(usize, usize)> = Vec::new();
    let mut evidence_count = 0usize;
    let mut bytes = 0u64;

    for n in 0..args.records {
        let id = format!("ML-{:07}", 1_000_000 + n);
        let mi = pick_machine(&mut rng);
        let t = &TYPES[machines[mi].ty];
        let opened = start + (n as i64 * span) / args.records.max(1) as i64 + rng.below(4 * 1440) as i64;
        let recent = opened > start + span - 30 * 1440;
        let status = if recent && rng.chance(0.3) {
            "open"
        } else if rng.chance(0.03) {
            "canceled"
        } else {
            "closed"
        };
        let roll = rng.f();
        let hours = (rng.range(2, 40) as f64) / 4.0;

        let mut doc = Map::new();
        let summary;
        let (kind, priority);
        let resolution: Option<String>;
        let mut codes = Value::Null;
        let mut steps: Vec<Value> = Vec::new();
        let mut parts: Vec<Value> = Vec::new();
        let mut plan = Value::Null;
        let mut mode_global: Option<usize> = None;

        if roll < 0.50 {
            let p = rng.pick(t.plans);
            kind = "planned";
            priority = "normal";
            summary = format!("{} - {}", p.name, machines[mi].name);
            resolution = Some(if rng.chance(0.82) {
                rng.pick(STUBS).to_string()
            } else {
                format!("Service completed. {}", rng.pick(NOTES))
            });
            for (i, s) in p.steps.iter().enumerate() {
                steps.push(json!({"n": i + 1, "text": s}));
            }
            plan = json!({"id": p.id, "name": p.name, "steps": p.steps});
        } else if roll < 0.88 {
            let global =
                if rng.chance(0.3) { rng.below(GENERIC.len()) } else { *rng.pick(&machines[mi].mode_bag) };
            let m = mode(global);
            mode_global = Some(global);
            let breakdown = rng.chance(0.45);
            kind = if breakdown { "breakdown" } else { "corrective" };
            priority = if breakdown { "urgent" } else { "high" };
            let context = fill(rng.pick(CONTEXT), &mut rng);
            let symptom = fill(rng.pick(m.symptoms), &mut rng);
            let mut s = collapse(&[&context, &symptom]);
            if rng.chance(0.25) {
                s.push_str(&format!(". {}", rng.pick(NOTES)));
            }
            summary = s;
            if rng.chance(0.6) {
                codes = json!({"problem": m.problem, "cause": m.cause, "action": m.action});
            }
            let r = rng.f();
            resolution = if r < 0.58 {
                Some(fill(rng.pick(m.fixes), &mut rng))
            } else if r < 0.85 {
                Some(rng.pick(STUBS).to_string())
            } else {
                None
            };
            if rng.chance(0.55) {
                let k = rng.range(2, m.steps.len().min(4));
                let first = rng.below(m.steps.len() - k + 1);
                for (i, text) in m.steps[first..first + k].iter().enumerate() {
                    let mut step = json!({"n": i + 1, "text": text});
                    if rng.chance(0.2) {
                        step["note"] = json!(fill(rng.pick(m.fixes), &mut rng));
                    }
                    steps.push(step);
                }
            }
            if rng.chance(0.5) {
                let (sku, name) = rng.pick(m.parts);
                parts.push(json!({"sku": sku, "name": name, "qty": rng.range(1, 2)}));
            }
        } else {
            let (ask, done) = rng.pick(MISC);
            kind = "request";
            priority = "low";
            summary = fill(ask, &mut rng);
            resolution = Some(if rng.chance(0.5) { done.to_string() } else { rng.pick(STUBS).to_string() });
        }

        let mut labor = Vec::new();
        for _ in 0..rng.range(1, 2) {
            let mut entry = json!({"tech": rng.pick(TECHS), "hours": hours});
            if mode_global.is_some() && rng.chance(0.35) {
                entry["note"] = json!(rng.pick(NOTES));
            }
            labor.push(entry);
        }

        let stub = resolution.as_deref().is_none_or(|r| STUBS.contains(&r));
        let evidence =
            !stub || !steps.is_empty() || !parts.is_empty() || labor.iter().any(|e| e.get("note").is_some());
        let closed = opened + (hours * 60.0) as i64 + rng.below(3 * 1440) as i64;

        let machine = &machines[mi];
        doc.insert("id".into(), json!(id));
        doc.insert("asset".into(), json!({"id": machine.id, "name": machine.name}));
        doc.insert("kind".into(), json!(kind));
        doc.insert("priority".into(), json!(priority));
        doc.insert("status".into(), json!(status));
        doc.insert("opened".into(), json!(stamp(opened)));
        doc.insert("closed".into(), if status == "open" { Value::Null } else { json!(stamp(closed)) });
        doc.insert("summary".into(), json!(summary));
        if !codes.is_null() {
            doc.insert("codes".into(), codes);
        }
        doc.insert("resolution".into(), resolution.map_or(Value::Null, Value::String));
        doc.insert("steps".into(), json!(steps));
        doc.insert("parts".into(), json!(parts));
        doc.insert("labor".into(), json!(labor));
        if !plan.is_null() {
            doc.insert("plan".into(), plan);
        }
        doc.insert("hours".into(), json!(hours));
        doc.insert("crew".into(), json!(rng.pick(CREWS)));

        let line = serde_json::to_string(&doc)?;
        bytes += line.len() as u64 + 1;
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        machines[mi].records += 1;

        if evidence {
            evidence_count += 1;
            if let Some(g) = mode_global {
                relevant.entry((mi, g)).or_default().push(id.clone());
                eligible.push((mi, g));
            }
        }
    }
    out.flush()?;

    // ---- queries ------------------------------------------------------------
    let mut queries = BufWriter::new(File::create(args.out.join("queries.jsonl"))?);
    let mut used = HashSet::new();
    let mut written = 0;
    let mut attempts = 0;
    while written < args.queries && attempts < args.queries * 50 && !eligible.is_empty() {
        attempts += 1;
        let key = *rng.pick(&eligible);
        if !used.insert(key) {
            continue;
        }
        let (mi, g) = key;
        let m = mode(g);
        let machine = &machines[mi];
        let style = rng.f();
        let (group, ref_style) = if style < 0.5 {
            (machine.id.clone(), "key")
        } else if style < 0.8 {
            (machine.name.clone(), "name")
        } else {
            (machine.name.to_lowercase(), "name_lowercase")
        };
        let lead = ["", "", "again ", "what fixed ", "how did we fix ", "tech says "];
        let query = format!("{}{}", rng.pick(&lead), rng.pick(m.asks));
        let q = json!({
            "qid": format!("q{written:04}"), "group": group, "group_ref_style": ref_style,
            "group_key": machine.id, "group_name": machine.name, "query": query, "mode": m.key,
            "generic_mode": g < GENERIC.len(), "relevant": relevant[&key], "group_records": machine.records,
        });
        serde_json::to_writer(&mut queries, &q)?;
        queries.write_all(b"\n")?;
        written += 1;
    }
    queries.flush()?;

    let manifest = json!({
        "generator": "leviathan-synth", "version": env!("CARGO_PKG_VERSION"), "seed": args.seed,
        "records": args.records, "machines": args.machines, "queries": written,
        "records_with_evidence": evidence_count, "corpus_bytes": bytes,
        "machine_types": TYPES.len(), "failure_modes": GENERIC.len() + TYPES.iter().map(|t| t.modes.len()).sum::<usize>(),
    });
    fs::write(args.out.join("manifest.json"), serde_json::to_string_pretty(&manifest)? + "\n")?;
    eprintln!(
        "[synth] {} records ({:.1} MB), {} machines, {} queries -> {}",
        args.records,
        bytes as f64 / 1e6,
        args.machines,
        written,
        args.out.display()
    );
    Ok(())
}
