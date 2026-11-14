//! `radb` — the integrated database shell.
//!
//! SQL text goes in and rows come out. The shell supports the SQL subset plus
//! `EXPLAIN`, `EXPLAIN ANALYZE`, `ANALYZE TABLE`, `SET frames = n` and
//! `LOAD TABLE name FROM file`. It is a prompt around [`Database`].

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use radb::Database;
use radb::optimize::analyze::DEFAULT_BUCKETS;

mod command;
use command::Command;

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
        match line.parse::<Command>() {
            Ok(Command::Exit) => break,
            Ok(Command::Help) => print_help(),
            Ok(Command::Sql(sql)) => match db.execute_sql(&sql) {
                Ok(rows) => {
                    for row in rows {
                        let line: Vec<String> = row.iter().map(ToString::to_string).collect();
                        println!("{}", line.join("\t"));
                    }
                }
                Err(err) => eprintln!("Error: {err}"),
            },
            Ok(Command::Explain { analyze, query }) => match db.explain(&query, analyze) {
                Ok(text) => print!("{text}"),
                Err(err) => eprintln!("Error: {err}"),
            },
            Ok(Command::AnalyzeTable { name }) => {
                println!("analyzing table {name}...");
                match db.analyze(&name, DEFAULT_BUCKETS) {
                    Ok(_) => println!("analyzed table {name}"),
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
