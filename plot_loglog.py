#!/usr/bin/env python3
"""Draws the log-log figure embedded in REPORT.md.

    uv run --with matplotlib plot_loglog.py

matplotlib is allowed for the report only (the brief's "Allowed" list), and
it is the only dependency. Nothing here touches the engine: the numbers below
are transcribed from REPORT.md's tables, which are the stdout of

    radb-study study --sizes 1000,2000,4000,8000,16000,32000,64000 --matches 5 --seed 7

Re-run the sweep first, then update the three lists below, then run this.
The script prints the least-squares fit it drew, so the slope quoted in
REPORT.md question 2 can be checked against the figure rather than guessed.

Output: report_loglog.png, 200 dpi, in the repository root.
"""
import math
import pathlib

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import LogLocator, NullFormatter

# The measurements, exactly as REPORT.md tabulates them
N = [1000, 2000, 4000, 8000, 16000, 32000, 64000]
JOIN = [0.007, 0.025, 0.106, 0.469, 1.879, 7.642, 30.627]
SELECT = [0.0001, 0.0001, 0.0003, 0.0006, 0.0015, 0.0032, 0.0070]
PROJECT = [0.0001, 0.0001, 0.0002, 0.0005, 0.0012, 0.0025, 0.0065]

JOIN_C, SELECT_C, PROJECT_C = "#1f77b4", "#2ca02c", "#ff7f0e"
OUTPUT_PATH = pathlib.Path(__file__).resolve().parent / "report_loglog.png"

GRID = dict(which="major", color="#e6e6e6", linewidth=1)
FRAME = dict(color="#000000", linewidth=1.0)


def fit(values):
    """Least-squares fit of log10(t) against log10(n). Returns (slope, intercept)."""
    xs = [math.log10(n) for n in N]
    ys = [math.log10(t) for t in values]
    k = len(xs)
    mx, my = sum(xs) / k, sum(ys) / k
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sum((x - mx) ** 2 for x in xs)
    return slope, my - slope * mx


def style(ax, title):
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.set_xlim(8e2, 1.2e5)
    ax.set_ylim(3e-5, 1e2)
    ax.xaxis.set_major_locator(LogLocator(base=10))
    ax.xaxis.set_minor_locator(LogLocator(base=10, subs=tuple(range(2, 10))))
    ax.xaxis.set_minor_formatter(NullFormatter())
    ax.grid(True, **GRID)
    ax.set_axisbelow(True)
    for side in ("top", "right"):
        ax.spines[side].set_visible(False)
    for side in ("left", "bottom"):
        ax.spines[side].set(**FRAME)
    ax.set_title(title, fontsize=12, pad=12)
    ax.set_xlabel("n  (tuples per relation)", fontsize=10)
    ax.set_ylabel("wall time (s)", fontsize=10)


def fitted(ax, values, slope, intercept, colour, marker, label, linestyle):
    """The fitted line, drawn from the data's own n range so it cannot run off.

    Markers differ per series because select and project are both 0.0001 s at
    n = 1000 and n = 2000, so their points land on the same pixels; identical
    markers would hide one series behind the other.
    """
    xs = [min(N) * (max(N) / min(N)) ** (i / 100) for i in range(101)]
    ax.plot(
        xs,
        [10 ** (slope * math.log10(x) + intercept) for x in xs],
        color=colour,
        linewidth=1.4,
        linestyle=linestyle,
        zorder=2,
    )
    ax.plot(N, values, marker, color=colour, markersize=5, linestyle="none",
            label=label, zorder=3)


s_join, c_join = fit(JOIN)
s_sel, c_sel = fit(SELECT)
s_proj, c_proj = fit(PROJECT)

fig, ax = plt.subplots(figsize=(8.4, 5.6), constrained_layout=True)

style(ax, "Join, select and project at the same sizes")
fitted(ax, JOIN, s_join, c_join, JOIN_C, "o", f"join  (slope {s_join:.2f})", "--")
fitted(ax, SELECT, s_sel, c_sel, SELECT_C, "s", f"select  (slope {s_sel:.2f})", "--")
fitted(ax, PROJECT, s_proj, c_proj, PROJECT_C, "^", f"project  (slope {s_proj:.2f})", "--")
legend = ax.legend(loc="upper left", fontsize=10, framealpha=0.95, edgecolor="#cccccc")
legend.get_frame().set_linewidth(1)

# Opaque, so the figure is legible on a dark background too.
fig.savefig(OUTPUT_PATH, dpi=200, facecolor="white")
print(f"wrote {OUTPUT_PATH}")
print(f"join    log10(t) = {c_join:.2f} + {s_join:.3f}*log10(n)")
print(f"select  slope {s_sel:.3f}")
print(f"project slope {s_proj:.3f}")
