#!/usr/bin/env python3
"""Draw the charts in docs/search-quality.md from runs.tsv.

    python3 docs/search-quality/charts.py

Writes each chart twice, NAME.svg for GitHub's light theme and NAME-dark.svg
for its dark theme, next to this file. Uses only the standard library.
"""

import csv
import os
from datetime import date

HERE = os.path.dirname(os.path.abspath(__file__))
WIDTH = 720
FONT = "-apple-system,BlinkMacSystemFont,'Segoe UI','Noto Sans',Helvetica,Arial,sans-serif"

# Inks follow GitHub's own; series and ramp steps are a validated palette
# (colorblind-safe pairs, at least 3:1 against each theme's page).
THEMES = {
    "light": {
        "page": "#ffffff",
        "ink": "#1f2328",
        "ink2": "#59636e",
        "muted": "#818b98",
        "grid": "#e6e9ed",
        "axis": "#b9c0c8",
        "series": ["#2a78d6", "#eb6834"],
        "base": "#a3abb5",
        # great, fine, poor, useless
        "grades": ["#104281", "#256abf", "#5598e7", "#86b6ef"],
    },
    "dark": {
        "page": "#0d1117",
        "ink": "#f0f6fc",
        "ink2": "#9198a1",
        "muted": "#7d8590",
        "grid": "#21262d",
        "axis": "#3d444d",
        "series": ["#3987e5", "#d95926"],
        "base": "#656c76",
        "grades": ["#9ec5f4", "#5598e7", "#256abf", "#184f95"],
    },
}


def load_runs():
    with open(os.path.join(HERE, "runs.tsv"), encoding="utf-8") as f:
        lines = [line for line in f if not line.startswith("#")]
    runs = []
    for row in csv.DictReader(lines, delimiter="\t"):
        run = {k: row[k] for k in ("run", "date", "build")}
        for k, v in row.items():
            if k not in run:
                run[k] = float(v)
        run["mean"] = run["grade_sum"] / 1000
        runs.append(run)
    return runs


def esc(text):
    return str(text).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text_width(text, size=12):
    """A rough width for laying out labels, in pixels."""
    return len(str(text)) * size * 0.56


def short_date(iso):
    d = date.fromisoformat(iso)
    return d.strftime("%b ") + str(d.day)


class Svg:
    def __init__(self, theme, height, title, desc):
        self.t = THEMES[theme]
        self.height = height
        self.parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" '
            f'viewBox="0 0 {WIDTH} {height}" role="img" font-family="{FONT}">',
            f"<title>{esc(title)}</title><desc>{esc(desc)}</desc>",
        ]

    def text(self, x, y, s, size=12, fill="ink", anchor="start", weight=400):
        color = self.t.get(fill, fill)
        self.parts.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{color}" '
            f'text-anchor="{anchor}" font-weight="{weight}">{esc(s)}</text>'
        )

    def line(self, x1, y1, x2, y2, stroke="grid", width=1):
        color = self.t.get(stroke, stroke)
        self.parts.append(
            f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" '
            f'stroke="{color}" stroke-width="{width}"/>'
        )

    def polyline(self, points, color, width=2):
        pts = " ".join(f"{x:.1f},{y:.1f}" for x, y in points)
        self.parts.append(
            f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="{width}" '
            'stroke-linejoin="round" stroke-linecap="round"/>'
        )

    def dot(self, x, y, color, r=4.5):
        # A ring in the page color keeps dots legible where they cross lines.
        self.parts.append(
            f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r}" fill="{color}" '
            f'stroke="{self.t["page"]}" stroke-width="2"/>'
        )

    def bar(self, x, y, w, h, color, round_right=False):
        if w <= 0:
            return
        if not round_right or w < 6:
            self.parts.append(
                f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h}" fill="{color}"/>'
            )
            return
        r = 4
        self.parts.append(
            f'<path d="M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r} '
            f'a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z" fill="{color}"/>'
        )

    def swatch(self, x, y, color, label):
        self.parts.append(
            f'<rect x="{x:.1f}" y="{y - 9:.1f}" width="10" height="10" rx="2" fill="{color}"/>'
        )
        self.text(x + 15, y, label, fill="ink2")
        return x + 15 + text_width(label) + 18

    def heading(self, title, subtitle):
        self.text(0, 18, title, size=15, weight=600)
        self.text(0, 38, subtitle, fill="ink2")

    def save(self, name, theme):
        self.parts.append("</svg>")
        suffix = "" if theme == "light" else "-dark"
        path = os.path.join(HERE, f"{name}{suffix}.svg")
        with open(path, "w", encoding="utf-8") as f:
            f.write("\n".join(self.parts) + "\n")


def run_axis(svg, runs, xs, bottom):
    """Run numbers, dates and builds under a chart."""
    for run, x in zip(runs, xs):
        svg.text(x, bottom + 18, f"Run {run['run']}", anchor="middle", fill="ink2")
        svg.text(x, bottom + 33, short_date(run["date"]), anchor="middle", fill="muted", size=11)


def y_grid(svg, left, right, ticks, y_of, fmt):
    for tick in ticks:
        y = y_of(tick)
        svg.line(left, y, right, y, stroke="grid")
        svg.text(left - 8, y + 4, fmt(tick), anchor="end", fill="muted", size=11)


def line_chart(name, theme, runs, series, ticks, fmt, title, subtitle, desc, goal=None):
    """series: (label, key or function, color index)."""
    height = 300
    svg = Svg(theme, height, title, desc)
    svg.heading(title, subtitle)
    left, right, top, bottom = 44, WIDTH - 150, 64, height - 46
    lo, hi = ticks[0], ticks[-1]

    def y_of(v):
        return bottom - (v - lo) / (hi - lo) * (bottom - top)

    xs = [left + 24 + i * (right - left - 48) / (len(runs) - 1) for i in range(len(runs))]
    y_grid(svg, left, right, ticks, y_of, fmt)
    svg.line(left, bottom, right, bottom, stroke="axis")
    if goal is not None:
        y = y_of(goal[0])
        svg.line(left, y, right, y, stroke="base", width=1.5)
        svg.text(right + 8, y + 4, goal[1], fill="muted", size=11)
    for label, value, color in series:
        color = svg.t["series"][color]
        values = [value(r) for r in runs]
        points = [(x, y_of(v)) for x, v in zip(xs, values)]
        svg.polyline(points, color)
        for x, y in points:
            svg.dot(x, y, color)
        first, last = points[0], points[-1]
        svg.text(first[0], first[1] - 12, fmt(values[0]), anchor="middle", fill="ink2", size=11)
        svg.text(last[0] + 12, last[1] + 4, f"{fmt(values[-1])}  {label}", weight=600)
    run_axis(svg, runs, xs, bottom)
    svg.save(name, theme)


def grades_chart(theme, runs):
    title = "How each search was graded"
    subtitle = "Share of the 1,000 searches by grade, run by run"
    desc = "Stacked bars per run: " + "; ".join(
        f"run {r['run']}: {int(r['great'])} great, {int(r['fine'])} fine, "
        f"{int(r['poor'])} poor, {int(r['useless'])} useless"
        for r in runs
    )
    row, bar_h = 34, 22
    top = 80
    height = top + row * len(runs) + 16
    svg = Svg(theme, height, title, desc)
    svg.heading(title, subtitle)
    keys = [("great", "Great (3)"), ("fine", "Fine (2)"), ("poor", "Poor (1)"), ("useless", "Useless (0)")]
    x = 0
    for (key, label), color in zip(keys, svg.t["grades"]):
        x = svg.swatch(x, 62, color, label)
    left, right = 104, WIDTH - 8
    for i, run in enumerate(runs):
        y = top + i * row
        svg.text(0, y + 15, f"Run {run['run']}", fill="ink2")
        svg.text(46, y + 15, short_date(run["date"]), fill="muted", size=11)
        x = left
        for j, ((key, _), color) in enumerate(zip(keys, svg.t["grades"])):
            share = run[key] / 1000
            w = share * (right - left)
            last = j == len(keys) - 1
            # A 2px gap in the page color between segments.
            svg.bar(x, y, w - (0 if last else 2), bar_h, color, round_right=last)
            label = f"{share * 100:.0f}%"
            if text_width(label, 11) + 12 < w:
                ink = "#ffffff" if luminance(color) < 0.33 else THEMES["light"]["ink"]
                svg.text(x + (w - 2) / 2, y + 15, label, size=11, fill=ink, anchor="middle", weight=600)
            x += w
    svg.save("grades", theme)


def lanes_chart(theme, runs):
    first, last = runs[0], runs[-1]
    title = "Mean grade by kind of search"
    subtitle = f"Run {first['run']} ({short_date(first['date'])}) against run {last['run']} ({short_date(last['date'])}); grades go from 0 (useless) to 3 (great)"
    lanes = [
        ("All 1,000 searches", lambda r: r["mean"]),
        ("Popular searches (184)", lambda r: r["popular"]),
        ("Test-set searches (226)", lambda r: r["known"]),
        ("AI agent searches (240)", lambda r: r["agent"]),
        ("Everyday searches (350)", lambda r: r["everyday"]),
    ]
    desc = "; ".join(f"{name}: {f(first):.2f} to {f(last):.2f}" for name, f in lanes)
    row = 36
    top = 92
    height = top + row * len(lanes) + 30
    svg = Svg(theme, height, title, desc)
    svg.heading(title, subtitle)
    x = svg.swatch(0, 62, svg.t["base"], f"Run {first['run']}")
    svg.swatch(x, 62, svg.t["series"][0], f"Run {last['run']}")
    left, right = 190, WIDTH - 40
    lo, hi = 1.0, 3.0

    def x_of(v):
        return left + (v - lo) / (hi - lo) * (right - left)

    bottom = top + row * len(lanes) - 10
    for tick in (1.0, 1.5, 2.0, 2.5, 3.0):
        svg.line(x_of(tick), top - 12, x_of(tick), bottom, stroke="grid")
        svg.text(x_of(tick), bottom + 18, f"{tick:.1f}", anchor="middle", fill="muted", size=11)
    for i, (name, f) in enumerate(lanes):
        y = top + i * row + 6
        svg.text(0, y + 4, name, fill="ink" if i == 0 else "ink2", weight=600 if i == 0 else 400)
        a, b = f(first), f(last)
        svg.line(x_of(a), y, x_of(b), y, stroke="base", width=2)
        svg.dot(x_of(a), y, svg.t["base"])
        svg.dot(x_of(b), y, svg.t["series"][0])
        low, high = (a, b) if a <= b else (b, a)
        svg.text(x_of(low) - 10, y + 4, f"{low:.2f}", anchor="end", fill="ink2", size=11)
        svg.text(x_of(high) + 10, y + 4, f"{high:.2f}", fill="ink", size=11, weight=600 if high == b else 400)
    svg.save("lanes", theme)


def luminance(hex_color):
    def channel(c):
        c = c / 255
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    r, g, b = (int(hex_color[i : i + 2], 16) for i in (1, 3, 5))
    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)


def main():
    runs = load_runs()
    last = runs[-1]
    for theme in THEMES:
        line_chart(
            "mean-grade",
            theme,
            runs,
            [("all searches", lambda r: r["mean"], 0)],
            [1.6, 1.7, 1.8, 1.9, 2.0, 2.1, 2.2],
            lambda v: f"{v:.2f}" if v not in (1.6, 1.7, 1.8, 1.9, 2.0, 2.1, 2.2) else f"{v:.1f}",
            "Mean grade of 1,000 searches",
            "Each search graded 0 (useless) to 3 (great) by an LLM reviewer",
            "Mean grade by run: " + ", ".join(f"run {r['run']} {r['mean']:.2f}" for r in runs),
            goal=(2.0, "Goal 2.0"),
        )
        line_chart(
            "known-answers",
            theme,
            runs,
            [
                ("in the top 3", lambda r: r["known_top3"] / r["known_n"] * 100, 1),
                ("at #1", lambda r: r["known_top1"] / r["known_n"] * 100, 0),
            ],
            [50, 60, 70, 80, 90, 100],
            lambda v: f"{v:.0f}%",
            "Searches with a known right answer",
            f"Where the expected page landed, {int(last['known_n'])} searches, no reviewer involved",
            "Share with the answer at #1 and in the top 3 by run: "
            + ", ".join(
                f"run {r['run']} {r['known_top1'] / r['known_n']:.0%} and {r['known_top3'] / r['known_n']:.0%}"
                for r in runs
            ),
        )
        line_chart(
            "speed",
            theme,
            runs,
            [
                ("95th percentile", lambda r: r["p95_s"], 1),
                ("median", lambda r: r["median_s"], 0),
            ],
            [0.0, 0.4, 0.8, 1.2, 1.6],
            lambda v: f"{v:.1f} s" if v in (0.0, 0.4, 0.8, 1.2, 1.6) else f"{v:.2f} s",
            "Time to answer a search",
            "Median and 95th percentile over the same 1,000 searches on the test server",
            "Median and 95th percentile seconds by run: "
            + ", ".join(f"run {r['run']} {r['median_s']} s and {r['p95_s']} s" for r in runs),
        )
        grades_chart(theme, runs)
        lanes_chart(theme, runs)


if __name__ == "__main__":
    main()
