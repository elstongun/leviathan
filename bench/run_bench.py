#!/usr/bin/env python3
"""Leviathan vs. an agent reading the data directly.

Dataset: the synthetic maintenance log from bench/synth (an invented facility),
indexed with examples/maintenance/leviathan.toml. For every gold-labeled
question, measure what each strategy puts into the agent's context (tokens),
whether a relevant record is in there, and how long it took:

  leviathan       `leviathan search -g <group> "<question>" -n 5` (one call)
  grep_keywords   `rg -F <group key> corpus | rg -i -e <word> ...` (group, then any question word)
  grep_group      `rg -F <group key> corpus` (the group's whole history)
  read_all        the whole corpus file (computed, not run per query)

The grep baselines are given the exact group key even when the question named
the group by its human name, an advantage a real agent does not have.

Usage: python3 bench/run_bench.py [--scales 10000,100000,1000000] [--queries 200]
Writes bench/results/results.json (+ one partial file per scale).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
DATA = ROOT / "bench" / "data"
RESULTS = ROOT / "bench" / "results"
CONFIG = ROOT / "examples" / "maintenance" / "leviathan.toml"
TOOL_OUTPUT_CAP_CHARS = 30_000  # a common shell-tool output cap in coding agents
CONTEXT_WINDOW = 200_000
RECORD_ID = re.compile(r'"id":"(ML-\d+)"')
STOP = set(
    "a an and are as at be been but by did do does for from had has have how i if in into is it its last me my "
    "of on or our so that the their then there this time to was we were what when where which who why will with "
    "you again fixed fix tech says".split()
)


def tokenizer():
    try:
        import tiktoken

        enc = tiktoken.get_encoding("o200k_base")
        return "o200k_base", lambda s: len(enc.encode_ordinary(s))
    except Exception:  # pragma: no cover - fallback when tiktoken is absent
        return "bytes/4", lambda s: len(s.encode()) // 4


TOKENIZER, _count = tokenizer()
EXACT_LIMIT = 1_000_000


def count_tokens(text: str) -> tuple[int, bool]:
    """Exact below 1 MB; above that, extrapolated from an even 512-line sample."""
    if len(text) <= EXACT_LIMIT:
        return _count(text), True
    lines = text.splitlines(keepends=True)
    step = max(1, len(lines) // 512)
    sample = "".join(lines[::step])
    ratio = _count(sample) / max(1, len(sample.encode()))
    return round(ratio * len(text.encode())), False


def pct(values, p):
    if not values:
        return 0.0
    ordered = sorted(values)
    k = (len(ordered) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (k - lo)


def run(cmd, shell=False):
    start = time.perf_counter()
    proc = subprocess.run(cmd, shell=shell, capture_output=True, text=True, errors="replace")
    return proc, time.perf_counter() - start


def keywords(question: str) -> list[str]:
    words = [w.lower() for w in re.findall(r"[A-Za-z0-9][A-Za-z0-9_.-]*", question)]
    out = []
    for w in words:
        if len(w) >= 3 and w not in STOP and w not in out:
            out.append(w)
    return out or words[:1]


def gold_in(text: str, relevant: set[str]) -> tuple[bool, bool]:
    full = any(m in relevant for m in RECORD_ID.findall(text))
    capped = any(m in relevant for m in RECORD_ID.findall(text[:TOOL_OUTPUT_CAP_CHARS]))
    return full, capped


def corpus_tokens(path: Path) -> tuple[int, int]:
    size = path.stat().st_size
    lines = []
    with path.open() as fh:
        for i, line in enumerate(fh):
            if i % 97 == 0:
                lines.append(line)
            if len(lines) >= 2000:
                break
    sample = "".join(lines)
    return round(_count(sample) / len(sample.encode()) * size), size


def bench_scale(n: int, args, lev: Path, synth: Path, rg: str, previous: dict | None = None) -> dict:
    out_dir = DATA / f"log_{n}"
    corpus = out_dir / "corpus" / "maintenance_log.jsonl"
    if not corpus.exists() or args.regenerate:
        subprocess.run(
            [str(synth), "--records", str(n), "--machines", str(args.machines), "--queries", str(args.queries),
             "--seed", str(args.seed), "--out", str(out_dir)],
            check=True,
        )
    db = out_dir / "leviathan.db"
    proc, build_s = run([str(lev), "--index", str(db), "--json", "index", "-c", str(CONFIG), str(out_dir / "corpus"), "--force", "-q"])
    if proc.returncode != 0:
        sys.exit(proc.stderr)
    build = json.loads(proc.stdout)
    manifest = json.loads((out_dir / "manifest.json").read_text())
    queries = [json.loads(l) for l in (out_dir / "queries.jsonl").read_text().splitlines() if l.strip()]
    if previous:
        all_tokens, corpus_bytes = previous["corpus_tokens"], previous["corpus_bytes"]
        cached = {r["qid"]: r for r in previous["rows"]}
    else:
        all_tokens, corpus_bytes = corpus_tokens(corpus)
        cached = {}
    print(f"[bench] {n:,} records: corpus {corpus_bytes / 1e6:.0f} MB ~{all_tokens:,} tokens; index built in {build_s:.1f}s",
          file=sys.stderr)

    # Warm the page cache so grep is measured at its best.
    if not previous:
        subprocess.run([rg, "-c", "zzzz-no-match", str(corpus)], capture_output=True)

    rows = []
    for i, q in enumerate(queries):
        relevant = set(q["relevant"])
        row = {"qid": q["qid"], "group_records": q["group_records"], "relevant": len(relevant),
               "ref_style": q["group_ref_style"], "generic_mode": q["generic_mode"]}

        text, secs = run([str(lev), "--index", str(db), "search", "-g", q["group"], q["query"], "-n", "5"])
        tok, _ = count_tokens(text.stdout)
        js, _ = run([str(lev), "--index", str(db), "--json", "search", "-g", q["group"], q["query"], "-n", "5"])
        result = json.loads(js.stdout)
        ids = [c["id"] for c in result.get("results", [])]
        rank = next((k + 1 for k, wid in enumerate(ids) if wid in relevant), None)
        row["leviathan"] = {
            "tokens": tok, "json_tokens": count_tokens(js.stdout)[0], "bytes": len(text.stdout.encode()),
            "seconds": secs, "status": result.get("status"), "hit1": rank == 1, "hit5": rank is not None,
            "rr": 1 / rank if rank else 0.0,
        }
        if q["qid"] in cached:
            row["grep_group"] = cached[q["qid"]]["grep_group"]
            row["grep_keywords"] = cached[q["qid"]]["grep_keywords"]
            rows.append(row)
            continue

        key = f'"{q["group_key"]}"'
        out, secs = run([rg, "-F", "--no-filename", "--", key, str(corpus)])
        tok, exact = count_tokens(out.stdout)
        full, capped = gold_in(out.stdout, relevant)
        row["grep_group"] = {"tokens": tok, "exact": exact, "bytes": len(out.stdout.encode()), "seconds": secs,
                               "gold": full, "gold_capped": capped, "lines": out.stdout.count("\n")}

        kws = keywords(q["query"])
        pipe = (f"{shlex.quote(rg)} -F --no-filename -- {shlex.quote(key)} {shlex.quote(str(corpus))} | "
                f"{shlex.quote(rg)} -i " + " ".join(f"-e {shlex.quote(k)}" for k in kws))
        out, secs = run(pipe, shell=True)
        tok, exact = count_tokens(out.stdout)
        full, capped = gold_in(out.stdout, relevant)
        row["grep_keywords"] = {"tokens": tok, "exact": exact, "bytes": len(out.stdout.encode()), "seconds": secs,
                                "gold": full, "gold_capped": capped, "lines": out.stdout.count("\n"),
                                "keywords": kws}
        rows.append(row)
        if (i + 1) % 25 == 0:
            print(f"[bench]   {i + 1}/{len(queries)} queries", file=sys.stderr)

    def summarize(method):
        tokens = [r[method]["tokens"] for r in rows]
        secs = [r[method]["seconds"] for r in rows]
        s = {
            "tokens_median": statistics.median(tokens), "tokens_mean": statistics.fmean(tokens),
            "tokens_p90": pct(tokens, 90), "tokens_max": max(tokens),
            "latency_p50_ms": pct(secs, 50) * 1000, "latency_p95_ms": pct(secs, 95) * 1000,
            "fits_context": sum(t <= CONTEXT_WINDOW for t in tokens) / len(rows),
        }
        if method == "leviathan":
            s.update(hit1=sum(r[method]["hit1"] for r in rows) / len(rows),
                     hit5=sum(r[method]["hit5"] for r in rows) / len(rows),
                     mrr5=statistics.fmean(r[method]["rr"] for r in rows),
                     json_tokens_median=statistics.median(r[method]["json_tokens"] for r in rows),
                     resolved=sum(r[method]["status"] == "ok" for r in rows) / len(rows))
        else:
            s.update(gold=sum(r[method]["gold"] for r in rows) / len(rows),
                     gold_capped=sum(r[method]["gold_capped"] for r in rows) / len(rows),
                     exact_token_share=sum(r[method]["exact"] for r in rows) / len(rows))
        return s

    return {
        "records": n, "machines": args.machines, "queries": len(rows), "seed": args.seed,
        "corpus_bytes": corpus_bytes, "corpus_tokens": all_tokens,
        "records_with_evidence": manifest["records_with_evidence"],
        "index": {"seconds": build_s, "bytes": build["index_bytes"], "docs_per_second": n / build_s,
                  "mb_per_second": corpus_bytes / 1e6 / build_s},
        "summary": {m: summarize(m) for m in ("leviathan", "grep_keywords", "grep_group")},
        "rows": rows,
    }


def mcp_schema_tokens(lev: Path, db: Path) -> int:
    """Tool schemas as an MCP client sees them, including the dataset summary."""
    msgs = "\n".join(json.dumps(m) for m in [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    ]) + "\n"
    proc = subprocess.run([str(lev), "--index", str(db), "mcp"], input=msgs, capture_output=True, text=True)
    tools = json.loads(proc.stdout.splitlines()[1])["result"]["tools"]
    return _count(json.dumps(tools, separators=(",", ":")))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--scales", default="10000,50000,100000,250000,500000,1000000")
    ap.add_argument("--queries", type=int, default=200)
    ap.add_argument("--machines", type=int, default=1500)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--regenerate", action="store_true")
    ap.add_argument("--rerun", action="store_true", help="ignore cached per-scale results")
    ap.add_argument("--leviathan-only", action="store_true",
                    help="re-measure Leviathan, reuse cached grep baselines (for ranking/output changes)")
    args = ap.parse_args()

    lev, synth = TARGET / "release" / "leviathan", TARGET / "release" / "leviathan-synth"
    if not lev.exists() or not synth.exists():
        subprocess.run(["cargo", "build", "--release", "--workspace"], cwd=ROOT, check=True)
    rg = os.environ.get("RG") or shutil.which("rg")
    if not rg:
        sys.exit("ripgrep (rg) is required for the baselines")
    RESULTS.mkdir(parents=True, exist_ok=True)

    scales = []
    for n in (int(s.replace("_", "")) for s in args.scales.split(",")):
        cache = RESULTS / f"scale_{n}.json"
        previous = json.loads(cache.read_text()) if cache.exists() else None
        if previous and not args.rerun and not args.leviathan_only:
            scales.append(previous)
            continue
        result = bench_scale(n, args, lev, synth, rg, previous if args.leviathan_only else None)
        cache.write_text(json.dumps(result))
        scales.append(result)

    rg_version = subprocess.run([rg, "--version"], capture_output=True, text=True).stdout.splitlines()[0]
    lev_version = subprocess.run([str(lev), "--version"], capture_output=True, text=True).stdout.strip()
    cpu = next((l.split(":", 1)[1].strip() for l in Path("/proc/cpuinfo").read_text().splitlines()
                if l.startswith("model name")), "unknown") if Path("/proc/cpuinfo").exists() else "unknown"
    report = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tokenizer": TOKENIZER, "tool_output_cap_chars": TOOL_OUTPUT_CAP_CHARS, "context_window": CONTEXT_WINDOW,
        "environment": {"leviathan": lev_version, "ripgrep": rg_version, "cpu": cpu, "cores": os.cpu_count(),
                        "python": sys.version.split()[0]},
        "mcp_schema_tokens": mcp_schema_tokens(lev, DATA / f"log_{scales[-1]['records']}" / "leviathan.db"),
        "scales": [{k: v for k, v in s.items() if k != "rows"} for s in scales],
        "rows_largest_scale": scales[-1]["rows"],
    }
    (RESULTS / "results.json").write_text(json.dumps(report, indent=2))
    print(f"[bench] wrote {RESULTS / 'results.json'}", file=sys.stderr)


if __name__ == "__main__":
    main()
