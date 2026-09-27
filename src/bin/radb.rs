//! `radb` — the command-line front end.
//!
//! ```text
//!   radb --tree "QUERY"                 print the parse tree, do not execute
//!   radb -r R.txt -r S.txt "QUERY"      load relations (spec §4.1), run, print result
//! ```
//!
//! `radb --help` prints the full flag list (clap).

use std::{fmt, fs};
use std::path::PathBuf;

use clap::Parser as ClapParser;

use radb::parse_relation;
use radb::{parse_query, Engine, ParseError, SemanticError};

/// A small relational-algebra query engine (radb).
#[derive(ClapParser)]
#[command(name = "radb", version, about = "relational algebra engine (radb)")]
struct Args {
    /// Print the parse tree for QUERY without executing it.
    #[arg(long)]
    tree: bool,

    /// Load a relation-definition file (spec §4.1); may be repeated.
    #[arg(short = 'r', long = "relation", value_name = "REL_FILE")]
    relations: Vec<PathBuf>,

    /// The query to parse and run, e.g. "project[Name](select[Age>30](E))".
    #[arg(trailing_var_arg = true, value_name = "QUERY")]
    query: Vec<String>,
}

/// A failure message. A failed `main` is reported as `Error: <message>`, which
/// uses the `Debug` form, so `Debug` prints the message and never a stack
/// trace.
struct AppError(String);

impl AppError {
    /// An error carrying `message`.
    fn new(message: impl Into<String>) -> Self {
        AppError(message.into())
    }
}

impl fmt::Display for AppError {
    /// Renders the message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for AppError {
    /// Renders the message, so a failed `main` prints no stack trace.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppError {}

impl From<ParseError> for AppError {
    /// Carries the parse error's message.
    fn from(err: ParseError) -> Self {
        AppError(err.to_string())
    }
}

impl From<SemanticError> for AppError {
    /// Carries the semantic error's message.
    fn from(err: SemanticError) -> Self {
        AppError(err.to_string())
    }
}

/// Print the parse tree of QUERY with `--tree`, otherwise load the given
/// relation files and run QUERY against them.
///
/// Errors: [`AppError`] carrying the message of the first failure — a missing
/// query, an unreadable file, or a parse or semantic error.
fn main() -> Result<(), AppError> {
    let args = Args::parse();
    if args.query.is_empty() {
        return Err(AppError::new(
            "no query given (see `radb --help`); a typical invocation is\n  \
             radb --tree \"project[Name](select[Age>30](Employees))\"",
        ));
    }
    let query = args.query.join(" ");

    let expr = parse_query(&query)?;
    if args.tree {
        println!("{expr}");
        return Ok(());
    }

    let mut eng = Engine::new();
    for file in &args.relations {
        let text = fs::read_to_string(file)
            .map_err(|e| AppError::new(format!("cannot read {}: {e}", file.display())))?;
        let (name, relation) = parse_relation(&text)?;
        eng.load(&name, relation);
    }

    let result = eng.execute(&expr)?;
    println!("{result}");
    Ok(())
}
