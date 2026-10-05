//! Record sources: JSONL / NDJSON, JSON arrays, CSV / TSV (each optionally
//! gzip), SQLite tables or queries, and stdin. Every source yields JSON
//! objects; line-oriented formats stream, so inputs far larger than memory
//! are fine.
//!
//! Any other database works through its export tool and stdin, e.g.
//! `psql --csv -c 'select ...' | leviathan index - --format csv`.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use flate2::read::MultiGzDecoder;
use serde_json::{Map, Value};

use crate::config::Format;

const BUF: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub format: Format,
}

impl Source {
    pub fn is_stdin(&self) -> bool {
        self.path.as_os_str() == "-"
    }

    /// Short label for synthetic ids and messages.
    pub fn label(&self) -> String {
        if self.is_stdin() {
            return "stdin".into();
        }
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// One unit read from a source.
pub enum Item {
    Record { line: u64, value: Value, raw: Option<String> },
    Bad { line: u64, error: String },
}

fn format_of(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "jsonl" | "ndjson" => Format::Jsonl,
        "json" => Format::Json,
        "csv" => Format::Csv,
        "tsv" | "tab" => Format::Tsv,
        "db" | "sqlite" | "sqlite3" => Format::Sqlite,
        _ => return None,
    })
}

/// Expand files and directories into sources. Directories contribute every
/// JSONL, JSON, CSV and TSV file (optionally `.gz`) beneath them; SQLite
/// files must be named explicitly.
pub fn discover(paths: &[PathBuf], format: Format) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for path in paths {
        if path.as_os_str() == "-" {
            out.push(Source { path: path.clone(), format });
        } else if path.is_dir() {
            let mut found = BTreeSet::new();
            walk(path, &mut found)?;
            for file in found {
                let detected = format_of(&file).unwrap_or(Format::Auto);
                out.push(Source {
                    path: file,
                    format: if format == Format::Auto { detected } else { format },
                });
            }
        } else if path.is_file() {
            let detected = format_of(path).unwrap_or(Format::Auto);
            out.push(Source {
                path: path.clone(),
                format: if format == Format::Auto { detected } else { format },
            });
        } else {
            bail!("source path not found: {}", path.display());
        }
    }
    if out.is_empty() {
        bail!("no data files found (looked for .jsonl .ndjson .json .csv .tsv, optionally .gz)");
    }
    Ok(out)
}

fn walk(dir: &Path, out: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        let hidden = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        if path.is_dir() {
            walk(&path, out)?;
        } else if matches!(format_of(&path), Some(Format::Jsonl | Format::Json | Format::Csv | Format::Tsv)) {
            out.insert(path);
        }
    }
    Ok(())
}

/// Paths, sizes and mtimes; empty when a source cannot be fingerprinted
/// (stdin), which forces a rebuild.
pub fn manifest(sources: &[Source], extra: &str) -> Result<String> {
    let mut entries = Vec::new();
    for s in sources {
        if s.is_stdin() {
            return Ok(String::new());
        }
        let meta = fs::metadata(&s.path).with_context(|| format!("stat {}", s.path.display()))?;
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_nanos();
        entries.push(serde_json::json!({
            "path": s.path.canonicalize().unwrap_or_else(|_| s.path.clone()),
            "size": meta.len(),
            "mtime_ns": mtime.to_string(),
        }));
    }
    Ok(serde_json::to_string(&serde_json::json!({"files": entries, "mapping": extra}))?)
}

pub fn byte_size(sources: &[Source]) -> u64 {
    sources.iter().filter(|s| !s.is_stdin()).filter_map(|s| fs::metadata(&s.path).ok()).map(|m| m.len()).sum()
}

fn open(source: &Source) -> Result<Box<dyn BufRead>> {
    if source.is_stdin() {
        return Ok(Box::new(BufReader::with_capacity(BUF, io::stdin())));
    }
    let file = File::open(&source.path).with_context(|| format!("open {}", source.path.display()))?;
    let gz = source.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gz"));
    let reader: Box<dyn Read> =
        if gz { Box::new(MultiGzDecoder::new(BufReader::with_capacity(BUF, file))) } else { Box::new(file) };
    Ok(Box::new(BufReader::with_capacity(BUF, reader)))
}

fn first_byte(reader: &mut Box<dyn BufRead>) -> Result<Option<u8>> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(None);
        }
        if let Some(pos) = buf.iter().position(|b| !b.is_ascii_whitespace()) {
            return Ok(Some(buf[pos]));
        }
        let n = buf.len();
        reader.consume(n);
    }
}

/// Stream every record of `source` into `on`. Return `false` from `on` to
/// stop early (sampling).
pub fn read(source: &Source, sql: Option<&str>, on: &mut dyn FnMut(Item) -> Result<bool>) -> Result<()> {
    if source.format == Format::Sqlite {
        return read_sqlite(&source.path, sql, on);
    }
    let mut reader = open(source)?;
    let format = match source.format {
        Format::Auto | Format::Json => match first_byte(&mut reader)? {
            None => return Ok(()),
            Some(b'[') => Format::Json,
            Some(b'{') => Format::Jsonl,
            Some(_) if source.format == Format::Json => Format::Jsonl,
            Some(_) => Format::Csv,
        },
        f => f,
    };
    match format {
        Format::Json => read_json_array(reader, on),
        Format::Csv => read_delimited(reader, b',', on),
        Format::Tsv => read_delimited(reader, b'\t', on),
        _ => read_jsonl(reader, on),
    }
}

fn read_jsonl(mut reader: Box<dyn BufRead>, on: &mut dyn FnMut(Item) -> Result<bool>) -> Result<()> {
    let mut line = String::new();
    let mut line_no = 0u64;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        line_no += 1;
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let item = match serde_json::from_str::<Value>(raw) {
            Ok(value @ Value::Object(_)) => Item::Record { line: line_no, value, raw: Some(raw.to_string()) },
            Ok(_) => Item::Bad { line: line_no, error: "not a JSON object".into() },
            Err(err) => Item::Bad { line: line_no, error: err.to_string() },
        };
        if !on(item)? {
            return Ok(());
        }
    }
}

fn read_json_array(reader: Box<dyn BufRead>, on: &mut dyn FnMut(Item) -> Result<bool>) -> Result<()> {
    let items: Vec<Value> = serde_json::from_reader(reader).context("parse JSON array")?;
    for (i, value) in items.into_iter().enumerate() {
        let line = i as u64 + 1;
        let item = match value {
            Value::Object(_) => Item::Record { line, value, raw: None },
            _ => Item::Bad { line, error: "array element is not an object".into() },
        };
        if !on(item)? {
            break;
        }
    }
    Ok(())
}

fn read_delimited(
    reader: Box<dyn BufRead>,
    delimiter: u8,
    on: &mut dyn FnMut(Item) -> Result<bool>,
) -> Result<()> {
    let mut csv = csv::ReaderBuilder::new().delimiter(delimiter).flexible(true).from_reader(reader);
    let headers: Vec<String> = csv.headers()?.iter().map(|h| h.trim().to_string()).collect();
    for row in csv.records() {
        let item = match row {
            Ok(row) => {
                let line = row.position().map(|p| p.line()).unwrap_or(0);
                let mut map = Map::new();
                for (h, v) in headers.iter().zip(row.iter()) {
                    if !v.trim().is_empty() && !h.is_empty() {
                        map.insert(h.clone(), Value::String(v.to_string()));
                    }
                }
                Item::Record { line, value: Value::Object(map), raw: None }
            }
            Err(err) => {
                let line = err.position().map(|p| p.line()).unwrap_or(0);
                Item::Bad { line, error: err.to_string() }
            }
        };
        if !on(item)? {
            break;
        }
    }
    Ok(())
}

fn read_sqlite(path: &Path, sql: Option<&str>, on: &mut dyn FnMut(Item) -> Result<bool>) -> Result<()> {
    use rusqlite::types::ValueRef;
    let conn =
        crate::index::open_ro(path).with_context(|| format!("open SQLite source {}", path.display()))?;
    let query = match sql {
        Some(q) => q.to_string(),
        None => {
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            match tables.as_slice() {
                [one] => format!("SELECT * FROM \"{}\"", one.replace('"', "\"\"")),
                _ => bail!(
                    "{} has {} tables ({}); choose one with --sql 'SELECT * FROM <table>'",
                    path.display(),
                    tables.len(),
                    tables.join(", ")
                ),
            }
        }
    };
    let mut stmt = conn.prepare(&query).with_context(|| format!("prepare {query:?}"))?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_string).collect();
    let mut rows = stmt.query([])?;
    let mut line = 0u64;
    while let Some(row) = rows.next()? {
        line += 1;
        let mut map = Map::new();
        for (i, name) in names.iter().enumerate() {
            let value = match row.get_ref(i)? {
                ValueRef::Null | ValueRef::Blob(_) => continue,
                ValueRef::Integer(n) => Value::from(n),
                ValueRef::Real(f) => {
                    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
                }
                ValueRef::Text(t) => {
                    let s = String::from_utf8_lossy(t).into_owned();
                    match s.trim_start().as_bytes().first() {
                        Some(b'{' | b'[') => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                        _ => Value::String(s),
                    }
                }
            };
            map.insert(name.clone(), value);
        }
        if !on(Item::Record { line, value: Value::Object(map), raw: None })? {
            break;
        }
    }
    Ok(())
}
