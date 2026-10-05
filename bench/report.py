#!/usr/bin/env python3
"""Render benchmark charts (docs/assets/*.png) and bench/results/SUMMARY.md
from bench/results/results.json. Requires matplotlib."""

from __future__ import annotations

import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import FuncFormatter, LogLocator, PercentFormatter  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench" / "results"
ASSETS = ROOT / "docs" / "assets"

INK = "#1F2933"
MUTED = "#6B7280"
GRID = "#E5E7EB"
COLORS = {
    "leviathan": "#0B7A75",
    "grep_keywords": "#F4A261",
    "grep_group": "#E76F51",
    "read_all": "#9CA3AF",
}
LABELS = {
    "leviathan": "Leviathan search",
    "grep_keywords": "grep entity + question words",
    "grep_group": "grep entity's full history",
    "read_all": "read the whole dataset",
}

plt.rcParams.update({
    "font.family": "DejaVu Sans",
    "font.size": 12,
    "axes.edgecolor": GRID,
    "axes.labelcolor": INK,
    "axes.titlecolor": INK,
    "xtick.color": MUTED,
    "ytick.color": MUTED,
    "axes.spines.top": False,
    "axes.spines.right": False,
    "axes.grid": True,
    "grid.color": GRID,
    "grid.linewidth": 0.8,
    "axes.axisbelow": True,
    "figure.facecolor": "white",
    "savefig.facecolor": "white",
    "legend.frameon": False,
})


def human(n: float) -> str:
    n = float(n)
    for unit, div in (("B", 1e9), ("M", 1e6), ("K", 1e3)):
        if abs(n) >= div:
            v = n / div
            return f"{v:.0f}{unit}" if v >= 100 else f"{v:.1f}{unit}".replace(".0" + unit, unit)
    return f"{n:.0f}"


def headline(fig, title: str, subtitle: str):
    fig.text(0.04, 0.955, title, fontsize=21, fontweight="bold", color=INK, va="top")
    fig.text(0.04, 0.885, subtitle, fontsize=12, color=MUTED, va="top")


def footer(fig, report: dict, extra: str = ""):
    env = report["environment"]
    text = (f"{env['leviathan']} · {env['ripgrep']} · tokenizer {report['tokenizer']} · "
            f"{env['cpu']} · synthetic data, reproducible: python3 bench/run_bench.py")
    fig.text(0.04, 0.02, (extra + "\n" if extra else "") + text, fontsize=8.5, color=MUTED, va="bottom")


def series(report, method, key):
    return [s["summary"][method][key] for s in report["scales"]]


def hero(report):
    big = report["scales"][-1]
    lev = big["summary"]["leviathan"]
    values = {
        "leviathan": lev["tokens_median"],
        "grep_keywords": big["summary"]["grep_keywords"]["tokens_median"],
        "grep_group": big["summary"]["grep_group"]["tokens_median"],
        "read_all": big["corpus_tokens"],
    }
    order = ["read_all", "grep_group", "grep_keywords", "leviathan"]
    fig, ax = plt.subplots(figsize=(13, 6.6))
    fig.subplots_adjust(left=0.26, right=0.83, top=0.76, bottom=0.17)
    ys = range(len(order))
    bars = ax.barh(ys, [values[m] for m in order], color=[COLORS[m] for m in order], height=0.62)
    ax.set_xscale("log")
    ax.set_xlim(100, values["read_all"] * 3)
    ax.set_yticks(list(ys), [LABELS[m] for m in order], fontsize=13, color=INK)
    ax.tick_params(axis="y", length=0)
    ax.grid(axis="y", visible=False)
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.set_xlabel("median tokens put into the agent's context per question (log scale)", color=MUTED)
    ax.axvline(report["context_window"], color=INK, lw=1, ls=(0, (4, 3)), alpha=0.6)
    ax.text(report["context_window"] * 1.08, len(order) - 0.45, "200K-token\ncontext window",
            fontsize=9.5, color=MUTED, va="top")
    for bar, m in zip(bars, order):
        v = values[m]
        y = bar.get_y() + bar.get_height() / 2
        if m == "read_all":
            ax.text(v / 1.25, y, f"{human(v)} tokens · does not fit any context window", va="center", ha="right",
                    fontsize=12.5, color="white", fontweight="bold")
        else:
            ax.text(v * 1.15, y, f"{human(v)} tokens", va="center", fontsize=12.5, color=INK,
                    fontweight="bold" if m == "leviathan" else "normal")
        if m != "leviathan":
            ratio = v / values["leviathan"]
            ax.text(1.02, bar.get_y() + bar.get_height() / 2,
                    f"{ratio:,.0f}× more" if ratio < 1e5 else f"{human(ratio)}× more",
                    transform=ax.get_yaxis_transform(), va="center", fontsize=12.5, color=COLORS[m],
                    fontweight="bold")
    factor = values["grep_keywords"] / values["leviathan"]
    headline(fig, f"Same question, {factor:,.0f}× fewer tokens than the best grep",
             f"Example dataset: {big['records']:,} synthetic maintenance records on {big['machines']:,} machines · "
             f"{big['queries']} gold-labeled questions, each about one machine")
    fig.text(0.04, 0.80,
             f"Leviathan returns a relevant record in its top 5 for {lev['hit5']:.1%} of questions "
             f"({lev['hit1']:.1%} at rank 1) in {lev['latency_p50_ms']:.0f} ms median, one call.",
             fontsize=12, color=COLORS["leviathan"], fontweight="bold", va="top")
    footer(fig, report, "Entity = the machine asked about. Grep baselines are handed its exact key even when the question used its name. "
                        "Medians; see docs/BENCHMARKS.md for means, p90 and methodology.")
    fig.savefig(ASSETS / "hero.png", dpi=180)
    plt.close(fig)


def scaling(report):
    xs = [s["records"] for s in report["scales"]]
    fig, ax = plt.subplots(figsize=(12, 6.6))
    fig.subplots_adjust(left=0.09, right=0.8, top=0.8, bottom=0.17)
    data = {m: series(report, m, "tokens_median") for m in ("leviathan", "grep_keywords", "grep_group")}
    data["read_all"] = [s["corpus_tokens"] for s in report["scales"]]
    nudge = {"grep_group": 13, "grep_keywords": -13}
    for m in ("read_all", "grep_group", "grep_keywords", "leviathan"):
        ax.plot(xs, data[m], marker="o", ms=7, lw=3 if m == "leviathan" else 2.2, color=COLORS[m], label=LABELS[m])
        ax.annotate(f"{LABELS[m]}\n{human(data[m][-1])}", (xs[-1], data[m][-1]), xytext=(12, nudge.get(m, 0)),
                    textcoords="offset points", va="center", fontsize=10.5, color=COLORS[m], fontweight="bold")
    ax.axhspan(report["context_window"], max(data["read_all"]) * 10, color="#FEE2E2", alpha=0.45, lw=0)
    ax.text(xs[0], report["context_window"] * 1.4, "beyond a 200K-token context window", fontsize=10, color="#B91C1C")
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.set_ylim(100, max(data["read_all"]) * 4)
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.yaxis.set_major_locator(LogLocator(base=10))
    ax.set_xlabel("records in the dataset")
    ax.set_ylabel("median tokens per question")
    headline(fig, "Leviathan's cost stays flat as history grows",
             "Reading data directly scales with the size of the entity's history; a ranked index returns a "
             "bounded answer")
    footer(fig, report)
    fig.savefig(ASSETS / "scaling_tokens.png", dpi=180)
    plt.close(fig)


def history_scatter(report):
    rows = report["rows_largest_scale"]
    big = report["scales"][-1]
    fig, ax = plt.subplots(figsize=(12, 6.6))
    fig.subplots_adjust(left=0.09, right=0.97, top=0.8, bottom=0.17)
    xs = [r["group_records"] for r in rows]
    for m, size, alpha in (("grep_group", 26, 0.55), ("grep_keywords", 26, 0.6), ("leviathan", 30, 0.9)):
        ax.scatter(xs, [r[m]["tokens"] for r in rows], s=size, alpha=alpha, color=COLORS[m], label=LABELS[m],
                   edgecolors="white", linewidths=0.4)
    ax.axhline(report["context_window"], color=INK, lw=1, ls=(0, (4, 3)), alpha=0.6)
    ax.text(max(xs), report["context_window"] / 1.3, "200K-token context window", fontsize=9.5, color=MUTED,
            ha="right", va="top")
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.set_xlabel("records about the entity being asked about")
    ax.set_ylabel("tokens put into context")
    ax.legend(loc="upper left", fontsize=11)
    headline(fig, "Every question, one dot",
             f"{big['queries']} questions over {big['records']:,} records: the longer an entity's history, the more "
             "a direct read costs; Leviathan does not care")
    footer(fig, report)
    fig.savefig(ASSETS / "history_scatter.png", dpi=180)
    plt.close(fig)


def accuracy(report):
    xs = [s["records"] for s in report["scales"]]
    fig, ax = plt.subplots(figsize=(12, 6.6))
    fig.subplots_adjust(left=0.09, right=0.84, top=0.8, bottom=0.3)
    lines = [
        ("leviathan", "hit5", "-", "Leviathan: relevant record in top 5", "top 5"),
        ("leviathan", "hit1", (0, (1, 1.5)), "Leviathan: relevant record at rank 1", "rank 1"),
        ("grep_keywords", "gold_capped", "-", "grep + words: relevant record inside a 30K-char tool output", "grep + words"),
        ("grep_group", "gold_capped", "-", "grep history: relevant record inside a 30K-char tool output", "grep history"),
    ]
    ends = []
    for m, key, style, label, tag in lines:
        ys = series(report, m, key)
        ax.plot(xs, ys, ls=style, marker="o", ms=6, lw=3 if m == "leviathan" else 2.2, color=COLORS[m], label=label)
        ends.append([ys[-1], ys[-1], f"{ys[-1]:.1%} {tag}", COLORS[m]])
    ends.sort(key=lambda e: -e[0])
    for above, below in zip(ends, ends[1:]):
        below[1] = min(below[1], above[1] - 0.045)
    for _, y, text, color in ends:
        ax.text(xs[-1] * 1.12, y, text, va="center", color=color, fontweight="bold", fontsize=10.5)
    ax.set_xscale("log")
    ax.set_xlim(xs[0] / 1.25, xs[-1] * 1.25)
    ax.set_ylim(0, 1.05)
    ax.yaxis.set_major_formatter(PercentFormatter(1.0))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.set_xlabel("records in the dataset")
    ax.set_ylabel("questions answered")
    ax.legend(loc="upper center", bbox_to_anchor=(0.5, -0.14), ncol=2, fontsize=10.5)
    headline(fig, "As accurate as grep, without the haystack",
             "Coding-agent shells cap tool output (30,000 characters is common); grep history loses the answer "
             "past the cap as entities grow")
    uncapped = [s["summary"][m]["gold"] for s in report["scales"] for m in ("grep_keywords", "grep_group")]
    footer(fig, report, f"Without the cap, the grep baselines contain a relevant record {min(uncapped):.1%}–"
                        f"{max(uncapped):.0%} of the time, buried in the entity's full history.")
    fig.savefig(ASSETS / "accuracy.png", dpi=180)
    plt.close(fig)


def latency(report):
    xs = [s["records"] for s in report["scales"]]
    fig, ax = plt.subplots(figsize=(12, 6.6))
    fig.subplots_adjust(left=0.09, right=0.8, top=0.8, bottom=0.17)
    for m in ("grep_group", "grep_keywords", "leviathan"):
        p50 = series(report, m, "latency_p50_ms")
        p95 = series(report, m, "latency_p95_ms")
        ax.plot(xs, p50, marker="o", lw=3 if m == "leviathan" else 2.2, color=COLORS[m], label=f"{LABELS[m]} (p50)")
        ax.fill_between(xs, p50, p95, color=COLORS[m], alpha=0.15, lw=0)
        ax.annotate(f"{p50[-1]:,.0f} ms", (xs[-1], p50[-1]), xytext=(10, {"grep_group": -9, "grep_keywords": 9}.get(m, 0)),
                    textcoords="offset points",
                    va="center", color=COLORS[m], fontweight="bold")
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
    ax.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:,.0f} ms" if v >= 1 else f"{v:.1f} ms"))
    ax.set_xlabel("records in the dataset")
    ax.set_ylabel("wall-clock per question, process start included")
    ax.legend(loc="upper left", fontsize=10.5)
    headline(fig, "Milliseconds, at any size",
             "Median wall-clock per question (band to p95); ripgrep is measured on a warm page cache, its best case")
    footer(fig, report)
    fig.savefig(ASSETS / "latency.png", dpi=180)
    plt.close(fig)


def indexing(report):
    xs = [s["records"] for s in report["scales"]]
    fig, (a1, a2) = plt.subplots(1, 2, figsize=(13, 6.2))
    fig.subplots_adjust(left=0.07, right=0.97, top=0.78, bottom=0.18, wspace=0.28)
    secs = [s["index"]["seconds"] for s in report["scales"]]
    a1.plot(xs, secs, marker="o", lw=3, color=COLORS["leviathan"])
    for x, y, s in zip(xs, secs, report["scales"]):
        a1.annotate(f"{s['index']['docs_per_second'] / 1000:,.0f}K rec/s", (x, y), xytext=(0, 9),
                    textcoords="offset points", ha="center", fontsize=9, color=MUTED)
    a1.set_xscale("log")
    a1.set_yscale("log")
    a1.set_title("full build time", loc="left", fontsize=13, color=INK)
    a1.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}s"))
    corpus = [s["corpus_bytes"] / 1e9 for s in report["scales"]]
    index = [s["index"]["bytes"] / 1e9 for s in report["scales"]]
    a2.plot(xs, corpus, marker="o", lw=2.2, color=COLORS["read_all"], label="JSONL source")
    a2.plot(xs, index, marker="o", lw=3, color=COLORS["leviathan"], label="Leviathan index (records + full-text)")
    a2.set_xscale("log")
    a2.set_yscale("log")
    a2.set_title("on-disk size", loc="left", fontsize=13, color=INK)
    a2.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g} GB"))
    a2.legend(loc="upper left", fontsize=10.5)
    for ax in (a1, a2):
        ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: human(v)))
        ax.set_xlabel("records")
    big = report["scales"][-1]
    headline(fig, f"Index {human(big['records'])} records in {big['index']['seconds']:.0f} seconds",
             "Single-threaded streaming build into one SQLite/FTS5 file, swapped in atomically; "
             "incremental upserts after that")
    footer(fig, report)
    fig.savefig(ASSETS / "indexing.png", dpi=180)
    plt.close(fig)


def markdown(report) -> str:
    out = ["# Benchmark summary", "",
           f"Generated {report['generated_at']} · tokenizer `{report['tokenizer']}` · "
           f"{report['environment']['leviathan']} · {report['environment']['ripgrep']} · "
           f"{report['environment']['cpu']} ({report['environment']['cores']} threads)", "",
           f"MCP tool schemas: {report['mcp_schema_tokens']:,} tokens per session (CLI: 0).", "",
           "| records | dataset | Leviathan median tok | grep+words median tok | grep history median tok | "
           "read all tok | Leviathan hit@1 / hit@5 / MRR@5 | grep+words answer in 30K-char cap | "
           "grep history answer in cap | Leviathan p50 ms | grep history p50 ms | build s | index |",
           "|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|"]
    for s in report["scales"]:
        lev, kw, gm = (s["summary"][m] for m in ("leviathan", "grep_keywords", "grep_group"))
        out.append(
            f"| {s['records']:,} | {s['corpus_bytes'] / 1e6:,.0f} MB | **{lev['tokens_median']:,.0f}** | "
            f"{kw['tokens_median']:,.0f} | {gm['tokens_median']:,.0f} | {s['corpus_tokens']:,} | "
            f"{lev['hit1']:.1%} / {lev['hit5']:.1%} / {lev['mrr5']:.3f} | {kw['gold_capped']:.1%} | "
            f"{gm['gold_capped']:.1%} | {lev['latency_p50_ms']:.1f} | {gm['latency_p50_ms']:.1f} | "
            f"{s['index']['seconds']:.1f} | {s['index']['bytes'] / 1e6:,.0f} MB |")
    out += ["", "Means and tails (tokens):", "",
            "| records | Leviathan mean / p90 / max | grep+words mean / p90 / max | grep history mean / p90 / max | "
            "grep history fits 200K |", "|---:|---|---|---|---:|"]
    for s in report["scales"]:
        lev, kw, gm = (s["summary"][m] for m in ("leviathan", "grep_keywords", "grep_group"))
        fmt = lambda d: f"{d['tokens_mean']:,.0f} / {d['tokens_p90']:,.0f} / {d['tokens_max']:,.0f}"  # noqa: E731
        out.append(f"| {s['records']:,} | {fmt(lev)} | {fmt(kw)} | {fmt(gm)} | {gm['fits_context']:.0%} |")
    return "\n".join(out) + "\n"


def main():
    report = json.loads((RESULTS / "results.json").read_text())
    ASSETS.mkdir(parents=True, exist_ok=True)
    for render in (hero, scaling, history_scatter, accuracy, latency, indexing):
        render(report)
    (RESULTS / "SUMMARY.md").write_text(markdown(report))
    print(f"wrote {ASSETS}/*.png and {RESULTS / 'SUMMARY.md'}")


if __name__ == "__main__":
    main()
