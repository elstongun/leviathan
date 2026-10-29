//! Record sources: JSONL / NDJSON, JSON documents, CSV / TSV (each optionally
//! gzip), every table of a SQLite file, and stdin. Every source yields JSON
//! objects; line-oriented formats stream, so inputs far larger than memory
//! are fine.
//!
//! Real exports are messy, so reading is forgiving: a JSON file may be one
//! record per line, an array, a pretty-printed document that wraps its
//! records (`{"data": {"items": [...]}}`), or a map of id to record; a CSV
//! file may start with a byte-order mark, use `;`, `|` or tabs, have blank or
//! repeated headers, or not be UTF-8.
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
    /// One table of a SQLite file.
    pub table: Option<String>,
    /// The kind its records belong to, in an index that holds several.
    pub kind: Option<String>,
}

impl Source {
    pub fn new(path: PathBuf, format: Format) -> Self {
        Self { path, format, table: None, kind: None }
    }

    pub fn is_stdin(&self) -> bool {
        self.path.as_os_str() == "-"
    }

    /// Short label for synthetic ids and messages.
    pub fn label(&self) -> String {
        if self.is_stdin() {
            return "stdin".into();
        }
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match &self.table {
            Some(t) => format!("{name}#{t}"),
            None => name,
        }
    }

    /// Stable identity for matching sources to kinds: the canonical path, and
    /// `#table` for one table of a SQLite file.
    pub fn key(&self) -> String {
        let path = self.path.canonicalize().unwrap_or_else(|_| self.path.clone());
        match &self.table {
            Some(t) => format!("{}#{t}", path.display()),
            None => path.display().to_string(),
        }
    }

    /// A name for what this source holds: the table, else the file name
    /// without extensions.
    pub fn stem(&self) -> String {
        if let Some(t) = &self.table {
            return t.clone();
        }
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = name.strip_suffix(".gz").unwrap_or(&name);
        name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string()
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

/// True when the file starts with SQLite's magic header.
pub fn is_sqlite_file(path: &Path) -> bool {
    let mut head = [0u8; 16];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == b"SQLite format 3\0"
}

/// Expand files and directories into sources. Directories contribute every
/// JSONL, JSON, CSV and TSV file (optionally `.gz`) and every SQLite file
/// beneath them. A SQLite file contributes each of its tables (or `sql`'s
/// result), except Leviathan's own index and memory files. `file.db#table`
/// names one table.
pub fn discover(paths: &[PathBuf], format: Format, sql: Option<&str>) -> Result<Vec<Source>> {
    // (source, named explicitly): a file found in a directory that turns out
    // unreadable is skipped; one named explicitly is an error.
    let mut files = Vec::new();
    for path in paths {
        if path.as_os_str() == "-" {
            files.push((Source::new(path.clone(), format), true));
        } else if path.is_dir() {
            let mut found = BTreeSet::new();
            walk(path, &mut found)?;
            for file in found {
                let detected = format_of(&file).unwrap_or(Format::Auto);
                files
                    .push((Source::new(file, if format == Format::Auto { detected } else { format }), false));
            }
        } else if path.is_file() {
            let detected = format_of(path).unwrap_or(Format::Auto);
            let detected =
                if detected == Format::Auto && is_sqlite_file(path) { Format::Sqlite } else { detected };
            files.push((
                Source::new(path.clone(), if format == Format::Auto { detected } else { format }),
                true,
            ));
        } else if let Some((file, table)) = path.to_str().and_then(|p| p.rsplit_once('#'))
            && Path::new(file).is_file()
        {
            let mut s = Source::new(PathBuf::from(file), Format::Sqlite);
            s.table = Some(table.to_string());
            files.push((s, true));
        } else {
            bail!("source path not found: {}", path.display());
        }
    }
    let mut out = Vec::new();
    for (s, explicit) in files {
        if s.format != Format::Sqlite || s.table.is_some() || sql.is_some() {
            out.push(s);
            continue;
        }
        match sqlite_tables(&s.path) {
            Ok(tables) => out.extend(tables.into_iter().map(|t| Source { table: Some(t), ..s.clone() })),
            Err(e) if explicit => return Err(e),
            Err(_) => {}
        }
    }
    if out.is_empty() {
        bail!(
            "no data files found (looked for .jsonl .ndjson .json .csv .tsv, optionally .gz, and SQLite tables)"
        );
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
        } else {
            match format_of(&path) {
                Some(Format::Jsonl | Format::Json | Format::Csv | Format::Tsv) => {
                    out.insert(path);
                }
                Some(Format::Sqlite) if is_sqlite_file(&path) => {
                    out.insert(path);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The tables and views of a SQLite file that hold data: not SQLite's own,
/// not full-text shadow tables, and none at all for a Leviathan index or
/// memory file.
pub fn sqlite_tables(path: &Path) -> Result<Vec<String>> {
    let conn =
        crate::index::open_ro(path).with_context(|| format!("open SQLite source {}", path.display()))?;
    let rows: Vec<(String, String, String)> = conn
        .prepare("SELECT name, type, coalesce(sql, '') FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name")
        .with_context(|| format!("{} is not a readable SQLite database", path.display()))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let names: BTreeSet<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
    let leviathan_index = ["meta", "records", "record_fts", "facets"].iter().all(|t| names.contains(t));
    let leviathan_memory = ["meta", "memories", "memory_fts"].iter().all(|t| names.contains(t));
    if leviathan_index || leviathan_memory {
        return Ok(Vec::new());
    }
    let virtual_tables: Vec<&str> = rows
        .iter()
        .filter(|(_, _, sql)| sql.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
        .map(|(n, _, _)| n.as_str())
        .collect();
    Ok(rows
        .iter()
        .map(|(n, _, _)| n)
        .filter(|n| !n.starts_with("sqlite_"))
        .filter(|n| !virtual_tables.iter().any(|v| *n == v || n.starts_with(&format!("{v}_"))))
        .cloned()
        .collect())
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
            "path": s.key(),
            "size": meta.len(),
            "mtime_ns": mtime.to_string(),
        }));
    }
    Ok(serde_json::to_string(&serde_json::json!({"files": entries, "mapping": extra}))?)
}

pub fn byte_size(sources: &[Source]) -> u64 {
    let files: BTreeSet<&Path> = sources.iter().filter(|s| !s.is_stdin()).map(|s| s.path.as_path()).collect();
    files.into_iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum()
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

/// Skip whitespace and a UTF-8 byte-order mark; return the next byte.
fn first_byte(reader: &mut Box<dyn BufRead>) -> Result<Option<u8>> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(None);
        }
        if buf.starts_with(b"\xEF\xBB\xBF") {
            reader.consume(3);
            continue;
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
        return read_sqlite(&source.path, source.table.as_deref(), sql, on);
    }
    let mut reader = open(source)?;
    // A `.json` file holding one object is a document (`{"data": [...]}`);
    // several objects, one per line, are JSON Lines whatever the extension.
    let document_hint = source.format == Format::Json;
    let format = match source.format {
        Format::Auto | Format::Json | Format::Jsonl => match first_byte(&mut reader)? {
            None => return Ok(()),
            Some(b'[') => Format::Json,
            Some(b'{') => Format::Jsonl,
            Some(_) if source.format != Format::Auto => Format::Jsonl,
            Some(_) => Format::Csv,
        },
        f => f,
    };
    match format {
        Format::Json => read_json_document(reader, String::new(), on),
        Format::Csv => read_delimited(reader, None, on),
        Format::Tsv => read_delimited(reader, Some(b'\t'), on),
        _ => read_jsonl(reader, document_hint, on),
    }
}

/// One JSON object per line. A first line that is the start of a larger
/// document (pretty-printed JSON) switches to reading the whole document.
fn read_jsonl(
    mut reader: Box<dyn BufRead>,
    document_hint: bool,
    on: &mut dyn FnMut(Item) -> Result<bool>,
) -> Result<()> {
    let mut line = String::new();
    let mut line_no = 0u64;
    let mut records = 0u64;
    loop {
        line.clear();
        if read_line_lossy(&mut reader, &mut line)? == 0 {
            return Ok(());
        }
        line_no += 1;
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        let item = match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(map)) if map.is_empty() => {
                Item::Bad { line: line_no, error: "empty record".into() }
            }
            Ok(value @ Value::Object(_))
                if document_hint && records == 0 && only_whitespace_left(&mut reader)? =>
            {
                for (i, value) in records_in(value).into_iter().enumerate() {
                    let line = i as u64 + 1;
                    let item = match value {
                        Value::Object(map) if map.is_empty() => {
                            Item::Bad { line, error: "empty record".into() }
                        }
                        Value::Object(_) => Item::Record { line, value, raw: None },
                        _ => Item::Bad { line, error: "not a JSON object".into() },
                    };
                    if !on(item)? {
                        break;
                    }
                }
                return Ok(());
            }
            Ok(value @ Value::Object(_)) => {
                records += 1;
                Item::Record { line: line_no, value, raw: Some(raw.to_string()) }
            }
            Ok(_) => Item::Bad { line: line_no, error: "not a JSON object".into() },
            Err(err) if err.is_eof() && line_no == 1 => {
                return read_json_document(reader, line, on);
            }
            Err(err) => Item::Bad { line: line_no, error: err.to_string() },
        };
        if !on(item)? {
            return Ok(());
        }
    }
}

fn only_whitespace_left(reader: &mut Box<dyn BufRead>) -> Result<bool> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(true);
        }
        if buf.iter().any(|b| !b.is_ascii_whitespace()) {
            return Ok(false);
        }
        let n = buf.len();
        reader.consume(n);
    }
}

fn read_line_lossy(reader: &mut Box<dyn BufRead>, line: &mut String) -> Result<usize> {
    let mut bytes = Vec::new();
    let n = reader.read_until(b'\n', &mut bytes)?;
    line.push_str(&decode(&bytes));
    Ok(n)
}

/// UTF-8, or Latin-1 when the bytes are not UTF-8 (old spreadsheet exports).
fn decode(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => std::borrow::Cow::Borrowed(s),
        Err(_) => std::borrow::Cow::Owned(bytes.iter().map(|&b| b as char).collect()),
    }
}

/// A whole JSON document (`prefix` is text already read from it).
fn read_json_document(
    mut reader: Box<dyn BufRead>,
    prefix: String,
    on: &mut dyn FnMut(Item) -> Result<bool>,
) -> Result<()> {
    let mut bytes = prefix.into_bytes();
    reader.read_to_end(&mut bytes)?;
    let text = decode(&bytes);
    let doc: Value =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).context("parse JSON document")?;
    for (i, value) in records_in(doc).into_iter().enumerate() {
        let line = i as u64 + 1;
        let item = match value {
            Value::Object(map) if map.is_empty() => Item::Bad { line, error: "empty record".into() },
            Value::Object(_) => Item::Record { line, value, raw: None },
            _ => Item::Bad { line, error: "not a JSON object".into() },
        };
        if !on(item)? {
            break;
        }
    }
    Ok(())
}

/// The records a JSON document holds: an array's elements (a first row of
/// strings is a header for the rest), the largest array of objects inside an
/// object (`{"data": [...]}`, up to three levels deep), the values of a map
/// of id to record, or else the object itself.
fn records_in(doc: Value) -> Vec<Value> {
    match doc {
        Value::Array(items) => rows_to_objects(items),
        Value::Object(map) => {
            if let Some(path) = largest_object_array(&map, 0) {
                let mut cursor = Value::Object(map);
                for key in &path {
                    cursor = cursor.get_mut(key.as_str()).map(Value::take).unwrap_or(Value::Null);
                }
                return match cursor {
                    Value::Array(items) => rows_to_objects(items),
                    _ => Vec::new(),
                };
            }
            let keyed = map.len() >= 2 && map.values().all(Value::is_object);
            if keyed {
                return map
                    .into_iter()
                    .map(|(key, mut value)| {
                        if let Some(obj) = value.as_object_mut()
                            && !obj.contains_key("_key")
                        {
                            obj.insert("_key".into(), Value::String(key));
                        }
                        value
                    })
                    .collect();
            }
            vec![Value::Object(map)]
        }
        other => vec![other],
    }
}

/// Path to the longest array whose elements are mostly objects.
fn largest_object_array(map: &Map<String, Value>, depth: usize) -> Option<Vec<String>> {
    let mut best: Option<(usize, Vec<String>)> = None;
    for (key, value) in map {
        let found = match value {
            Value::Array(items)
                if !items.is_empty() && items.iter().filter(|v| v.is_object()).count() * 2 > items.len() =>
            {
                Some((items.len(), vec![key.clone()]))
            }
            Value::Object(inner) if depth < 2 => largest_object_array(inner, depth + 1).map(|mut p| {
                let len =
                    p.iter().try_fold(value, |v, k| v.get(k)).and_then(Value::as_array).map_or(0, Vec::len);
                p.insert(0, key.clone());
                (len, p)
            }),
            _ => None,
        };
        if let Some((len, path)) = found
            && best.as_ref().is_none_or(|(n, _)| len > *n)
        {
            best = Some((len, path));
        }
    }
    best.map(|(_, p)| p)
}

/// `[[header...], [row...], ...]` to objects; anything else unchanged.
fn rows_to_objects(items: Vec<Value>) -> Vec<Value> {
    let header: Option<Vec<String>> = match items.first() {
        Some(Value::Array(first))
            if items.len() > 1 && !first.is_empty() && first.iter().all(Value::is_string) =>
        {
            Some(first.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect())
        }
        _ => None,
    };
    let Some(header) = header else { return items };
    let header = unique_headers(header);
    items
        .into_iter()
        .skip(1)
        .map(|row| match row {
            Value::Array(cells) => {
                let mut map = Map::new();
                for (i, cell) in cells.into_iter().enumerate() {
                    if !cell.is_null() {
                        map.insert(
                            header.get(i).cloned().unwrap_or_else(|| format!("column_{}", i + 1)),
                            cell,
                        );
                    }
                }
                Value::Object(map)
            }
            other => other,
        })
        .collect()
}

/// Trimmed headers; blank ones become `column_N`, repeats get `_2`, `_3`.
fn unique_headers(raw: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    raw.into_iter()
        .enumerate()
        .map(|(i, h)| {
            let h = h.trim().trim_start_matches('\u{feff}').trim().to_string();
            let h = if h.is_empty() { format!("column_{}", i + 1) } else { h };
            let n = seen.entry(h.to_lowercase()).or_insert(0);
            *n += 1;
            if *n == 1 { h } else { format!("{h}_{n}") }
        })
        .collect()
}

/// The delimiter a CSV file's first line uses most, outside quotes.
fn sniff_delimiter(reader: &mut Box<dyn BufRead>) -> Result<u8> {
    let buf = reader.fill_buf()?;
    let line = buf.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let mut counts = [0usize; 4];
    let candidates = *b",;\t|";
    let mut quoted = false;
    for &b in line {
        if b == b'"' {
            quoted = !quoted;
        } else if !quoted && let Some(i) = candidates.iter().position(|&c| c == b) {
            counts[i] += 1;
        }
    }
    let best = (0..4).max_by_key(|&i| (counts[i], std::cmp::Reverse(i))).unwrap_or(0);
    Ok(if counts[best] == 0 { b',' } else { candidates[best] })
}

fn read_delimited(
    mut reader: Box<dyn BufRead>,
    delimiter: Option<u8>,
    on: &mut dyn FnMut(Item) -> Result<bool>,
) -> Result<()> {
    first_byte(&mut reader)?;
    let delimiter = match delimiter {
        Some(d) => d,
        None => sniff_delimiter(&mut reader)?,
    };
    let mut csv =
        csv::ReaderBuilder::new().delimiter(delimiter).flexible(true).has_headers(false).from_reader(reader);
    let mut rows = csv.byte_records();
    let headers = match rows.next() {
        Some(Ok(h)) => unique_headers(h.iter().map(|c| decode(c).into_owned()).collect()),
        Some(Err(err)) => bail!("read CSV header: {err}"),
        None => return Ok(()),
    };
    for row in rows {
        let item = match row {
            Ok(row) => {
                let line = row.position().map(|p| p.line()).unwrap_or(0);
                let mut map = Map::new();
                for (i, cell) in row.iter().enumerate() {
                    let v = decode(cell);
                    if v.trim().is_empty() {
                        continue;
                    }
                    let h = headers.get(i).cloned().unwrap_or_else(|| format!("column_{}", i + 1));
                    map.insert(h, Value::String(v.into_owned()));
                }
                if map.is_empty() {
                    continue;
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

fn read_sqlite(
    path: &Path,
    table: Option<&str>,
    sql: Option<&str>,
    on: &mut dyn FnMut(Item) -> Result<bool>,
) -> Result<()> {
    use rusqlite::types::ValueRef;
    let conn =
        crate::index::open_ro(path).with_context(|| format!("open SQLite source {}", path.display()))?;
    let query = match (sql, table) {
        (Some(q), _) => q.to_string(),
        (None, Some(t)) => format!("SELECT * FROM \"{}\"", t.replace('"', "\"\"")),
        (None, None) => {
            let tables = sqlite_tables(path)?;
            match tables.as_slice() {
                [one] => format!("SELECT * FROM \"{}\"", one.replace('"', "\"\"")),
                _ => bail!(
                    "{} has {} tables ({}); choose one with --sql 'SELECT * FROM <table>' or name it as {}#<table>",
                    path.display(),
                    tables.len(),
                    tables.join(", "),
                    path.display()
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
                    let s = decode(t).into_owned();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_all(path: &Path) -> (Vec<Value>, usize) {
        let mut good = Vec::new();
        let mut bad = 0;
        for s in discover(&[path.to_path_buf()], Format::Auto, None).unwrap() {
            read(&s, None, &mut |item| {
                match item {
                    Item::Record { value, .. } => good.push(value),
                    Item::Bad { .. } => bad += 1,
                }
                Ok(true)
            })
            .unwrap();
        }
        (good, bad)
    }

    #[test]
    fn json_documents_in_every_common_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "wrapped.json",
                "{\n  \"meta\": {\"page\": 1},\n  \"data\": {\"items\": [{\"id\": 1}, {\"id\": 2}, {\"id\": 3}]}\n}",
            ),
            (
                "keyed.json",
                "{\"u1\": {\"name\": \"a\"}, \"u2\": {\"name\": \"b\"}, \"u3\": {\"name\": \"c\"}}",
            ),
            ("rows.json", "[[\"id\", \"name\"], [1, \"a\"], [2, \"b\"], [3, \"c\", \"extra\"]]"),
            ("pretty.jsonl", "{\n \"items\": [{\"id\": 1}, {\"id\": 2}, {\"id\": 3}]\n}\n"),
            ("one.json", "{\"id\": 1, \"name\": \"single\"}"),
            ("bom.json", "\u{feff}[{\"id\": 1}, {\"id\": 2}, {\"id\": 3}]"),
        ];
        for (name, text) in cases {
            std::fs::write(dir.path().join(name), text).unwrap();
            let (records, bad) = read_all(&dir.path().join(name));
            let expected = if name == "one.json" { 1 } else { 3 };
            assert_eq!((records.len(), bad), (expected, 0), "{name}: {records:?}");
        }
        let (keyed, _) = read_all(&dir.path().join("keyed.json"));
        assert_eq!(keyed[0]["_key"], json!("u1"));
        let (rows, _) = read_all(&dir.path().join("rows.json"));
        assert_eq!(rows[2], json!({"id": 3, "name": "c", "column_3": "extra"}));
    }

    #[test]
    fn csv_with_bom_semicolons_bad_headers_and_latin1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.csv");
        let mut bytes =
            b"\xEF\xBB\xBFid;Name;;name;city\n1;Caf\xE9 Bleu;x;dup;Paris\n\n2;\"A;B\";;;Lyon;spill\n"
                .to_vec();
        bytes.extend_from_slice(b";;;;\n");
        std::fs::write(&path, bytes).unwrap();
        let (records, bad) = read_all(&path);
        assert_eq!(bad, 0);
        assert_eq!(records.len(), 2, "blank rows are skipped: {records:?}");
        assert_eq!(
            records[0],
            json!({"id": "1", "Name": "Café Bleu", "column_3": "x", "name_2": "dup", "city": "Paris"})
        );
        assert_eq!(records[1], json!({"id": "2", "Name": "A;B", "city": "Lyon", "column_6": "spill"}));
    }

    #[test]
    fn sqlite_files_contribute_every_data_table_but_not_leviathan_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("shop.sqlite");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE customers (id TEXT, name TEXT); INSERT INTO customers VALUES ('c1', 'Acme');
             CREATE TABLE orders (id INTEGER, customer TEXT); INSERT INTO orders VALUES (1, 'c1'), (2, 'c1');
             CREATE VIRTUAL TABLE notes USING fts5(body); INSERT INTO notes VALUES ('hello');",
        )
        .unwrap();
        drop(conn);
        let sources = discover(&[dir.path().to_path_buf()], Format::Auto, None).unwrap();
        let tables: Vec<_> = sources.iter().map(|s| s.table.clone().unwrap()).collect();
        assert_eq!(tables, ["customers", "orders"], "fts tables and their shadows are skipped");
        let (records, _) = read_all(dir.path());
        assert_eq!(records.len(), 3);

        let one = discover(&[PathBuf::from(format!("{}#orders", db.display()))], Format::Auto, None).unwrap();
        assert_eq!((one.len(), one[0].label()), (1, "shop.sqlite#orders".to_string()));

        let index = dir.path().join("idx.db");
        let jsonl = dir.path().join("a.jsonl");
        std::fs::write(&jsonl, "{\"id\":1}\n").unwrap();
        let cfg = crate::config::Config::default();
        let src = discover(&[jsonl], Format::Auto, None).unwrap();
        crate::index::build(
            &index,
            &src,
            &cfg,
            true,
            crate::index::BuildOptions { quiet: true, ..Default::default() },
        )
        .unwrap();
        assert!(sqlite_tables(&index).unwrap().is_empty(), "a Leviathan index is not data");
    }
}
