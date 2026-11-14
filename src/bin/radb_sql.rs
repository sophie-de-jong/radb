//! `radb-sql` — the Module 3 command-line tool.
//!
//! ```text
//!   radb-sql parse   --query "SELECT ..." --explain
//!   radb-sql compile --db DIR --file q.sql --json
//!   radb-sql check   --db DIR --file q.sql
//! ```
//!
//! `parse` scans and parses a query without a catalog: with `--explain` it prints
//! the indented syntax tree, and without it the token stream. `compile` reads a
//! SQL file, checks it against `--db`, builds the logical plan and prints its
//! JSON. `check` does the same but prints only errors, exiting 1 on the first
//! one, in the one format §6 fixes:
//!
//! ```text
//! error: <category>: <message> at line L, column C
//! ```
//!
//! Everything specific to the command line lives here; the library owns the
//! front end. `radb-sql --help` prints the full flag list.

use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser as ClapParser, Subcommand};

use radb::sql::tokenizer::tokenize;
use radb::sql::{render_statement, render_syntax, statement_json};
use radb::storage::Policy;
use radb::{Compiler, Database, Statement};

/// A Project 2 SQL front end (parse, compile, check).
#[derive(ClapParser)]
#[command(
    name = "radb-sql",
    version,
    about = "Project 2 SQL front end (parse, compile, check)"
)]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// The three subcommands of §8.
#[derive(Subcommand)]
enum Command {
    /// Parse a query and print its tokens or its syntax tree.
    Parse {
        /// The query text.
        #[arg(long)]
        query: String,
        /// Print the indented syntax tree instead of the token stream.
        #[arg(long)]
        explain: bool,
    },
    /// Check a SQL file and render its logical plans.
    Compile {
        /// The database directory holding `catalog.db`.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The SQL file to compile.
        #[arg(long)]
        file: PathBuf,
        /// Print the plan as JSON (otherwise an indented tree).
        #[arg(long)]
        json: bool,
        /// The buffer-pool size used to read the catalog.
        #[arg(long, default_value_t = DEFAULT_FRAMES)]
        frames: NonZeroUsize,
    },
    /// Resolve and check a SQL file, printing only errors.
    Check {
        /// The database directory holding `catalog.db`.
        #[arg(long, default_value = ".")]
        db: PathBuf,
        /// The SQL file to check.
        #[arg(long)]
        file: PathBuf,
        /// The buffer-pool size used to read the catalog.
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
        Command::Parse { query, explain } => {
            if explain {
                print!("{}", render_syntax(&query)?);
            } else {
                // The token stream is the scanner's output alone, so it is worth
                // printing even for input that does not parse.
                for token in tokenize(&query)? {
                    println!(
                        "line {}, column {}: {}",
                        token.at.line,
                        token.at.col,
                        token.kind.describe()
                    );
                }
            }
            Ok(())
        }
        Command::Compile {
            db,
            file,
            json,
            frames,
        } => {
            let checked = compile(&db, &file, frames)?;
            for statement in &checked {
                if json {
                    println!("{}", statement_json(statement));
                } else {
                    print!("{}", render_statement(statement));
                }
            }
            Ok(())
        }
        Command::Check { db, file, frames } => {
            compile(&db, &file, frames)?;
            Ok(())
        }
    }
}

/// Parse and check `file` against the catalog in `db`.
fn compile(db: &Path, file: &Path, frames: NonZeroUsize) -> Result<Vec<Statement>, Box<dyn Error>> {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let db = Database::open_with(db, frames.get(), Policy::Lru)?;
    let mut compiler = Compiler::new(&text)?;
    compiler.compile_all(db.catalog()).map_err(Into::into)
}
