//! `radb` — the integrated database shell.
//!
//! The shell parses a small command language, splits SQL input into statements,
//! executes each one through [`Database`], and presents every result with the
//! shared tree printer.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use radb::optimize::analyze::DEFAULT_BUCKETS;
use radb::storage::stats::stats_tree;
use radb::{Database, IndentTree};

#[derive(Parser)]
#[command(name = "radb", version, about = "Integrated relational database shell")]
struct Args {
    /// Database directory
    #[arg(long, short = 'd', default_value = ".")]
    db: PathBuf,
    /// The replacement policy: `lru` or `clock`.
    #[arg(long, default_value = "lru")]
    policy: String,
}

/// One shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// A SQL statement (or several, separated by `;`).
    Sql(String),
    /// `EXPLAIN [ANALYZE] query`
    Explain { analyze: bool, query: String },
    /// `ANALYZE TABLE name`
    AnalyzeTable { name: String },
    /// `SET frames = n`
    SetFrames { frames: usize },
    /// `LOAD TABLE name FROM 'file.csv'`
    LoadTable { table: String, file: String },
    /// `exit` or `quit`
    Exit,
    /// `help`
    Help,
}

fn main() -> ExitCode {
    match cli() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn cli() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut db = Database::open_with(&args.db, 32, args.policy.parse()?)?;

    println!("radb - Integrated Database System");
    println!("Type 'exit' or 'quit' to exit, 'help' for help.");
    loop {
        print!("radb> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_command(line) {
            Ok(Command::Exit) => break,
            Ok(Command::Help) => print_help(),
            Ok(Command::Sql(sql)) => {
                let mut printed_result = false;
                for statement in split_sql(&sql) {
                    match db.execute_sql(statement) {
                        Ok(rows) => {
                            if !rows.is_empty() {
                                if printed_result {
                                    println!();
                                }
                                printed_result = true;
                                for row in rows {
                                    let line: Vec<String> =
                                        row.iter().map(ToString::to_string).collect();
                                    println!("{}", line.join("\t"));
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!("Error: {err}");
                            break;
                        }
                    }
                }
            }
            Ok(Command::Explain { analyze, query }) => match db.explain(&query, analyze) {
                Ok(explain) => {
                    println!("Query Plan:");
                    print!("{}", IndentTree::new(&explain));
                }
                Err(err) => eprintln!("Error: {err}"),
            },
            Ok(Command::AnalyzeTable { name }) => {
                println!("analyzing table {name}...");
                match db.analyze(&name, DEFAULT_BUCKETS) {
                    Ok(()) => match db.catalog().require(&name) {
                        Ok(meta) => {
                            let tree = stats_tree(&name, &meta.schema, meta.stats.as_ref().expect("we just analyzed"));
                            print!("{}", IndentTree::new(&tree));
                        }
                        Err(err) => eprintln!("Error: {err}"),
                    },
                    Err(err) => eprintln!("Error: {err}"),
                }
            }
            Ok(Command::SetFrames { frames }) => match db.set_frames(frames) {
                Ok(()) => println!("frames set to {frames}"),
                Err(err) => eprintln!("Error: {err}"),
            },
            Ok(Command::LoadTable { table, file }) => {
                println!("loading {table} from {file}...");
                match db.load_table(&table, Path::new(&file)) {
                    Ok(inserted) => println!("loaded {inserted} rows into {table}"),
                    Err(err) => eprintln!("Error: {err}"),
                }
            }
            Err(err) => eprintln!("Error: {err}"),
        }
    }
    Ok(())
}

/// Parse one shell input line.
fn parse_command(input: &str) -> Result<Command, String> {
    let trimmed = input.trim().trim_end_matches(';').trim();
    let mut words = trimmed.split_whitespace().peekable();
    let Some(head) = words.next() else {
        return Err("empty input".to_string());
    };
    match head.to_ascii_lowercase().as_str() {
        "exit" | "quit" => Ok(Command::Exit),
        "help" => Ok(Command::Help),
        "set" => parse_set(&mut words),
        "analyze" => parse_analyze(&mut words),
        "load" => parse_load(&mut words),
        "explain" => parse_explain(&mut words),
        _ => Ok(Command::Sql(trimmed.to_string())),
    }
}

/// `SET frames = n`; `frames=n` is also accepted.
fn parse_set<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<Command, String> {
    let keyword = words
        .next()
        .ok_or_else(|| "missing argument: frames".to_string())?;
    let (prefix, inline) = match keyword.split_once('=') {
        Some((prefix, value)) => (prefix, Some(value)),
        None => (keyword, None),
    };
    if !prefix.eq_ignore_ascii_case("frames") {
        return Err("SET only supports 'frames'".to_string());
    }
    let value = match inline {
        Some(value) => value,
        None => {
            let token = words
                .next()
                .ok_or_else(|| "expected '=' after 'frames'".to_string())?;
            token
                .strip_prefix('=')
                .or_else(|| token.eq_ignore_ascii_case("=").then_some(""))
                .ok_or_else(|| "expected '=' after 'frames'".to_string())?
        }
    };
    let value = if value.is_empty() {
        words
            .next()
            .ok_or_else(|| "missing frame count".to_string())?
    } else {
        value
    };
    let frames = value
        .parse::<usize>()
        .map_err(|_| "invalid frame count".to_string())?;
    Ok(Command::SetFrames { frames })
}

/// `ANALYZE TABLE name`.
fn parse_analyze<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<Command, String> {
    let keyword = words
        .next()
        .ok_or_else(|| "ANALYZE only supports 'TABLE'".to_string())?;
    if !keyword.eq_ignore_ascii_case("table") {
        return Err("ANALYZE only supports 'TABLE'".to_string());
    }
    let name = words
        .next()
        .ok_or_else(|| "missing argument: table name".to_string())?;
    Ok(Command::AnalyzeTable {
        name: name.to_string(),
    })
}

/// `LOAD TABLE name FROM 'file.csv'`.
fn parse_load<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<Command, String> {
    let keyword = words
        .next()
        .ok_or_else(|| "LOAD only supports 'TABLE'".to_string())?;
    if !keyword.eq_ignore_ascii_case("table") {
        return Err("LOAD only supports 'TABLE'".to_string());
    }
    let table = words
        .next()
        .ok_or_else(|| "missing argument: table name".to_string())?;
    let keyword = words
        .next()
        .ok_or_else(|| "expected 'FROM' after table name".to_string())?;
    if !keyword.eq_ignore_ascii_case("from") {
        return Err("expected 'FROM' after table name".to_string());
    }
    let file = words.collect::<Vec<_>>().join(" ");
    let file = file
        .strip_prefix('\'')
        .and_then(|file| file.strip_suffix('\''))
        .ok_or_else(|| "file path must be single-quoted".to_string())?;
    Ok(Command::LoadTable {
        table: table.to_string(),
        file: file.to_string(),
    })
}

/// `EXPLAIN [ANALYZE] query`.
fn parse_explain<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<Command, String> {
    let mut tokens: Vec<&str> = words.collect();
    let analyze = tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("analyze"));
    if analyze {
        tokens.remove(0);
    }
    if tokens.is_empty() {
        return Err("missing argument: query".to_string());
    }
    Ok(Command::Explain {
        analyze,
        query: tokens.join(" "),
    })
}

/// Split a SQL input on semicolons outside single-quoted strings and comments.
fn split_sql(input: &str) -> Vec<&str> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut in_string = false;
    let mut in_comment = false;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if in_comment {
            if ch == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if ch == '\'' {
                if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                    chars.next();
                } else {
                    in_string = false;
                }
            }
            continue;
        }
        match ch {
            '\'' => in_string = true,
            '-' if chars.peek().is_some_and(|(_, next)| *next == '-') => {
                chars.next();
                in_comment = true;
            }
            ';' => {
                let statement = input[start..index].trim();
                if !statement.is_empty() {
                    statements.push(statement);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    let statement = input[start..].trim();
    if !statement.is_empty() {
        statements.push(statement);
    }
    statements
}

/// The shell's help text.
fn print_help() {
    println!("Commands:");
    println!("  CREATE TABLE name (col TYPE, ...);");
    println!("  INSERT INTO name VALUES (...);");
    println!("  SELECT ...;");
    println!("  EXPLAIN [ANALYZE] SELECT ...;");
    println!("  ANALYZE TABLE name;");
    println!("  SET frames = n;");
    println!("  LOAD TABLE name FROM 'file.csv';");
    println!("  exit/quit");
}
