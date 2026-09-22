//! Section 8 — Performance-study binary.
//!
//! Usage:
//!   radb-study write   --n N --m M [--matches F] [--seed S]
//!   radb-study study   [--sizes S1,S2,...] [--matches F] [--seed S]
//!
//! `write` — generates R.txt and S.txt in the relation-definition syntax
//!           of §4.1.
//! `study` — parses the §8.3 query from source text through `radb::parse`
//!           (no hand-built ASTs) and times join at every size, then select
//!           and project at the same sizes (§8.4 q3). Progress prints live
//!           to stderr (each size's row appears the moment it finishes), and
//!           stdout carries only the two paste-ready tables — the §8.3 join
//!           table exactly as INSTRUCTIONS.txt defines it (n, m,
//!           comparisons, wall time (s), output tuples) followed by the
//!           select/project table, aligned with thousands separators, so
//!           `radb-study study > results.md` captures just the deliverables.
//!
//! Both subcommands' flag lists come from clap (`radb-study --help`).

use std::collections::HashSet;
use std::{fs, io};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser as ClapParser, Subcommand};

use radb::Value;
use radb::{parse_query, Engine, Relation};

/// radb performance-study data generator and experiment runner.
#[derive(ClapParser)]
#[command(name = "radb-study", version, about = "radb performance-study data generator")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate R.txt and S.txt in the §4.1 relation-definition syntax.
    Write {
        /// Number of R tuples.
        #[arg(long, default_value_t = 100)]
        n: usize,
        /// Number of S tuples.
        #[arg(long, default_value_t = 100)]
        m: usize,
        /// Roughly how many S tuples each R tuple should join with.
        #[arg(long, default_value_t = 1.0)]
        matches: f64,
        /// PRNG seed, so runs are reproducible.
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Time join (§8.3) plus select/project (§8.4 q3) at several sizes;
    /// prints the instruction-defined join table, then a select/project table.
    Study {
        /// Relation sizes to benchmark (comma-separated).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "1000,2000,4000,8000,16000,32000,64000"
        )]
        sizes: Vec<usize>,
        /// Roughly how many S tuples each R tuple should join with.
        #[arg(long, default_value_t = 5.0)]
        matches: f64,
        /// PRNG seed, so runs are reproducible.
        #[arg(long, default_value_t = 7)]
        seed: u64,
    },
}

fn write(n: usize, m: usize, matches: f64, seed: u64) -> io::Result<()> {
    let (r, s) = generate_relations(n, m, matches, seed)
        .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
    write_relation_file(&r, "R")?;
    write_relation_file(&s, "S")?;
    println!("wrote R.txt ({n} tuples) and S.txt ({m} tuples)");
    Ok(())
}

fn write_relation_file(rel: &Relation, name: &str) -> io::Result<()> {
    let path = PathBuf::from(format!("{name}.txt"));
    let mut f = fs::File::create(&path)?;
    writeln!(f, "{name}({}) = {{", rel.schema().join(", "))?;
    for row in rel.iter() {
        let vals: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        writeln!(f, "  {}", vals.join(", "))?;
    }
    writeln!(f, "}}")
}

fn study(sizes: Vec<usize>, matches: f64, seed: u64) {
    let join_q = parse_query("R join[R.b=S.b] S").expect("join query should parse");
    let project_q = parse_query("project[b](R)").expect("project query should parse");

    #[cfg(debug_assertions)]
    eprintln!("  warning    : debug build — the large joins take many minutes; use --release");
    eprintln!("radb-study — §8.3 join benchmark, then §8.4 q3 select/project");
    eprintln!("  sizes      : {sizes:?}");
    eprintln!("  match rate : ~{matches}, seed {seed}\n");

    let mut join_rows: Vec<(usize, u64, f64, usize)> = Vec::with_capacity(sizes.len());
    let mut sp_rows: Vec<(usize, f64, u64, f64)> = Vec::with_capacity(sizes.len());
    let started = Instant::now();

    for &n in &sizes {
        let (r, s) = generate_relations(n, n, matches, seed)
            .expect("study sizes are small enough for the generator");
        let mut eng = Engine::new();
        eng.load("R", r);
        eng.load("S", s);

        // Threshold scaled per size, like the old per-size select expression.
        let select_q =
            parse_query(&format!("select[a>={}](R)", n / 2)).expect("select query should parse");

        eng.reset_stats();
        let t = Instant::now();
        let output = eng.execute(&join_q).expect("join failed").len();
        let jt = t.elapsed().as_secs_f64();
        let comps = eng.stats.join_comparisons;
        eprintln!("  n = {n:<6} join    {comps} comparisons in {jt:8.3} s - {output} tuples");
        join_rows.push((n, comps, jt, output));

        eng.reset_stats();
        let t = Instant::now();
        let _ = eng.execute(&select_q).expect("select failed");
        let st = t.elapsed().as_secs_f64();
        let examined = eng.stats.selection_examinations;

        eng.reset_stats();
        let t = Instant::now();
        let _ = eng.execute(&project_q).expect("project failed");
        let pt = t.elapsed().as_secs_f64();
        eprintln!("  n = {n:<6} select  {st:8.4} s (examined {examined}) · project {pt:8.4} s");
        sp_rows.push((n, st, examined, pt));
    }

    eprintln!("\n  finished all {} sizes in {:.1} s\n", sizes.len(), started.elapsed().as_secs_f64());

    print_table(
        &["n", "m", "comparisons", "wall time (s)", "output tuples"],
        &join_rows
            .iter()
            .map(|&(n, comps, jt, out)| {
                vec![
                    n.to_string(),
                    n.to_string(),
                    comps.to_string(),
                    format!("{jt:.3}"),
                    out.to_string(),
                ]
            })
            .collect::<Vec<_>>(),
    );
    eprintln!();
    print_table(
        &["n", "select time (s)", "select examinations", "project time (s)"],
        &sp_rows
            .iter()
            .map(|&(n, st, examined, pt)| {
                vec![
                    n.to_string(),
                    format!("{st:.4}"),
                    examined.to_string(),
                    format!("{pt:.4}"),
                ]
            })
            .collect::<Vec<_>>(),
    );
}

/// Prints a markdown table whose columns also line up in a terminal: every
/// cell is padded to its column's widest entry and right-aligned, and the
/// separator row right-aligns each column (`---:`), the usual style for
/// numeric tables.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..headers.len())
        .map(|c| {
            headers[c]
                .len()
                .max(rows.iter().map(|r| r[c].len()).max().unwrap_or(0))
        })
        .collect();
    let line = |cells: &[String]| {
        let inner: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!(" {cell:>w$} "))
            .collect();
        format!("|{}|", inner.join("|"))
    };
    println!(
        "{}",
        line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>())
    );
    let sep_row: Vec<String> = widths
        .iter()
        .map(|w| "-".repeat(w + 2))
        .collect();
    println!("|{}|", sep_row.join("|"));
    for row in rows {
        println!("{}", line(row));
    }
}

/// Generates R(a, b) and S(b, c) for the §8.1 experiment setup.
///
/// The b-domain is chosen so that each R row matches approximately
/// `expected_matches` rows of S on average: domain ≈ m / expected.
pub fn generate_relations(
    n: usize,
    m: usize,
    expected_matches: f64,
    seed: u64,
) -> Result<(Relation, Relation), String> {
    let mut rng = Rng::new(seed);

    let domain: i64 = if expected_matches <= 0.0 {
        (m as i64 * 1000).max(1)
    } else {
        ((m as f64 / expected_matches).ceil() as i64).max(1)
    };

    // S is a set of (b, c) pairs; the loop below draws until it has collected
    // m distinct ones. If the (b, c) space (b-domain × c-range) is smaller
    // than m, that can never finish. Prefer fail upfront instead of spinning.
    let c_range: i64 = 1_000_000;
    let space = domain.saturating_mul(c_range);
    if m as i64 > space {
        return Err(format!(
            "cannot generate {m} distinct S tuples: the (b, c) value space is only {space} \
             (b-domain {domain} × {c_range} possible c's) at match rate ~{expected_matches}; \
             lower the match rate or the tuple count"
        ));
    }

    let mut r_rows = Vec::with_capacity(n);
    for i in 0..n {
        let a = i as i64;
        let b = rng.next_range(0, domain);
        r_rows.push([Value::Int(a), Value::Int(b)]);
    }

    let mut s_rows = Vec::with_capacity(m);
    let mut seen: HashSet<(i64, i64)> = HashSet::with_capacity(m * 2);
    while s_rows.len() < m {
        let b = rng.next_range(0, domain);
        let c = rng.next_range(0, c_range);
        if seen.insert((b, c)) {
            s_rows.push([Value::Int(b), Value::Int(c)]);
        }
    }

    let mut r = Relation::new(["a", "b"]);
    for row in r_rows {
        r.push(row).expect("generated rows are well-formed");
    }
    let mut s = Relation::new(["b", "c"]);
    for row in s_rows {
        s.push(row).expect("generated rows are well-formed");
    }
    Ok((r, s))
}

/// Small deterministic PRNG (xorshift64*), so runs are reproducible for a
/// given seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub fn next_range(&mut self, lo: i64, hi_exclusive: i64) -> i64 {
        debug_assert!(hi_exclusive > lo);
        let span = (hi_exclusive - lo) as u64;
        lo + (self.next_u64() % span) as i64
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Write {
            n,
            m,
            matches,
            seed,
        } => write(n, m, matches, seed),
        Command::Study {
            sizes,
            matches,
            seed,
        } => {
            study(sizes, matches, seed);
            Ok(())
        },
    }
}

#[cfg(test)]
mod test {
    use crate::*;
    use radb::parse_relation;

    const SEED: u64 = 1028;

    #[test]
    fn generates_the_requested_tuple_counts() {
        let (r, s) = generate_relations(100, 50, 2.0, SEED).unwrap();
        assert_eq!(r.schema(), ["a", "b"]);
        assert_eq!(s.schema(), ["b", "c"]);
        assert_eq!(r.len(), 100);
        assert_eq!(s.len(), 50);
        assert!(r.iter().all(|row| row.len() == 2));
    }

    #[test]
    fn b_domain_is_chosen_from_the_match_rate() {
        // m = 1000, expected 10 matches per tuple → b-domain ≈ 100.
        let (r, s) = generate_relations(500, 1000, 10.0, SEED).unwrap();
        let join = {
            let mut eng = Engine::new();
            eng.load("R", r);
            eng.load("S", s);
            let expr = parse_query("R join[R.b=S.b] S").unwrap();
            eng.execute(&expr).expect("join should run")
        };
        let target = 500 * 10;
        let lower = (target as f64 * 0.5) as usize;
        let upper = (target as f64 * 1.5) as usize;
        assert!(
            (lower..=upper).contains(&join.len()),
            "expected ~{target} output tuples for match rate 10, got {}",
            join.len()
        );
    }

    #[test]
    fn generated_relations_roundtrip_through_the_relation_format() {
        let (r, _s) = generate_relations(20, 20, 3.0, SEED).unwrap();
        let body: Vec<String> = r
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect();
        let text = format!("R(a, b) = {{\n{}\n}}\n", body.join("\n"));
        let (_, loaded) = parse_relation(&text).expect("round-trip should load");
        assert_eq!(loaded.schema(), r.schema());
        assert_eq!(loaded.len(), r.len());
        assert!(loaded.iter().all(|row| r.contains(row)));
    }

    #[test]
    fn duplicate_b_pairs_are_excluded_from_s() {
        // S is built with uniqueness on (b, c), so it stays a set even when n
        // is much larger than the b-domain.
        let (_, s) = generate_relations(1, 1000, 2.0, SEED).unwrap();
        assert_eq!(s.len(), 1000);
        let mut seen = std::collections::HashSet::new();
        for row in s.iter() {
            assert!(seen.insert(row.clone()), "duplicate tuple in S: {row:?}");
        }
    }

    #[test]
    fn relations_are_usable_by_the_engine() {
        let (r, s) = generate_relations(10, 10, 1.0, SEED).unwrap();
        let result = {
            let mut eng = Engine::new();
            eng.load("R", r);
            eng.load("S", s);
            let expr = parse_query("project[b](R)").unwrap();
            eng.execute(&expr).expect("project should run")
        };
        assert_eq!(result.schema(), ["b"]);
        assert!(!result.iter().any(|row| !matches!(row[0], Value::Int(_))));
    }
}
