#!/usr/bin/env python3
"""Leviathan memory vs markdown memory files.

Part 1, sessions: a synthetic project runs for many agent sessions. Each
session the agent learns facts, corrects some it learned before, notes
events and lessons, and answers questions about what it knows. Four memory
strategies see the same stream:

  leviathan       remember/recall through a real `leviathan mcp` process;
                  each session starts with the briefing, each question is
                  one `recall` with the question's words.
  md_full         MEMORY.md: append a line per memory, load the whole file
                  every session (how most agents do it today).
  md_200          the same file, but only the first 200 lines are loaded
                  (Claude Code's auto-memory rule).
  md_compacted    an idealized compactor: after every session the file
                  keeps only the newest line per fact, so it never holds a
                  stale value. Real LLM compaction is lossier; this is the
                  markdown best case.

Metrics: tokens put into context per session, questions whose loaded context
holds the current value (answerable), and questions whose context also holds
a superseded value (stale exposure: the agent can pick the wrong one).

Part 2, scale: recall and remember latency on stores of 10K, 100K and 1M
memories, against the tokens a markdown file of that size would cost.

Usage: python3 bench/memory/run_memory_bench.py --bin target/release/leviathan
Writes bench/results/memory.json; render with bench/memory/report.py.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import re
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

import tiktoken

ROOT = Path(__file__).resolve().parents[2]
RESULTS = ROOT / "bench" / "results"
ENC = tiktoken.get_encoding("o200k_base")


def tok(text: str) -> int:
    return len(ENC.encode(text, disallowed_special=()))


# ---- the synthetic project ----

KINDS_OF_SUBJECT = {
    "service": ["deploy target", "owner", "database", "on-call rotation", "runtime version", "rollout strategy"],
    "repo": ["package manager", "test command", "lint rule set", "release branch", "ci provider"],
    "person": ["preferred editor", "time zone", "review style", "team"],
    "env": ["region", "instance size", "secret store", "log retention"],
}
NAMES = ["atlas", "beacon", "cobalt", "delta", "ember", "falcon", "garnet", "harbor", "iris", "juniper", "kestrel",
         "lumen", "meridian", "nimbus", "onyx", "pylon", "quartz", "raven", "sable", "tundra", "umbra", "vega",
         "willow", "xenon", "yarrow", "zephyr"]
SUFFIX = {"service": "api", "repo": "web", "person": "", "env": "stage"}
VALUES = ["alder", "birch", "cedar", "dogwood", "elm", "fir", "ginkgo", "hazel", "ilex", "jacaranda", "larch",
          "maple", "nutmeg", "oak", "pine", "rowan", "spruce", "teak", "walnut", "yew"]
EVENTS = ["shipped", "rolled back", "migrated", "paged on", "load tested", "renamed", "upgraded", "archived"]
LESSONS = ["flaky when the cache is cold", "needs the feature flag before deploys", "breaks on daylight saving",
           "times out above 500 concurrent users", "must be drained before restarts"]


class World:
    def __init__(self, seed: int, subjects: int):
        self.rng = random.Random(seed)
        self.subjects = []
        kinds = list(KINDS_OF_SUBJECT)
        used = set()
        while len(self.subjects) < subjects:
            kind = kinds[len(self.subjects) % len(kinds)]
            name = f"{self.rng.choice(NAMES)}-{self.rng.choice(NAMES)}"
            if SUFFIX[kind]:
                name = f"{name}-{SUFFIX[kind]}"
            if name in used:
                continue
            used.add(name)
            self.subjects.append((name, kind))
        self.truth: dict[tuple[str, str], str] = {}
        self.history: dict[tuple[str, str], list[str]] = {}
        self.day = 0

    def value(self) -> str:
        return f"{self.rng.choice(VALUES)}-{self.rng.randint(2, 99)}"

    def session(self, learn: int, ask: int):
        """One session: questions about what was learned in earlier sessions
        (asked first), then the memories learned this session."""
        self.day += 1
        questions = []
        known = list(self.truth)
        for _ in range(ask if known else 0):
            slot = self.rng.choice(known)
            questions.append({"slot": slot, "query": f"{slot[0]} {slot[1]}", "current": self.truth[slot],
                              "stale": [v for v in self.history[slot] if v != self.truth[slot]]})
        learned = []
        for _ in range(learn):
            r = self.rng.random()
            subject, kind = self.rng.choice(self.subjects)
            if r < 0.70:
                attr = self.rng.choice(KINDS_OF_SUBJECT[kind])
                slot = (subject, attr)
                old = self.truth.get(slot)
                new = self.value()
                while new == old:
                    new = self.value()
                self.truth[slot] = new
                self.history.setdefault(slot, []).append(new)
                learned.append({"kind": "fact" if kind != "person" else "preference", "subject": subject,
                                "key": attr.replace(" ", "_"), "slot": slot, "value": new,
                                "text": f"{subject} {attr} is {new}" + (" (changed)" if old else "")})
            elif r < 0.88:
                ev = self.rng.choice(EVENTS)
                learned.append({"kind": "event", "subject": subject, "key": None, "slot": None, "value": None,
                                "text": f"day {self.day}: {ev} {subject}"})
            else:
                lesson = self.rng.choice(LESSONS)
                learned.append({"kind": "lesson", "subject": subject, "key": None, "slot": None, "value": None,
                                "text": f"{subject} is {lesson}"})
        return learned, questions


# ---- strategies ----

class Mcp:
    def __init__(self, binary: str, memory: Path):
        self.proc = subprocess.Popen([binary, f"--memory={memory}", "mcp"], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.n = 0
        self.call_raw("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                     "clientInfo": {"name": "bench", "version": "0"}})
        self.notify("notifications/initialized")

    def notify(self, method):
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method}) + "\n")

    def call_raw(self, method, params):
        self.n += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
        return json.loads(self.proc.stdout.readline())

    def tool(self, name, args) -> tuple[str, float]:
        t = time.perf_counter()
        reply = self.call_raw("tools/call", {"name": name, "arguments": args})
        ms = (time.perf_counter() - t) * 1000
        return reply["result"]["content"][0]["text"], ms

    def tools_tokens(self) -> int:
        return tok(json.dumps(self.call_raw("tools/list", {})["result"]["tools"]))

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=30)


def holds(text: str, fact: str, value: str) -> bool:
    """`fact value` appears as a whole value (pine-6 is not pine-69)."""
    return re.search(re.escape(fact + value) + r"(?![\w-])", text) is not None


def first_memory_line(text: str) -> str:
    for line in text.splitlines():
        if line.startswith("["):
            return line
    return ""


def run_sessions(binary: str, sessions: int, learn: int, ask: int, seed: int) -> dict:
    world = World(seed, subjects=60)
    tmp = Path(tempfile.mkdtemp(prefix="levmem-"))
    mcp = Mcp(binary, tmp / "memory.db")
    schema_tokens = mcp.tools_tokens()
    md_lines: list[str] = []
    compact: dict = {}  # slot or unique id -> line, insertion-ordered
    per_session = []
    totals = {s: {"answerable": 0, "stale": 0, "top1": 0, "asked": 0} for s in
              ("leviathan", "md_full", "md_200", "md_compacted")}
    write_ms, recall_ms = [], []
    for s in range(1, sessions + 1):
        learned, questions = world.session(learn, ask)
        # Session start: what each strategy loads.
        briefing, _ = mcp.tool("recall", {})
        start = {
            "leviathan": tok(briefing),
            "md_full": tok("\n".join(md_lines)),
            "md_200": tok("\n".join(md_lines[:200])),
            "md_compacted": tok("\n".join(compact.values())),
        }
        contexts = {"md_full": "\n".join(md_lines), "md_200": "\n".join(md_lines[:200]),
                    "md_compacted": "\n".join(compact.values())}
        q_tokens = 0
        for q in questions:
            out, ms = mcp.tool("recall", {"query": q["query"]})
            recall_ms.append(ms)
            q_tokens += tok(out)
            subject, attr = q["slot"]
            fact = f"{subject} {attr} is "
            stats = totals["leviathan"]
            stats["asked"] += 1
            stats["answerable"] += holds(out, fact, q["current"])
            stats["top1"] += holds(first_memory_line(out), fact, q["current"])
            stats["stale"] += any(holds(out, fact, v) for v in q["stale"])
            for name, ctx in contexts.items():
                st = totals[name]
                st["asked"] += 1
                has = holds(ctx, fact, q["current"])
                st["answerable"] += has
                st["top1"] += has
                st["stale"] += any(holds(ctx, fact, v) for v in q["stale"])
        for m in learned:
            args = {"text": m["text"], "kind": m["kind"], "subject": m["subject"]}
            if m["key"]:
                args["key"] = m["key"]
            _, ms = mcp.tool("remember", args)
            write_ms.append(ms)
            line = f"- {m['text']}"
            md_lines.append(line)
            if m["slot"]:
                compact.pop(m["slot"], None)
                compact[m["slot"]] = line
            else:
                compact[(s, len(compact))] = line
        per_session.append({"session": s, "start_tokens": start, "recall_tokens": q_tokens,
                            "questions": len(questions), "md_lines": len(md_lines)})
    mcp.close()
    summary = {}
    for name, st in totals.items():
        asked = max(st["asked"], 1)
        summary[name] = {"answerable": st["answerable"] / asked, "top1": st["top1"] / asked,
                         "stale_exposure": st["stale"] / asked, "asked": st["asked"]}
    last = per_session[-1]
    return {
        "sessions": sessions, "learn_per_session": learn, "ask_per_session": ask, "subjects": len(world.subjects),
        "facts": len(world.truth), "memories_written": len(md_lines), "schema_tokens": schema_tokens,
        "per_session": per_session, "summary": summary,
        "final_start_tokens": last["start_tokens"],
        "write_ms_p50": statistics.median(write_ms), "recall_ms_p50": statistics.median(recall_ms),
        "store_bytes": sum(f.stat().st_size for f in tmp.iterdir()),
    }


# ---- scale ----

def crockford_id(i: int) -> str:
    alphabet = "0123456789abcdefghjkmnpqrstvwxyz"
    chars = []
    for _ in range(16):
        chars.append(alphabet[i % 32])
        i //= 32
    return "m" + "".join(reversed(chars))


def vocabulary(rng: random.Random, size: int) -> list[str]:
    syllables = ["ka", "lo", "mi", "ren", "sa", "tor", "vel", "dun", "pri", "ax", "zen", "qu", "bri", "ost", "fal",
                 "gen", "hol", "jun", "nex", "pol", "rak", "sil", "tem", "ul", "vor", "wex", "yar", "zul"]
    words: set[str] = set()
    while len(words) < size:
        words.add("".join(rng.choice(syllables) for _ in range(rng.randint(2, 4))))
    return sorted(words)


def synth_rows(n: int, seed: int, samples: list):
    """Memories with a Zipf-like vocabulary, like real notes: a few words are
    everywhere, most are rare. Keeps a sample of rows to ask about."""
    rng = random.Random(seed)
    words = vocabulary(rng, 20_000)
    rng.shuffle(words)
    # Zipf past the ~100 most common words, which in real text are the
    # stopwords queries drop.
    cum, total = [], 0.0
    for r in range(len(words)):
        total += 1 / (r + 100) ** 1.05
        cum.append(total)
    base = 1_700_000_000
    every = max(n // 400, 1)
    for i in range(n):
        subject = f"{rng.choice(NAMES)}-{rng.choice(NAMES)}-{i % 5000}"
        key = f"k{i}"
        text = " ".join(rng.choices(words, cum_weights=cum, k=rng.randint(6, 14)))
        if i % every == 0:
            samples.append((subject, text))
        ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(base + i * 30))
        yield {"id": crockford_id(i), "ns": "default", "kind": rng.choice(["fact", "lesson", "note", "decision"]),
               "subject": subject, "key": key, "text": f"{subject} {text}", "importance": rng.randint(1, 5),
               "confidence": 1.0, "created_at": ts, "updated_at": ts, "valid_from": ts}


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))]


def run_scale(binary: str, sizes: list[int], seed: int) -> list[dict]:
    out = []
    for n in sizes:
        tmp = Path(tempfile.mkdtemp(prefix=f"levscale{n}-"))
        dump = tmp / "rows.jsonl"
        md_sample_tokens = 0
        samples: list = []
        with dump.open("w") as f:
            for i, row in enumerate(synth_rows(n, seed, samples)):
                f.write(json.dumps(row) + "\n")
                if i < 5000:
                    md_sample_tokens += tok(f"- {row['text']}\n")
        md_tokens = int(md_sample_tokens / min(n, 5000) * n)
        db = tmp / "memory.db"
        t = time.perf_counter()
        subprocess.run([binary, f"--memory={db}", "memory", "import", str(dump)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        import_s = time.perf_counter() - t
        dump.unlink()
        rng = random.Random(seed + n)

        def question() -> str:
            # Like an agent's recall: a subject and a few words of something stored.
            subject, text = rng.choice(samples)
            return " ".join([subject] + rng.sample(text.split(), 2))

        mcp = Mcp(binary, db)
        recall, briefing, write = [], [], []
        for _ in range(200):
            recall.append(mcp.tool("recall", {"query": question()})[1])
        for _ in range(30):
            briefing.append(mcp.tool("recall", {})[1])
        for i in range(200):
            write.append(mcp.tool("remember", {"text": f"bench write {i} {rng.choice(VALUES)} {rng.choice(NAMES)}",
                                               "subject": f"bench-{i % 20}", "key": f"w{i}"})[1])
        mcp.close()
        cold = []
        for _ in range(20):
            t = time.perf_counter()
            subprocess.run([binary, f"--memory={db}", "memory", "recall", question()], check=True,
                           stdout=subprocess.DEVNULL)
            cold.append((time.perf_counter() - t) * 1000)
        size = sum(f.stat().st_size for f in tmp.iterdir())
        out.append({"memories": n, "import_seconds": import_s, "store_bytes": size, "markdown_tokens": md_tokens,
                    "recall_p50_ms": pct(recall, 0.5), "recall_p95_ms": pct(recall, 0.95),
                    "briefing_p50_ms": pct(briefing, 0.5), "remember_p50_ms": pct(write, 0.5),
                    "remember_p95_ms": pct(write, 0.95), "cli_recall_p50_ms": pct(cold, 0.5)})
        print(f"scale {n:>9,}: recall p50 {out[-1]['recall_p50_ms']:.2f} ms, remember p50 "
              f"{out[-1]['remember_p50_ms']:.2f} ms, import {import_s:.1f}s, {size / 1e6:.0f} MB", flush=True)
        for f in tmp.iterdir():
            f.unlink()
        tmp.rmdir()
    return out


def cpu_name() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or platform.machine()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(ROOT / "target" / "release" / "leviathan"))
    ap.add_argument("--sessions", type=int, default=300)
    ap.add_argument("--learn", type=int, default=8)
    ap.add_argument("--ask", type=int, default=5)
    ap.add_argument("--sizes", default="10000,100000,1000000")
    ap.add_argument("--seed", type=int, default=7)
    args = ap.parse_args()
    version = subprocess.run([args.bin, "--version"], capture_output=True, text=True, check=True).stdout.strip()
    t = time.time()
    sessions = run_sessions(args.bin, args.sessions, args.learn, args.ask, args.seed)
    s = sessions["summary"]
    print(f"sessions: leviathan answerable {s['leviathan']['answerable']:.1%} (top1 {s['leviathan']['top1']:.1%}), "
          f"stale {s['leviathan']['stale_exposure']:.1%}; md_full stale {s['md_full']['stale_exposure']:.1%}; "
          f"md_200 answerable {s['md_200']['answerable']:.1%}", flush=True)
    scale = run_scale(args.bin, [int(x) for x in args.sizes.split(",")], args.seed)
    report = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tokenizer": "o200k_base",
        "environment": {"leviathan": version, "cpu": cpu_name(), "cores": os.cpu_count()},
        "sessions": sessions, "scale": scale, "seconds": time.time() - t,
    }
    RESULTS.mkdir(parents=True, exist_ok=True)
    (RESULTS / "memory.json").write_text(json.dumps(report, indent=1))
    print(f"wrote {RESULTS / 'memory.json'}")


if __name__ == "__main__":
    main()
