#!/usr/bin/env python3
"""Fail when the docs stop agreeing with the repository, or with themselves.

Two checks, both cheap enough to run on every push:

  consistency  Every benchmark measurement appears three times on benchmarks.html
               (scatter tooltip, at-a-glance grid, dossier bar) and again in the
               homepage summary. They must all say the same thing. This is the
               check that matters most: a stale rendering once gave erlang 0.46 s
               on binary trees where the other two said 0.64 s, which would have
               reversed the page's own "every row is a win" claim.

  repo         Counts and sizes the page states about the source tree, each
               re-derived from the tree itself.

Benchmark timings are NOT verified here. They need a quiet machine and twenty
minutes, so this script reports them as unverified rather than pretending. The
same goes for whether the setup block actually works; that needs a clean
container and belongs in its own job.

Usage: python3 docs/check-claims.py [--verbose]
Exit 0 when everything agrees, 1 otherwise.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

DOCS = Path(__file__).resolve().parent
REPO_ROOT = DOCS.parent.parent          # workspace root, above gleam/
GLEAM = DOCS.parent                     # the compiler fork

INDEX = DOCS / "index.html"
BENCH = DOCS / "benchmarks.html"

BENCHMARK_ORDER = [
    "String building",
    "Dictionary",
    "Ray tracer",
    "Coin change",
    "Vector records",
    "Binary trees",
]

failures: list[str] = []
notes: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


def norm(value: str) -> str:
    """Compare figures by meaning, not by markup."""
    return (
        value.replace("&lt;", "<")
        .replace("&nbsp;", " ")
        .replace("&middot;", "-")
        .strip()
    )


def target_of(raw: str) -> str:
    low = raw.lower()
    if "zig" in low:
        return "zig"
    if "node" in low:
        return "node"
    return "erlang"


# --------------------------------------------------------------- consistency

def scatter_figures(html: str) -> dict[tuple[str, str], tuple[str, str]]:
    """The per-run record: one dot per (benchmark, target)."""
    out = {}
    pattern = re.compile(
        r'data-bench="([^"]*)"[^>]*data-target="([^"]*)"[^>]*'
        r'data-time="([^"]*)" data-mem="([^"]*)"'
    )
    for bench, target, time, mem in pattern.findall(html):
        out[(bench, target_of(target))] = (norm(time), norm(mem))
    return out


def glance_figures(html: str) -> dict[tuple[str, str], tuple[str, str]]:
    """The summary grid: cpu cell and memory cell per row, zig/node/erl order."""
    out = {}
    rows = re.findall(r'<div class="g-row">.*?\n      </div>', html, re.S)
    for bench, row in zip(BENCHMARK_ORDER, rows):
        cells = re.findall(r'<div class="g-cell">.*?</div>\n        </div>', row, re.S)
        if len(cells) < 2:
            fail(f"glance row for {bench} has {len(cells)} cells, expected at least 2")
            continue
        cpu = [norm(v) for v in re.findall(r"<b>([^<]*)</b>", cells[0])]
        mem = [norm(v) for v in re.findall(r"<b>([^<]*)</b>", cells[1])]
        for i, target in enumerate(("zig", "node", "erlang")):
            if i < len(cpu) and i < len(mem):
                out[(bench, target)] = (cpu[i] + " s", mem[i])
    return out


def lane_figures(html: str) -> list[tuple[str, str, str]]:
    """Dossier and summary bars. These carry no benchmark name, so they are
    checked as a set: every lane must match some scatter entry for its target."""
    pattern = re.compile(
        r'<span class="who">(zig|node|erlang)</span><div class="bar"[^>]*></div>'
        r'<span class="t">([^<]*)</span><span class="m">([^<]*)</span>'
    )
    return [(w, norm(t), norm(m)) for w, t, m in pattern.findall(html)]


def check_consistency() -> None:
    bench_html = BENCH.read_text(encoding="utf-8")
    index_html = INDEX.read_text(encoding="utf-8")

    scatter = scatter_figures(bench_html)
    if len(scatter) != 18:
        fail(f"expected 18 scatter dots (6 benchmarks x 3 targets), found {len(scatter)}")
        return

    # The grid must agree with the dots, cell for cell.
    for key, glance_val in glance_figures(bench_html).items():
        dot = scatter.get(key)
        if dot is None:
            fail(f"glance has {key} but the scatter does not")
        elif glance_val[0] != dot[0] or glance_val[1] != dot[1]:
            fail(
                f"{key[0]} / {key[1]}: glance says {glance_val[0]} / {glance_val[1]}, "
                f"scatter says {dot[0]} / {dot[1]}"
            )

    # Every bar on either page must be some dot. A bar that matches nothing is
    # either stale or a figure that exists in only one place.
    known = set(scatter.values())
    for page, html in (("benchmarks.html", bench_html), ("index.html", index_html)):
        for target, time, mem in lane_figures(html):
            if (time, mem) not in known:
                fail(f"{page}: bar '{target} {time} / {mem}' matches no scatter dot")
    notes.append(f"consistency: {len(scatter)} measurements x 3 renderings checked")


# ---------------------------------------------------------------------- repo

def line_count(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def zig_fn_count(path: Path) -> int:
    return len(re.findall(r"^\s*(?:pub )?fn ", path.read_text(encoding="utf-8"), re.M))


def check_repo() -> None:
    """Counts the page asserts about the source, re-derived from the source."""
    index_html = INDEX.read_text(encoding="utf-8")

    prelude = GLEAM / "compiler-core" / "templates" / "prelude.zig"
    externals = REPO_ROOT / "gleam-stdlib" / "src" / "gleam_stdlib.zig"

    claims = []
    if prelude.exists():
        claims.append(("prelude line count", f"{line_count(prelude):,}", index_html))
    else:
        notes.append(f"skipped prelude line count: {prelude} not found")

    if externals.exists():
        claims.append(("externals line count", f"{line_count(externals):,}", index_html))
        claims.append(("externals fn count", str(zig_fn_count(externals)), index_html))
    else:
        notes.append(f"skipped externals counts: {externals} not found")

    for label, actual, html in claims:
        if actual not in html:
            fail(f"{label}: repo says {actual}, which appears nowhere on index.html")

    # Corpus program counts, from the directories the harness reads.
    rosetta = REPO_ROOT / "examples" / "rosetta"
    if rosetta.is_dir():
        n = len(list(rosetta.glob("*.gleam")))
        notes.append(f"rosetta holds {n} programs (pass/skip split needs a harness run)")


# ------------------------------------------------------------- not checked

def report_unverified() -> None:
    """Say plainly what this script does not cover, so a green run is not
    mistaken for a full verification."""
    notes.append("NOT checked here: benchmark timings (need a quiet machine)")
    notes.append("NOT checked here: whether the setup block works (needs a clean container)")
    notes.append("NOT checked here: prose claims about method, which need a reader")


def main() -> int:
    verbose = "--verbose" in sys.argv

    check_consistency()
    check_repo()
    report_unverified()

    if verbose or failures:
        for note in notes:
            print(f"  {note}")

    if failures:
        print(f"\n{len(failures)} claim(s) no longer hold:\n", file=sys.stderr)
        for f in failures:
            print(f"  {f}", file=sys.stderr)
        return 1

    print(f"docs claims hold ({len(notes)} notes, run with --verbose to see them)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
