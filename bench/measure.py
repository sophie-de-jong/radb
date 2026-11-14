#!/usr/bin/env python3
"""Reproducible measurements for the M2 and M4 reports.

Run from the repository root:

    cargo build --release
    python3 bench/measure.py m2
    python3 bench/measure.py m4

The script writes only under `bench/data/` (ignored by git) and prints JSON
results to stdout.  It never modifies source files.
"""

from __future__ import annotations

import json
import math
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release"
DATA = ROOT / "bench" / "data"


def run(args: list[str], check: bool = True) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(
        [str(BIN / args[0]), *args[1:]], text=True, capture_output=True
    )
    if check and proc.returncode != 0:
        raise RuntimeError(f"{' '.join(args)}\n{proc.stdout}\n{proc.stderr}")
    return proc


def storage(*args: str) -> str:
    return run(["radb-storage", *args]).stdout


def sql_compile(db: Path, query: str) -> dict:
    file = db / "query.sql"
    file.write_text(query)
    out = run(
        [
            "radb-sql",
            "compile",
            "--db",
            str(db),
            "--file",
            str(file),
            "--json",
        ]
    ).stdout
    return json.loads(out.strip().splitlines()[0])


def opt_plan(
    db: Path,
    logical: dict,
    frames: int,
    *extra: str,
) -> dict:
    file = db / "logical.json"
    file.write_text(json.dumps(logical))
    out = run(
        [
            "radb-opt",
            "plan",
            "--db",
            str(db),
            "--logical",
            str(file),
            "--frames",
            str(frames),
            "--json",
            *extra,
        ]
    ).stdout
    return json.loads(out)


def exec_trace(db: Path, plan: dict, frames: int) -> dict:
    file = db / "physical.json"
    file.write_text(json.dumps(plan))
    out = run(
        [
            "radb-exec",
            "trace",
            "--db",
            str(db),
            "--plan",
            str(file),
            "--frames",
            str(frames),
        ]
    ).stdout
    nodes = []
    totals = {"tuples_out": 0, "comparisons": 0, "page_reads": 0, "page_writes": 0}
    for line in out.splitlines():
        if not line.strip():
            continue
        depth = (len(line) - len(line.lstrip(" "))) // 2
        label, counters = line.strip().split("\t", 1)
        values = {}
        for part in counters.split():
            key, value = part.split("=", 1)
            values[key] = int(value)
        values["page_reads"] = values.pop("reads")
        values["page_writes"] = values.pop("writes")
        nodes.append({"depth": depth, "label": label, **values})
        for key in totals:
            totals[key] += values[key]
    return {"nodes": nodes, "totals": totals}


def table_pages(db: Path, table: str) -> int:
    proc = run(
        [
            "radb-storage",
            "layout",
            "--db",
            str(db),
            "--table",
            table,
            "--page",
            "999999",
        ],
        check=False,
    )
    text = proc.stdout + proc.stderr
    marker = "(1 pages)"
    if marker in text:
        return 1
    # "... (N pages)"
    for token in text.replace("(", " ").replace(")", " ").split():
        if token.isdigit():
            pass
    start = text.rfind("(")
    end = text.find(" pages)", start)
    if start < 0 or end < 0:
        raise RuntimeError(f"cannot read page count from {text!r}")
    return int(text[start + 1 : end]) - 1


def csv_field(value: object) -> str:
    text = str(value)
    if "," in text or '"' in text or "\n" in text:
        return '"' + text.replace('"', '""') + '"'
    return text


def write_csv(path: Path, rows: list[list[object]]) -> None:
    path.write_text("\n".join(",".join(csv_field(v) for v in row) for row in rows) + "\n")


def make_table(db: Path, name: str, schema: str, rows: list[list[object]]) -> None:
    storage(
        "create-table",
        "--db",
        str(db),
        "--name",
        name,
        "--schema",
        schema,
    )
    file = db / f"{name}.csv"
    write_csv(file, rows)
    storage(
        "load",
        "--db",
        str(db),
        "--table",
        name,
        "--file",
        str(file),
    )


def fresh(name: str) -> Path:
    path = DATA / name
    shutil.rmtree(path, ignore_errors=True)
    path.mkdir(parents=True)
    return path


# ---------------------------------------------------------------------------
# Module 2
# ---------------------------------------------------------------------------


def join_table(db: Path, name: str, pages: int, offset: int = 0) -> None:
    """`name(a INT, b INT, pad VARCHAR(72))`, exactly `pages` data pages."""
    rows_per_page = 47
    count = pages * rows_per_page
    ndv = max(1, count // 10)
    rows = [
        [offset + i, (i * 7 + 3) % ndv + 1, "x" * 72] for i in range(count)
    ]
    make_table(db, name, "a:INT,b:INT,pad:VARCHAR(72)", rows)


def probe_table(db: Path, name: str, pages: int) -> None:
    """`name(b INT, c INT, pad VARCHAR(72))`, matching the join-table keys."""
    rows_per_page = 47
    count = pages * rows_per_page
    ndv = max(1, count // 10)
    rows = [
        [(i * 11 + 5) % ndv + 1, i, "x" * 72] for i in range(count)
    ]
    make_table(db, name, "b:INT,c:INT,pad:VARCHAR(72)", rows)


def join_plan(kind: str) -> dict:
    return {
        "op": kind,
        "condition": "R.b = S.b",
        "left": {"op": "seqscan", "table": "R"},
        "right": {"op": "seqscan", "table": "S"},
    }


def measure_m2() -> dict:
    db = fresh("m2")
    results: dict[str, object] = {}

    sizes = [10, 40, 100]
    frames = [4, 16, 64, 256]
    algorithms = ["nested_loop_join", "sort_merge_join", "hash_join"]
    join_rows = []
    for pages in sizes:
        db_size = db / f"size{pages}"
        shutil.rmtree(db_size, ignore_errors=True)
        db_size.mkdir()
        make_table(
            db_size,
            "R",
            "a:INT,b:INT,pad:VARCHAR(72)",
            [
                [i, (i * 7 + 3) % max(1, pages * 47 // 10) + 1, "x" * 72]
                for i in range(pages * 47)
            ],
        )
        make_table(
            db_size,
            "S",
            "b:INT,c:INT,pad:VARCHAR(72)",
            [
                [(i * 11 + 5) % max(1, pages * 47 // 10) + 1, i, "x" * 72]
                for i in range(pages * 47)
            ],
        )
        m = table_pages(db_size, "R")
        n = table_pages(db_size, "S")
        for b in frames:
            for kind in algorithms:
                trace = exec_trace(db_size, join_plan(kind), b)
                join_rows.append(
                    {
                        "size_pages": pages,
                        "M": m,
                        "N": n,
                        "B": b,
                        "algorithm": kind,
                        "reads": trace["totals"]["page_reads"],
                        "writes": trace["totals"]["page_writes"],
                        "rows": trace["totals"]["tuples_out"],
                    }
                )
    results["joins"] = join_rows

    # Sort passes: 1 + ceil(log(ceil(N/B)) / log(B-1)).
    sort_db = db / "sorts"
    shutil.rmtree(sort_db, ignore_errors=True)
    sort_db.mkdir()
    sort_rows = []
    for pages, b in [(10, 4), (10, 8), (40, 4), (40, 16), (80, 4), (80, 16), (100, 32)]:
        name = f"T{pages}"
        if not (sort_db / f"{name}.db").exists():
            make_table(
                sort_db,
                name,
                "a:INT,b:INT,pad:VARCHAR(72)",
                [[i, i % 17, "x" * 72] for i in range(pages * 47)],
            )
        plan = {
            "op": "sort",
            "keys": [[f"{name}.a", "asc"]],
            "child": {"op": "seqscan", "table": name},
        }
        trace = exec_trace(sort_db, plan, b)
        sort_node = next(node for node in trace["nodes"] if node["label"] == "sort")
        n_pages = table_pages(sort_db, name)
        # The final pass is not written, so writes / N is pass count minus one.
        measured_passes = 1 + round(sort_node["page_writes"] / n_pages)
        formula_passes = 1 + math.ceil(
            math.log(math.ceil(n_pages / b), 2) / math.log(max(2, b - 1), 2)
        )
        sort_rows.append(
            {
                "N": n_pages,
                "B": b,
                "writes": sort_node["page_writes"],
                "reads": sort_node["page_reads"],
                "measured_passes": measured_passes,
                "formula_passes": formula_passes,
            }
        )
    results["sort"] = sort_rows

    # Crossover at a fixed size and at fixed B.
    crossover_b = []
    size = 100
    db_size = db / f"size{size}"
    for b in [4, 8, 16, 32, 64, 128, 256]:
        costs = {}
        for kind in algorithms:
            trace = exec_trace(db_size, join_plan(kind), b)
            costs[kind] = (
                trace["totals"]["page_reads"] + trace["totals"]["page_writes"]
            )
        winner = min(costs, key=costs.get)
        crossover_b.append({"B": b, **costs, "winner": winner})
    results["crossover_b"] = crossover_b

    crossover_size = []
    for pages in sizes:
        db_size = db / f"size{pages}"
        costs = {}
        for kind in algorithms:
            trace = exec_trace(db_size, join_plan(kind), 16)
            costs[kind] = (
                trace["totals"]["page_reads"] + trace["totals"]["page_writes"]
            )
        winner = min(costs, key=costs.get)
        crossover_size.append({"pages": pages, **costs, "winner": winner})
    results["crossover_size"] = crossover_size

    # Skewed hash join: 40% of both inputs share key 1.
    skew_db = db / "skew"
    shutil.rmtree(skew_db, ignore_errors=True)
    skew_db.mkdir()
    count = 100 * 47
    hot = int(count * 0.4)
    make_table(
        skew_db,
        "R",
        "a:INT,b:INT,pad:VARCHAR(72)",
        [[i, 1 if i < hot else i + 2, "x" * 72] for i in range(count)],
    )
    make_table(
        skew_db,
        "S",
        "b:INT,c:INT,pad:VARCHAR(72)",
        [[1 if i < hot else i + 2, i, "x" * 72] for i in range(count)],
    )
    skew = exec_trace(skew_db, join_plan("hash_join"), 4)
    uniform = next(
        row
        for row in join_rows
        if row["size_pages"] == 100 and row["B"] == 4 and row["algorithm"] == "hash_join"
    )
    results["skew"] = {
        "hot_rows_each": hot,
        "hot_fraction": 0.4,
        "reads": skew["totals"]["page_reads"],
        "writes": skew["totals"]["page_writes"],
        "rows": skew["totals"]["tuples_out"],
        "uniform_reads": uniform["reads"],
        "uniform_writes": uniform["writes"],
        "uniform_rows": uniform["rows"],
    }

    # The 20 x B budget case at B = 4.
    budget_pages = 80
    budget_name = f"T{budget_pages}"
    budget = exec_trace(
        sort_db,
        {
            "op": "sort",
            "keys": [[f"{budget_name}.a", "asc"]],
            "child": {"op": "seqscan", "table": budget_name},
        },
        4,
    )
    # The trace command does not print the peak; run --json does.
    plan_file = sort_db / "budget.json"
    plan_file.write_text(
        json.dumps(
            {
                "op": "sort",
                "keys": [[f"{budget_name}.a", "asc"]],
                "child": {"op": "seqscan", "table": budget_name},
            }
        )
    )
    out = run(
        [
            "radb-exec",
            "run",
            "--db",
            str(sort_db),
            "--plan",
            str(plan_file),
            "--frames",
            "4",
            "--json",
        ]
    ).stdout
    budget_json = json.loads(out)
    results["budget"] = {
        "N": table_pages(sort_db, budget_name),
        "B": 4,
        "rows": budget["totals"]["tuples_out"],
        "reads": budget["totals"]["page_reads"],
        "writes": budget["totals"]["page_writes"],
        "peak_frames": budget_json["peak_frames"],
    }

    return results


# ---------------------------------------------------------------------------
# Module 4
# ---------------------------------------------------------------------------


QUERIES = [
    "SELECT * FROM Emp WHERE age > 40;",
    "SELECT name, salary FROM Emp WHERE age > 40 AND salary > 5500;",
    "SELECT COUNT(*) FROM Emp;",
    "SELECT did, COUNT(*) AS n FROM Emp GROUP BY did ORDER BY n DESC LIMIT 5;",
    "SELECT e.name, d.title FROM Emp e JOIN Dept d ON e.did = d.did WHERE e.age > 45;",
    "SELECT e.name, d.title FROM Emp e JOIN Dept d ON e.did = d.did WHERE d.budget > 5000;",
    "SELECT e.name, p.name FROM Emp e JOIN Proj p ON e.did = p.did WHERE e.age > 45 AND p.name < 'p5';",
    "SELECT e.name, d.title, p.name FROM Emp e JOIN Dept d ON e.did = d.did JOIN Proj p ON d.did = p.did;",
    "SELECT e.name, d.title, p.name, l.addr FROM Emp e JOIN Dept d ON e.did = d.did JOIN Proj p ON d.did = p.did JOIN Loc l ON p.did = l.did;",
    "SELECT e.name, m.name FROM Emp e JOIN Emp m ON e.mgr = m.eid WHERE e.age > 40;",
    "SELECT e.name, d.title FROM Emp e, Dept d WHERE e.did = d.did AND e.age > 40 AND d.budget > 5000;",
    "SELECT did, COUNT(*) AS n FROM Emp GROUP BY did HAVING COUNT(*) > 50;",
    "SELECT DISTINCT city FROM Emp WHERE age > 35;",
    "SELECT name FROM Emp ORDER BY salary DESC LIMIT 10;",
    "SELECT t.did, t.n FROM (SELECT did, COUNT(*) AS n FROM Emp GROUP BY did) AS t WHERE t.n > 100;",
    "SELECT city FROM Emp WHERE age > 35 UNION SELECT city FROM Dept;",
    "SELECT city FROM Emp WHERE age > 35 UNION ALL SELECT city FROM Dept;",
    "SELECT e.did, COUNT(*) AS n, AVG(salary) AS a FROM Emp e WHERE e.age > 40 GROUP BY e.did ORDER BY a DESC;",
    "SELECT e.name, d.title FROM Emp e JOIN Dept d ON e.did = d.did WHERE e.age > 45 AND e.salary > 5000;",
    "SELECT name FROM Emp WHERE city = 'city3' AND salary > 6000;",
]


def parse_explain(text: str) -> list[dict]:
    nodes = []
    for line in text.splitlines():
        if "est_rows=" not in line:
            continue
        depth = (len(line) - len(line.lstrip(" "))) // 2
        left, rest = line.strip().split(" est_rows=", 1)
        rows, cost = rest.split("  est_cost=", 1)
        nodes.append(
            {"depth": depth, "label": left.strip(), "est_rows": int(rows), "est_cost": int(cost)}
        )
    return nodes


def optimize_and_explain(db: Path, logical: dict, frames: int, *extra: str) -> list[dict]:
    file = db / "logical.json"
    file.write_text(json.dumps(logical))
    out = run(
        [
            "radb-opt",
            "explain",
            "--db",
            str(db),
            "--logical",
            str(file),
            "--frames",
            str(frames),
            "--passes",
            "10",
        ]
    ).stdout
    return parse_explain(out)


def qerror(est: int, actual: int) -> float:
    return max(est / max(1, actual), max(1, actual) / max(1, est))


def make_m4_data(db: Path) -> None:
    import random

    random.seed(7)
    emp = []
    for eid in range(1, 12001):
        age = 20 + (eid % 41)
        did = 1 + (eid % 50)
        salary = age * 100 + did * 13 + (eid % 7)
        city = f"city{did}"
        emp.append([eid, f"emp{eid}", age, did, salary, max(1, eid // 10), city])
    make_table(
        db,
        "Emp",
        "eid:INT,name:VARCHAR(20),age:INT,did:INT,salary:INT,mgr:INT,city:VARCHAR(20)",
        emp,
    )
    dept = []
    for did in range(1, 51):
        dept.append([did, f"dept{did}", 1000 + did * 400, f"city{did}"])
    make_table(db, "Dept", "did:INT,title:VARCHAR(20),budget:INT,city:VARCHAR(20)", dept)
    proj = [[1 + (i % 50), f"p{i % 25}"] for i in range(1, 3001)]
    make_table(db, "Proj", "did:INT,name:VARCHAR(20)", proj)
    loc = [[1 + (i % 50), f"addr{i}"] for i in range(1, 501)]
    make_table(db, "Loc", "did:INT,addr:VARCHAR(30)", loc)
    for table in ["Emp", "Dept", "Proj", "Loc"]:
        run(
            [
                "radb-opt",
                "analyze",
                "--db",
                str(db),
                "--table",
                table,
                "--buckets",
                "20",
            ]
        )


def workload_io(db: Path, plan: dict, frames: int) -> tuple[int, int, list[dict]]:
    trace = exec_trace(db, plan, frames)
    io = trace["totals"]["page_reads"] + trace["totals"]["page_writes"]
    return io, trace["totals"]["tuples_out"], trace["nodes"]


def measure_m4() -> dict:
    db = fresh("m4")
    make_m4_data(db)
    frames = 32
    rows = []
    all_qerrors = []
    optimized_io = 0
    baseline_io = 0
    worst = None

    for index, query in enumerate(QUERIES, start=1):
        logical = sql_compile(db, query)
        estimates = optimize_and_explain(db, logical, frames)
        plan = opt_plan(db, logical, frames)
        io, actual_rows, trace_nodes = workload_io(db, plan, frames)
        nodes = []
        for estimate, actual in zip(estimates, trace_nodes):
            q = qerror(estimate["est_rows"], actual["tuples_out"])
            all_qerrors.append(q)
            nodes.append(
                {
                    "label": estimate["label"],
                    "est_rows": estimate["est_rows"],
                    "actual_rows": actual["tuples_out"],
                    "q_error": q,
                }
            )

        baseline = opt_plan(db, logical, frames, "--no-optimize")
        baseline_value, _, _ = workload_io(db, baseline, frames)
        ratio = io / max(1, baseline_value)
        item = {
            "query": index,
            "sql": query,
            "rows": actual_rows,
            "io": io,
            "baseline_io": baseline_value,
            "ratio_optimized_to_baseline": ratio,
            "nodes": nodes,
            "worst_q_error": max((node["q_error"] for node in nodes), default=1.0),
        }
        rows.append(item)
        optimized_io += io
        baseline_io += baseline_value
        if worst is None or ratio > worst["ratio_optimized_to_baseline"]:
            worst = item

    all_qerrors.sort()
    median = all_qerrors[len(all_qerrors) // 2] if all_qerrors else 1.0

    rule_io = {}
    for rule in range(1, 8):
        total = 0
        for query in QUERIES:
            logical = sql_compile(db, query)
            plan = opt_plan(db, logical, frames, "--without-rule", str(rule))
            io, _, _ = workload_io(db, plan, frames)
            total += io
        rule_io[str(rule)] = total

    # Where the independence assumption breaks: age and salary are correlated
    # in this dataset, and both predicates sit on the same relation.
    correlated_sql = (
        "SELECT e.name, p.name, l.addr FROM Emp e "
        "JOIN Proj p ON e.did = p.did "
        "JOIN Loc l ON p.did = l.did "
        "WHERE e.age > 40 AND e.salary > 6500;"
    )
    logical = sql_compile(db, correlated_sql)
    estimates = optimize_and_explain(db, logical, frames)
    plan = opt_plan(db, logical, frames)
    io, actual_rows, trace_nodes = workload_io(db, plan, frames)
    baseline = opt_plan(db, logical, frames, "--no-optimize")
    baseline_value, _, _ = workload_io(db, baseline, frames)
    correlated_nodes = [
        {
            "label": estimate["label"],
            "est_rows": estimate["est_rows"],
            "actual_rows": actual["tuples_out"],
            "q_error": qerror(estimate["est_rows"], actual["tuples_out"]),
        }
        for estimate, actual in zip(estimates, trace_nodes)
    ]
    correlated = {
        "sql": correlated_sql,
        "rows": actual_rows,
        "io": io,
        "baseline_io": baseline_value,
        "ratio_optimized_to_baseline": io / max(1, baseline_value),
        "nodes": correlated_nodes,
        "worst_q_error": max((node["q_error"] for node in correlated_nodes), default=1.0),
    }

    # A four-relation query for the full DP table.
    four = (
        "SELECT e.name, d.title, p.name, l.addr FROM Emp e "
        "JOIN Dept d ON e.did = d.did "
        "JOIN Proj p ON d.did = p.did "
        "JOIN Loc l ON p.did = l.did "
        "WHERE e.age > 40;"
    )
    logical = sql_compile(db, four)
    logical_file = db / "four.json"
    logical_file.write_text(json.dumps(logical))
    dp_table = run(
        [
            "radb-opt",
            "dp-table",
            "--db",
            str(db),
            "--logical",
            str(logical_file),
            "--frames",
            str(frames),
        ]
    ).stdout

    return {
        "queries": rows,
        "median_q_error": median,
        "worst_q_error": max(all_qerrors) if all_qerrors else 1.0,
        "optimized_io": optimized_io,
        "baseline_io": baseline_io,
        "overall_ratio": optimized_io / max(1, baseline_io),
        "worst_optimized_query": worst,
        "rule_io": rule_io,
        "correlated": correlated,
        "dp_table": dp_table,
    }


def main() -> None:
    if len(sys.argv) != 2 or sys.argv[1] not in ("m2", "m4"):
        raise SystemExit("usage: measure.py m2|m4")
    DATA.mkdir(parents=True, exist_ok=True)
    result = measure_m2() if sys.argv[1] == "m2" else measure_m4()
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
