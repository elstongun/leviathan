# Configuring Leviathan

Leviathan indexes **records**: JSON objects, CSV rows, or SQLite rows. A
small **field mapping** tells it which fields to use as the id, the title, the
searchable text, the group, the date and the filters. Everything else in a
record is kept and returned by `leviathan get`, but not searched.

There are three ways to provide the mapping, and they combine:

1. **Nothing.** `leviathan index data/` samples the data and infers a
   mapping. Good for a first look; `leviathan describe` shows what it chose.
2. **`leviathan.toml`**, written by hand or proposed by `leviathan init`.
   It's picked up from the working directory, or passed with
   `--config/-c` (env `LEVIATHAN_CONFIG`).
3. **Flags** on `leviathan index` (`--id`, `--title`, `--text`, `--group`,
   `--group-name`, `--date`, `--filter`, `--display`, `--empty-value`). They
   override the same keys from the file.

The effective mapping is stored inside the index, so `search`, `upsert`,
`describe` and the MCP server never need the config again. Changing the
mapping and re-running `leviathan index` rebuilds; an unchanged mapping over
unchanged files is a no-op.

## `leviathan init`

```bash
leviathan init data/                 # writes ./leviathan.toml
leviathan init data/ -o -            # print instead
leviathan init data/ --json          # proposal + per-field statistics, for agents
leviathan init app.db --sql "SELECT * FROM tickets WHERE created > '2023-01-01'"
```

It samples up to `--sample` records (default 2,000) and profiles every field
path: how often it's present, how many distinct values it has, how long it
is, and whether it parses as a date. It then proposes:

| Role | Chosen when |
|---|---|
| `id` | present in ≥99% of records, every value unique, short; prefers names like `id`, `uuid`, `key`, `number` |
| `date` | ≥90% of values parse as dates; prefers fields that are almost always present and named like `date`, `timestamp`, `created` |
| `group` | 2 to n/4 distinct values with a name like `customer`, `account`, `host`, `device`, `asset`, `project`, or a sibling name field |
| `group_name` | a sibling such as `customer.name`, `customer_name` or `Customer Name` |
| `title` | short text (8–240 chars, 2+ words) with a name like `title`, `subject`, `summary` |
| `filters` | up to 8 fields with 2–50 distinct, repeated, short values (`status`, `priority`, `tags[]`, …) |
| `text` | every remaining field with prose (20+ chars on average, or 3+ words) |

Every choice is commented with its reason, and the file lists every field
seen with its statistics. Review it like a pull request.

## Field paths

| Path | Means |
|---|---|
| `status` | top-level field |
| `customer.name` | nested object |
| `comments[].text` | `text` of every element of the `comments` array |
| `tags[]` | every element of the `tags` array |
| `Customer Name` | CSV header with a space |
| `meta.body` | key inside a JSON-valued SQLite column `meta` |

Arrays are flattened automatically, so `comments.text` works too. A literal
key containing dots (`"a.b"`, common in CSV headers) wins over nesting.

## Reference

```toml
[about]
name = "Example support desk"        # shown by describe and in MCP descriptions
description = "Customer tickets with resolutions"
record = "ticket"                    # noun for one record (default "record")
group = "customer"                   # noun for one group (default "group")

[source]
paths = ["tickets.jsonl"]            # used when `leviathan index` gets no paths; relative to this file
format = "auto"                      # auto | jsonl | json | csv | tsv | sqlite
sql = "SELECT * FROM tickets"        # SQLite sources only

[fields]
id = "ticket_id"                     # unique key; enables upsert/delete and `get`
title = "subject"                    # or a list: first present wins
text = ["body", "comments[].text"]   # searched; default: every string in the record
group = "customer.id"                # enables `-g`, per-group scoping and resolution
group_name = "customer.name"         # human name used to resolve "acme" -> C-ACME
date = ["closed_at", "created_at"]   # first present wins; enables --since/--until and `recent`
filters = ["status", "tags[]"]       # exact-match, faceted: --where status=open
display = ["status", "resolution"]   # shown on each result card
empty_values = ["n/a", "see notes"]  # placeholders treated as missing

[rank]
title_weight = 2.0                   # BM25 weight of the title vs the text (1.0)

[[rank.boost]]                       # multiply the score by 1 + weight when it applies
field = "resolution"                 # ... when any of these fields is present (non-empty)
weight = 0.15

[[rank.boost]]
field = "status"
equals = "closed"                    # ... or when it equals this value (case-insensitive)
weight = 0.03
```

All keys are optional. Unknown keys are an error, so typos don't pass
silently.

What each field enables:

| Field | Without it |
|---|---|
| `id` | records are keyed `<file>:<line>`; `upsert`/`delete` are unavailable |
| `title` | cards show the id and the best-matching snippet |
| `text` | every string in the record is searched |
| `group` | no `-g`, no per-group scoping; `resolve_group` is unavailable |
| `date` | no `--since`/`--until`; `recent` lists the last-ingested records first |
| `filters` | no `--where`; `describe` lists no values |

## Sources

| Input | How |
|---|---|
| JSONL / NDJSON | one object per line; `.gz` is read transparently |
| JSON | a top-level array of objects |
| CSV / TSV | header row required; cells are strings; `.gz` ok |
| SQLite | `--sql` query, or the database's only table; JSON text cells are parsed |
| stdin | `-`; sniffed (`{` JSONL, `[` JSON, else CSV) or set with `--format` |
| a directory | every `.jsonl .ndjson .json .csv .tsv` (and `.gz`) beneath it; hidden files skipped |

Dates are normalized to sortable `YYYY-MM-DDTHH:MM:SS` from ISO 8601 or RFC
3339 (offsets dropped), `YYYY/MM/DD`, US `M/D/YYYY [h:mm AM/PM]`, and epoch
seconds or milliseconds. `--since`/`--until` accept `YYYY`, `YYYY-MM` or
`YYYY-MM-DD`; `--until` includes the whole period.

Records that aren't valid are **counted and skipped** (the first five are
listed in the build report); `--strict` fails the build instead. A failed
build never replaces a working index.

## Any database, through a pipe

Leviathan doesn't connect to databases. It reads what their CLIs already
export, so credentials stay with the tool you already use. Stdin needs a
mapping (a config or flags), because inference would consume the stream; to
infer one, save a sample to a file and run `leviathan init` on it first.

```bash
# PostgreSQL: one JSON object per row
psql "$DATABASE_URL" -At -c "SELECT row_to_json(t) FROM tickets t" | leviathan index - -c tickets.toml

# MySQL / MariaDB: tab-separated with a header
mysql -B -e "SELECT * FROM tickets" app | leviathan index - --format tsv -c tickets.toml

# SQLite: directly
leviathan index app.db --sql "SELECT * FROM tickets" -c tickets.toml

# DuckDB: Parquet, CSV, anything DuckDB reads
duckdb -json -c "SELECT * FROM 'events/*.parquet'" | leviathan index - -c events.toml

# MongoDB: JSONL; ids look like {"_id": {"$oid": "..."}}
mongoexport --db app --collection tickets | leviathan index - --id '_id.$oid' -c tickets.toml
```

Keep an index fresh with `leviathan upsert` (insert or replace by id) and
`leviathan delete <id>...`, for example from a nightly export of rows changed
since the last run.

## Writing a config as an agent

An agent that already knows a database's schema can skip inference. The
recipe:

1. Pick the **id** (primary key) and the **group**: the entity people ask
   about by name (customer, host, device, repository, patient, machine). Add
   its human-readable name as `group_name`.
2. Put the fields people *describe things with* in `text`, the short headline
   in `title`, and the event time in `date`.
3. Make low-cardinality columns (`status`, `priority`, `region`, `tags[]`)
   `filters`, and pick 3–5 `display` fields that answer the usual question at a
   glance.
4. List placeholder values (`"n/a"`, `"-"`, `"NULL"`, `"done"`) as
   `empty_values`, and add a `rank.boost` for records that contain an answer.
5. Run `leviathan index`, then `leviathan describe` to check the counts and
   filter values.

With no schema knowledge, `leviathan init <sample> --json` returns the
proposal with per-field statistics to reason from.

## Examples

- [`examples/tickets`](../examples/tickets): 24 support tickets and a
  complete config. Used by the test suite.
- [`examples/maintenance`](../examples/maintenance): the config for the
  synthetic maintenance log used by the benchmark.
