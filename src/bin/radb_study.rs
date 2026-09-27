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
//!           and project at the same sizes (§8.4 q3). Progress and the
//!           per-size select/project figures print to stderr (each size's row
//!           appears the moment it finishes), and stdout carries only the
//!           §8.3 join table — n, m, comparisons, wall time (s), output
//!           tuples — so `radb-study study > join_table.md` captures just the
//!           deliverable.
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
    /// Time join (§8.3) plus select/project (§8.4 q3) at several sizes.
    /// Prints the §8.3 join table on stdout; the per-size select and
    /// project figures, and progress, go to stderr.
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

/// The `write` subcommand: generate R.txt and S.txt at the requested sizes.
///
/// Errors: an [`io::Error`] if the requested S cannot be generated or a file
/// cannot be written.
fn write(n: usize, m: usize, matches: f64, seed: u64) -> io::Result<()> {
    let (r, s) = generate_relations(n, m, matches, seed)
        .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
    write_relation_file(&r, "R")?;
    write_relation_file(&s, "S")?;
    println!("wrote R.txt ({n} tuples) and S.txt ({m} tuples)");
    Ok(())
}

/// Write `rel` to `<name>.txt` in the §4.1 relation-definition syntax.
///
/// Errors: an [`io::Error`] from creating or writing the file.
fn write_relation_file(rel: &Relation, name: &str) -> io::Result<()> {
    let path = PathBuf::from(format!("{name}.txt"));
    let mut f = fs::File::create(&path)?;
    writeln!(f, "{name}({}) = {{", rel.schema().names().join(", "))?;
    for row in rel.iter() {
        let vals: Vec<String> = row.iter().map(|v| v.to_string()).collect();
        writeln!(f, "  {}", vals.join(", "))?;
    }
    writeln!(f, "}}")
}

/// The `study` subcommand: time the §8.3 join at each size, then select and
/// project at the same sizes (§8.4 q3). The §8.3 join table goes to stdout;
/// progress and the per-size select and project figures go to stderr.
fn study(sizes: Vec<usize>, matches: f64, seed: u64) {
    let join_q = parse_query("R join[R.b=S.b] S").expect("join query should parse");
    let project_q = parse_query("project[b](R)").expect("project query should parse");

    eprintln!("radb-study - §8.3 join benchmark");
    eprintln!("  sizes      : {sizes:?}");
    eprintln!("  match rate : ~{matches}, seed {seed}\n");

    #[cfg(debug_assertions)]
    eprintln!("  warning    : debug build - the large joins take many minutes; use --release");

    let mut table_rows: Vec<Vec<String>> = Vec::with_capacity(sizes.len());
    let started = Instant::now();

    for &n in &sizes {
        let (r, s) = generate_relations(n, n, matches, seed)
            .expect("study sizes are small enough for the generator");
        let mut eng = Engine::new();
        eng.load("R", r);
        eng.load("S", s);

        // Threshold scaled to the size, so each selection keeps about half its
        // tuples.
        let select_q =
            parse_query(&format!("select[a>={}](R)", n / 2)).expect("select query should parse");

        eng.reset_stats();
        let t = Instant::now();
        let output = eng.execute(&join_q).expect("join failed").len();
        let jt = t.elapsed().as_secs_f64();
        let comps = eng.stats.join_comparisons;
        eprintln!("  n = {n:<6} join    in {jt:.3} s ({comps} comparisons - {output} tuples)");
        table_rows.push(vec![n.to_string(), n.to_string(), comps.to_string(), format!("{jt:.3}"), output.to_string()]);

        eng.reset_stats();
        let t = Instant::now();
        let output = eng.execute(&select_q).expect("select failed").len();
        let st = t.elapsed().as_secs_f64();
        let comps = eng.stats.select_comparisons;
        eprintln!("  n = {n:<6} select  in {st:.3} s ({comps} comparisons - {output} tuples)");

        eng.reset_stats();
        let t = Instant::now();
        let _ = eng.execute(&project_q).expect("project failed");
        let pt = t.elapsed().as_secs_f64();
        eprintln!("  n = {n:<6} project in {pt:.3} s");
    }

    eprintln!("\n  finished all {} sizes in {:.1} s\n", sizes.len(), started.elapsed().as_secs_f64());

    let headers = ["n", "m", "comparisons", "wall time (s)", "output tuples"];
    print_table(&headers, &table_rows);
}

/// Print a Markdown table with these `headers` and `rows`.
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

/// R(a, b) and S(b, c) for the §8.1 experiment, sized so that each R tuple
/// joins with about `expected_matches` S tuples on average.
///
/// Errors: a message when `m` distinct S tuples cannot be drawn at that match
/// rate, i.e. when the (b, c) value space is smaller than `m`.
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

    // S must be a set, so m distinct (b, c) pairs have to exist.
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

    let mut r = Relation::new(["a", "b"]).expect("generated header names are distinct");
    for row in r_rows {
        r.insert(row).expect("generated rows are well-formed");
    }
    let mut s = Relation::new(["b", "c"]).expect("generated header names are distinct");
    for row in s_rows {
        s.insert(row).expect("generated rows are well-formed");
    }
    Ok((r, s))
}

/// A small deterministic PRNG (xorshift64*), so a run is reproducible from its
/// seed.
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`; 0 is treated as 1, which is a fixed
    /// point of the generator.
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    /// The next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// A pseudo-random value in `lo..hi_exclusive`.
    pub fn next_range(&mut self, lo: i64, hi_exclusive: i64) -> i64 {
        debug_assert!(hi_exclusive > lo);
        let span = (hi_exclusive - lo) as u64;
        lo + (self.next_u64() % span) as i64
    }
}

/// Run the subcommand given on the command line.
///
/// Errors: whatever the `write` subcommand reports.
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

    /// The generator produces exactly the requested tuple counts and headers.
    #[test]
    fn generates_the_requested_tuple_counts() {
        let (r, s) = generate_relations(100, 50, 2.0, SEED).unwrap();
        assert_eq!(r.schema(), ["a", "b"]);
        assert_eq!(s.schema(), ["b", "c"]);
        assert_eq!(r.len(), 100);
        assert_eq!(s.len(), 50);
        assert!(r.iter().all(|row| row.len() == 2));
    }

    /// The match rate picks the b-domain, so the join really does produce
    /// about `n × matches` tuples.
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

    /// Generated relations reload through the §4.1 syntax unchanged.
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

    /// S holds no duplicate tuple, so it stays a set of the requested size
    /// even when n is much larger than the b-domain.
    #[test]
    fn duplicate_b_pairs_are_excluded_from_s() {
        let (_, s) = generate_relations(1, 1000, 2.0, SEED).unwrap();
        assert_eq!(s.len(), 1000);
        let mut seen = std::collections::HashSet::new();
        for row in s.iter() {
            assert!(seen.insert(row.clone()), "duplicate tuple in S: {row:?}");
        }
    }

    /// Generated relations load into the engine and answer a query.
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
