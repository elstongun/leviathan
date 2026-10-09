#!/usr/bin/env python3
"""Render the memory benchmark: docs/assets/memory-*-dark.png and
bench/results/MEMORY_SUMMARY.md from bench/results/memory.json."""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import report as base  # noqa: E402  (shared fonts, colors and helpers)

plt = base.plt
ROOT = base.ROOT
ASSETS = base.ASSETS
RESULTS = base.RESULTS
BG, INK, MUTED = base.BG, base.INK, base.MUTED
COLORS = {"leviathan": "#2DD4BF", "md_full": "#F0705A", "md_compacted": "#F4A261", "md_200": "#6E7681"}
LABELS = {
    "leviathan": "Leviathan memory",
    "md_full": "MEMORY.md, whole file",
    "md_compacted": "MEMORY.md, ideal compaction",
    "md_200": "MEMORY.md, first 200 lines",
}
human = base.human


def footer(fig, report, extra=""):
    env = report["environment"]
    text = (f"{env['leviathan']} · tokenizer {report['tokenizer']} · {env['cpu']} · synthetic workload, "
            "reproducible: python3 bench/memory/run_memory_bench.py")
    fig.text(0.04, 0.02, (extra + "\n" if extra else "") + text, fontsize=7.5, color=MUTED, va="bottom")


def tokens_over_sessions(report):
    s = report["sessions"]
    rows = s["per_session"]
    xs = [r["session"] for r in rows]
    fig, ax = plt.subplots(figsize=(12, 6.6))
    fig.subplots_adjust(left=0.09, right=0.78, top=0.78, bottom=0.17)
    for m in ("md_full", "md_compacted", "md_200", "leviathan"):
        ys = [r["start_tokens"][m] for r in rows]
        ax.plot(xs, ys, lw=3 if m == "leviathan" else 2.2, color=COLORS[m])
        ax.annotate(f"{LABELS[m]}\n{human(ys[-1])} tokens", (xs[-1], ys[-1]), xytext=(10, 0),
                    textcoords="offset points", va="center", fontsize=10, color=COLORS[m], fontweight="bold")
    ax.set_yscale("log")
    ax.yaxis.set_major_formatter(base.FuncFormatter(lambda v, _: human(v)))
    ax.set_xlabel("session")
    ax.set_ylabel("tokens loaded at session start")
    per_q = sum(r["recall_tokens"] for r in rows) / max(sum(r["questions"] for r in rows), 1)
    base.headline(fig, "Memory that does not grow with every session",
                  f"{s['sessions']} sessions · {s['memories_written']:,} memories written · {s['facts']:,} facts, "
                  f"many corrected over time · each recall adds ~{per_q:,.0f} tokens")
    footer(fig, report, "Leviathan loads a budgeted briefing and recalls on demand; markdown files are loaded "
                        "whole (or cut at 200 lines). Ideal compaction keeps only the newest line per fact.")
    fig.savefig(ASSETS / "memory-tokens-dark.png", dpi=180)
    plt.close(fig)


def quality(report):
    s = report["sessions"]["summary"]
    order = ["md_200", "md_full", "md_compacted", "leviathan"]
    fig, (a1, a2) = plt.subplots(1, 2, figsize=(13, 6.2), sharey=True)
    fig.subplots_adjust(left=0.22, right=0.96, top=0.76, bottom=0.17, wspace=0.12)
    ys = range(len(order))
    for ax, key, title in ((a1, "answerable", "questions whose answer is in context"),
                           (a2, "stale_exposure", "context also holds an outdated value")):
        vals = [s[m][key] for m in order]
        ax.barh(ys, vals, color=[COLORS[m] for m in order], height=0.6)
        for y, v in zip(ys, vals):
            ax.text(v + 0.02, y, f"{v:.0%}", va="center", fontsize=12, color=INK,
                    fontweight="bold" if order[y] == "leviathan" else "normal")
        ax.set_xlim(0, 1.15)
        ax.xaxis.set_major_formatter(base.PercentFormatter(1.0))
        ax.set_title(title, loc="left", fontsize=12.5, color=INK)
        ax.grid(axis="y", visible=False)
        ax.tick_params(axis="y", length=0)
    a1.set_yticks(list(ys), [LABELS[m] for m in order], fontsize=12, color=INK)
    lev = s["leviathan"]
    base.headline(fig, "The current answer, without the stale ones",
                  f"{lev['asked']:,} questions about facts learned in earlier sessions · Leviathan: answer ranked "
                  f"first {lev['top1']:.0%} of the time")
    footer(fig, report, "Keyed memories replace the old value (history stays queryable); a markdown file keeps "
                        "every version, and a 200-line cut drops the newest.")
    fig.savefig(ASSETS / "memory-quality-dark.png", dpi=180)
    plt.close(fig)


def scale(report):
    rows = report["scale"]
    xs = [r["memories"] for r in rows]
    fig, (a1, a2) = plt.subplots(1, 2, figsize=(13, 6.2))
    fig.subplots_adjust(left=0.07, right=0.97, top=0.76, bottom=0.18, wspace=0.28)
    for key, label, color in (("recall_p50_ms", "recall (p50)", COLORS["leviathan"]),
                              ("briefing_p50_ms", "briefing (p50)", "#7DD3FC"),
                              ("remember_p50_ms", "remember (p50)", "#C4B5FD")):
        a1.plot(xs, [r[key] for r in rows], marker="o", lw=2.6, color=color, label=label)
    a1.set_xscale("log")
    a1.set_yscale("log")
    a1.yaxis.set_major_formatter(base.FuncFormatter(lambda v, _: f"{v:g} ms"))
    a1.set_title("latency in one MCP session", loc="left", fontsize=13, color=INK)
    a1.legend(loc="lower right", bbox_to_anchor=(1, 0.1), fontsize=10.5)
    budget = 800
    a2.plot(xs, [r["markdown_tokens"] for r in rows], marker="o", lw=2.2, color=COLORS["md_full"],
            label="markdown file of the same memories")
    a2.plot(xs, [budget] * len(xs), marker="o", lw=3, color=COLORS["leviathan"], label="Leviathan recall budget")
    a2.axhline(200_000, color=INK, lw=1, ls=(0, (4, 3)), alpha=0.6)
    a2.text(xs[0], 200_000 * 1.3, "200K-token context window", fontsize=9.5, color=MUTED)
    a2.set_xscale("log")
    a2.set_yscale("log")
    a2.yaxis.set_major_formatter(base.FuncFormatter(lambda v, _: human(v)))
    a2.set_title("tokens to put it in context", loc="left", fontsize=13, color=INK)
    a2.legend(loc="lower right", bbox_to_anchor=(1, 0.1), fontsize=10.5)
    for ax in (a1, a2):
        ax.xaxis.set_major_formatter(base.FuncFormatter(lambda v, _: human(v)))
        ax.set_xlabel("memories in the store")
    big = rows[-1]
    base.headline(fig, f"{human(big['memories'])} memories, {big['recall_p50_ms']:.0f} ms recall",
                  "One SQLite file (WAL, FTS5); writes stay sub-millisecond as the store grows")
    footer(fig, report)
    fig.savefig(ASSETS / "memory-scale-dark.png", dpi=180)
    plt.close(fig)


def markdown(report) -> str:
    s = report["sessions"]
    rows = s["per_session"]
    per_q = sum(r["recall_tokens"] for r in rows) / max(sum(r["questions"] for r in rows), 1)
    out = ["# Memory benchmark summary", "",
           f"Generated {report['generated_at']} · tokenizer `{report['tokenizer']}` · "
           f"{report['environment']['leviathan']} · {report['environment']['cpu']}", "",
           f"Sessions: {s['sessions']} · memories written: {s['memories_written']:,} · facts: {s['facts']:,} · "
           f"subjects: {s['subjects']} · memory tool schemas: {s['schema_tokens']:,} tokens per session.", "",
           "| strategy | tokens at the last session start | answer in context | answer ranked first | "
           "stale value in context |", "|---|---:|---:|---:|---:|"]
    for m in ("leviathan", "md_compacted", "md_full", "md_200"):
        q = s["summary"][m]
        out.append(f"| {LABELS[m]} | {s['final_start_tokens'][m]:,} | {q['answerable']:.1%} | {q['top1']:.1%} | "
                   f"{q['stale_exposure']:.1%} |")
    out += ["", f"Leviathan recall adds {per_q:,.0f} tokens per question on average "
                f"(remember p50 {s['write_ms_p50']:.2f} ms, recall p50 {s['recall_ms_p50']:.2f} ms).", "",
            "| memories | store | import | recall p50 / p95 | briefing p50 | remember p50 / p95 | "
            "CLI recall p50 (process start) | markdown file tokens |",
            "|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for r in report["scale"]:
        out.append(f"| {r['memories']:,} | {r['store_bytes'] / 1e6:,.0f} MB | {r['import_seconds']:.1f} s | "
                   f"{r['recall_p50_ms']:.1f} / {r['recall_p95_ms']:.1f} ms | {r['briefing_p50_ms']:.1f} ms | "
                   f"{r['remember_p50_ms']:.2f} / {r['remember_p95_ms']:.2f} ms | {r['cli_recall_p50_ms']:.0f} ms | "
                   f"{r['markdown_tokens']:,} |")
    return "\n".join(out) + "\n"


def main():
    report = json.loads((RESULTS / "memory.json").read_text())
    ASSETS.mkdir(parents=True, exist_ok=True)
    for render in (tokens_over_sessions, quality, scale):
        render(report)
    (RESULTS / "MEMORY_SUMMARY.md").write_text(markdown(report))
    print(f"wrote {ASSETS}/memory-*.png and {RESULTS / 'MEMORY_SUMMARY.md'}")


if __name__ == "__main__":
    main()
