//! `radb-opt` — the Module 4 command-line tool.

use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser as ClapParser, Subcommand};

use radb::optimize::rules::default_rules;
use radb::optimize::{DEFAULT_PASSES, Optimizer, explain, render_dp_tables};
use radb::sql::plan::Plan as LogicalPlan;
use radb::storage::Policy;
use radb::storage::stats::stats_tree;
use radb::{Database, IndentTree};

#[derive(ClapParser)]
#[command(
    name = "radb-opt",
    version,
    about = "Project 2 query optimizer (analyze, plan, explain, dp-table)"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Analyze {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        table: String,
        #[arg(long, default_value_t = 20)]
        buckets: usize,
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
    Plan {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        logical: PathBuf,
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        #[arg(long, default_value_t = DEFAULT_PASSES)]
        passes: usize,
        #[arg(long)]
        json: bool,
        /// Skip rewrites, join reordering and algorithm choice (report baseline).
        #[arg(long)]
        no_optimize: bool,
        /// Disable one numbered rewrite rule (repeatable, for measurements).
        #[arg(long)]
        without_rule: Vec<usize>,
    },
    Explain {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        logical: PathBuf,
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        #[arg(long, default_value_t = DEFAULT_PASSES)]
        passes: usize,
    },
    DpTable {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        logical: PathBuf,
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        #[arg(long, default_value_t = DEFAULT_PASSES)]
        passes: usize,
    },
}

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
        Command::Analyze {
            db,
            table,
            buckets,
            frames,
        } => {
            let mut db = Database::open_with(db, frames.get(), Policy::Lru)?;
            db.analyze(&table, buckets)?;
            let meta = db.catalog().require(&table)?;
            let tree = stats_tree(&table, &meta.schema, meta.stats.as_ref().expect("we just analyzed"));
            print!("{}", IndentTree::new(&tree));
            Ok(())
        }
        Command::Plan {
            db,
            logical,
            frames,
            passes,
            json,
            no_optimize,
            without_rule,
        } => {
            let db = Database::open_with(db, frames.get(), Policy::Lru)?;
            let logical = read_logical(&logical)?;
            let plan = if no_optimize {
                Optimizer::new(logical, db.catalog(), frames.get()).finish_unoptimized()?
            } else {
                let disable_cleanup = without_rule.contains(&7);
                let mut rules = default_rules();
                let mut indexes = without_rule;
                indexes.sort_unstable();
                indexes.dedup();
                for index in indexes.into_iter().rev() {
                    if (1..=rules.len()).contains(&index) {
                        rules.remove(index - 1);
                    }
                }
                let mut optimizer =
                    Optimizer::with_rules(logical, db.catalog(), frames.get(), rules);
                optimizer.set_passes(passes);
                optimizer.set_cleanup(!disable_cleanup);
                optimizer.optimize()?;
                optimizer.finish()?
            };
            if json {
                println!("{}", serde_json::to_string(&plan)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            }
            Ok(())
        }
        Command::Explain {
            db,
            logical,
            frames,
            passes,
        } => {
            let db = Database::open_with(db, frames.get(), Policy::Lru)?;
            let mut optimizer = Optimizer::new(read_logical(&logical)?, db.catalog(), frames.get());
            optimizer.set_passes(passes);
            optimizer.optimize()?;
            let plan = optimizer.finish()?;
            let tree = explain(&plan, db.catalog(), frames.get())?;
            print!("{}", IndentTree::new(&tree));
            Ok(())
        }
        Command::DpTable {
            db,
            logical,
            frames,
            passes,
        } => {
            let db = Database::open_with(db, frames.get(), Policy::Lru)?;
            let mut optimizer = Optimizer::new(read_logical(&logical)?, db.catalog(), frames.get());
            optimizer.set_passes(passes);
            optimizer.optimize()?;
            print!("{}", render_dp_tables(optimizer.dp_tables()?));
            Ok(())
        }
    }
}

/// Read one logical plan from `path`.
fn read_logical(path: &Path) -> Result<LogicalPlan, Box<dyn Error>> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("invalid logical plan: {e}").into())
}
