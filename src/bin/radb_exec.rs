//! `radb-exec` — the Module 2 command-line tool.
//!
//! ```text
//!   radb-exec run   --db DIR --plan plan.json --frames 32 --json
//!   radb-exec trace --db DIR --plan plan.json --frames 32
//! ```
//!
//! `run` executes the plan and prints the result (as JSON with `--json`).
//! `trace` executes the plan and prints one line of counters per operator.
//!
//! `radb-exec --help` prints the full flag list. Everything specific to the
//! command line lives here; the library owns the engine.

use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser as ClapParser, Subcommand};

use radb::Database;
use radb::IndentTree;
use radb::exec::{Counters, Plan};
use radb::storage::Policy;
use radb::tree::tree_from_flat;

/// A Project 2 execution engine (iterators, sorting, joins, one memory budget).
#[derive(ClapParser)]
#[command(
    name = "radb-exec",
    version,
    about = "Project 2 execution engine (run, trace)"
)]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The two subcommands of §4.
#[derive(Subcommand)]
enum Command {
    /// Run a plan and print its result.
    Run {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The plan file, a JSON document.
        #[arg(long)]
        plan: PathBuf,
        /// The memory budget, in frames.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        /// Print the JSON shape required by §4.
        #[arg(long)]
        json: bool,
    },
    /// Run a plan and print per-operator counters.
    Trace {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The plan file, a JSON document.
        #[arg(long)]
        plan: PathBuf,
        /// The memory budget, in frames.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
}

/// The default number of frames when `--frames` is omitted.
const DEFAULT_FRAMES: NonZeroUsize = NonZeroUsize::new(16).unwrap();

fn main() -> ExitCode {
    match cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn cli() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    match args.command {
        Command::Run {
            db,
            plan,
            frames,
            json,
        } => {
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            let plan = fs::read_to_string(&plan)
                .map_err(|e| format!("{}: {e}", plan.display()))?
                .parse::<Plan>()?;
            let mut exec = db.executor(&plan)?;

            let rows = exec.by_ref().collect::<Result<Vec<_>, _>>()?;
            let schema = exec.schema();
            let totals = Counters::totals_for(
                rows.len() as u64,
                exec.walk().map(|(_, node)| node.counters()),
            );

            if json {
                let rows: Vec<serde_json::Value> = rows.iter().map(|row| row.json()).collect();
                let doc = serde_json::json!({
                    "schema": schema.json(),
                    "rows": rows,
                    "totals": {
                        "tuples_out": totals.tuples_out,
                        "comparisons": totals.comparisons,
                        "page_reads": totals.page_reads,
                        "page_writes": totals.page_writes,
                    },
                    "peak_frames": exec.peak_frames(),
                });
                println!("{}", serde_json::to_string(&doc)?);
            } else {
                for row in &rows {
                    for value in row.iter() {
                        print!("{value}\t")
                    }
                    println!();
                }
            }
            Ok(())
        }
        Command::Trace { db, plan, frames } => {
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            let plan = fs::read_to_string(&plan)
                .map_err(|e| format!("{}: {e}", plan.display()))?
                .parse::<Plan>()?;
            let mut exec = db.executor(&plan)?;

            for row in &mut exec {
                row?;
            }

            let lines = exec
                .walk()
                .map(|(depth, node)| (depth, format!("{node}\t{}", node.counters())))
                .collect();
            if let Some(tree) = tree_from_flat(lines) {
                print!("{}", IndentTree::new(&tree));
            }
            Ok(())
        }
    }
}
