//! `radb-storage` — the Module 1 command-line tool.
//!
//! ```text
//!   radb-storage create-table --db DIR --name R --schema "a:INT,b:VARCHAR(20)"
//!   radb-storage load         --db DIR --table R --file r.csv
//!   radb-storage scan         --db DIR --table R --frames 8 [--limit N] --json
//!   radb-storage layout       --db DIR --table R --page 3
//!   radb-storage trace        --db DIR --file trace.txt --frames 16 --policy lru|clock --json
//! ```
//!
//! `radb-storage --help` prints the full flag list.

use std::error::Error;
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser as ClapParser, Subcommand};

use radb::Database;
use radb::Schema;
use radb::storage::{BufferPool, Policy};

/// A Project 2 storage engine (pages, heap files, buffer pool).
#[derive(ClapParser)]
#[command(
    name = "radb-storage",
    version,
    about = "Project 2 storage engine (create-table, load, scan, layout, trace)"
)]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The subcommands, matching §7 of the module specification.
#[derive(Subcommand)]
enum Command {
    /// Create a table and its catalog entry.
    CreateTable {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The table name.
        #[arg(long)]
        name: String,
        /// The schema, for example `a:INT,b:VARCHAR(20)`.
        #[arg(long)]
        schema: String,
        /// The buffer-pool size (not in the spec's invocation; defaults to 16).
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
    /// Bulk-load a CSV file into a table.
    Load {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The table name.
        #[arg(long)]
        table: String,
        /// The CSV file to read.
        #[arg(long)]
        file: PathBuf,
        /// The buffer-pool size (not in the spec's invocation; defaults to 16).
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
    /// Scan a table and print its rows and buffer statistics.
    Scan {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The table name.
        #[arg(long)]
        table: String,
        /// The buffer-pool size.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        /// Stop after this many rows.
        #[arg(long)]
        limit: Option<usize>,
        /// Print the JSON shape required by §7.
        #[arg(long)]
        json: bool,
    },
    /// Print the byte-level layout of one page.
    Layout {
        /// The database directory.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The table name.
        #[arg(long)]
        table: String,
        /// The page id to dump.
        #[arg(long)]
        page: u32,
        /// The buffer-pool size.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
    /// Replay a trace of page ids through the buffer pool.
    Trace {
        /// The database directory (documented invocation, §7: the trace runs
        /// on a synthetic disk and never opens it).
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The trace file, one page id per line.
        #[arg(long)]
        file: PathBuf,
        /// The buffer-pool size.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
        /// The replacement policy: `lru` or `clock`.
        #[arg(long, default_value = "lru")]
        policy: String,
        /// Print the JSON shape required by §7.
        #[arg(long)]
        json: bool,
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
        Command::CreateTable {
            db,
            name,
            schema,
            frames,
        } => {
            let schema: Schema = schema.parse()?;
            let arity = schema.arity();
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            db.create_table(&name, schema)?;
            println!("created table {name} ({arity} columns)");
            Ok(())
        }
        Command::Load {
            db,
            table,
            file,
            frames,
        } => {
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            let inserted = db.load_table(&table, &file)?;
            println!("loaded {inserted} rows into {table}");
            Ok(())
        }
        Command::Scan {
            db,
            table,
            frames,
            limit,
            json,
        } => {
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            let report = db.scan_table(&table, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report.json()).expect("a report is JSON")
                );
            } else {
                println!(
                    "{} ({})",
                    report.table,
                    report
                        .schema
                        .columns()
                        .iter()
                        .map(|c| format!("{}:{}", c.name, c.ty))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                for row in &report.rows {
                    println!(
                        "{}",
                        row.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
                let stats = report.stats;
                println!(
                    "{} row(s); hits={} misses={} evictions={} reads={} writes={}",
                    report.rows.len(),
                    stats.hits,
                    stats.misses,
                    stats.evictions,
                    stats.reads,
                    stats.writes
                );
            }
            Ok(())
        }
        Command::Layout {
            db,
            table,
            page,
            frames,
        } => {
            let mut db = Database::open_with(&db, frames.get(), Policy::Lru)?;
            print!("{}", db.page_layout(&table, page)?);
            Ok(())
        }
        Command::Trace {
            db,
            file,
            frames,
            policy,
            json,
        } => {
            // `--db` is part of the documented invocation (§7) but the trace
            // runs on a synthetic disk, so there is no directory to use.
            let _ = db;
            let policy = policy.parse()?;
            let mut pool = BufferPool::new(frames.get(), policy);
            let f = pool.open_null();

            let text = fs::read_to_string(&file)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", file.display())))?;
            pool.replay_trace(f, &text)?;

            let s = pool.stats();
            if json {
                let value = serde_json::json!({
                    "policy": policy.as_str(),
                    "frames": frames,
                    "hits": s.hits,
                    "misses": s.misses,
                    "evictions": s.evictions,
                    "reads": s.reads,
                    "writes": s.writes,
                });
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            } else {
                println!(
                    "policy={} frames={} hits={} misses={} evictions={} reads={} writes={}",
                    policy.as_str(),
                    frames,
                    s.hits,
                    s.misses,
                    s.evictions,
                    s.reads,
                    s.writes
                );
            }
            Ok(())
        }
    }
}
