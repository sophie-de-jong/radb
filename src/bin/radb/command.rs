//! The shell's command line: `exit`, `help`, `EXPLAIN`, `ANALYZE`, `SET`,
//! `LOAD`, and SQL as the fallback.

use std::fmt;
use std::str::FromStr;

/// A shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A SQL statement (or multiple statements).
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

/// Error parsing a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandParseError {
    /// Missing required argument.
    MissingArgument(String),
    /// Invalid argument value.
    InvalidArgument(String),
}

impl fmt::Display for CommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandParseError::MissingArgument(arg) => write!(f, "missing argument: {arg}"),
            CommandParseError::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl std::error::Error for CommandParseError {}

impl FromStr for Command {
    type Err = CommandParseError;

    /// Parse a raw input line. The shell keywords are matched
    /// case-insensitively; anything else is kept as SQL for the front end. A
    /// trailing `;` is ignored.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Err(CommandParseError::InvalidArgument("empty input".into()));
        }

        let (head, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((head, rest)) => (head, rest.trim()),
            None => (trimmed, ""),
        };

        match head.to_ascii_lowercase().as_str() {
            "exit" | "quit" => Ok(Command::Exit),
            "help" => Ok(Command::Help),
            "set" => parse_set_frames(rest),
            "analyze" => parse_analyze_table(rest),
            "load" => parse_load_table(rest),
            "explain" => parse_explain(rest),
            _ => Ok(Command::Sql(input.to_string())),
        }
    }
}

/// Strip a leading keyword, case-insensitively, returning what follows. The
/// keyword must be a whole word: `frames` matches `frames = 3`, not `framesx`.
fn strip_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let head = input.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    match input.as_bytes().get(keyword.len()) {
        None => Some(""),
        Some(b) if !b.is_ascii_alphanumeric() && *b != b'_' => {
            Some(input[keyword.len()..].trim_start())
        }
        _ => None,
    }
}

/// `SET FRAMES = n`.
fn parse_set_frames(rest: &str) -> Result<Command, CommandParseError> {
    let rest = strip_keyword(rest, "frames")
        .ok_or_else(|| CommandParseError::InvalidArgument("SET only supports 'frames'".into()))?;
    let frames = rest
        .strip_prefix('=')
        .ok_or_else(|| CommandParseError::InvalidArgument("expected '=' after 'frames'".into()))?
        .trim()
        .parse::<usize>()
        .map_err(|_| CommandParseError::InvalidArgument("invalid frame count".into()))?;
    Ok(Command::SetFrames { frames })
}

/// `ANALYZE TABLE name`.
fn parse_analyze_table(rest: &str) -> Result<Command, CommandParseError> {
    let name = strip_keyword(rest, "table").ok_or_else(|| {
        CommandParseError::InvalidArgument("ANALYZE only supports 'TABLE'".into())
    })?;
    if name.is_empty() {
        return Err(CommandParseError::MissingArgument("table name".into()));
    }
    Ok(Command::AnalyzeTable {
        name: name.to_string(),
    })
}

/// `LOAD TABLE name FROM 'file.csv'`.
fn parse_load_table(rest: &str) -> Result<Command, CommandParseError> {
    let rest = strip_keyword(rest, "table")
        .ok_or_else(|| CommandParseError::InvalidArgument("LOAD only supports 'TABLE'".into()))?;
    let (table, rest) = rest
        .split_once(char::is_whitespace)
        .ok_or_else(|| CommandParseError::MissingArgument("table name".into()))?;
    let file = strip_keyword(rest.trim(), "from").ok_or_else(|| {
        CommandParseError::InvalidArgument("expected 'FROM' after table name".into())
    })?;
    let file = file
        .strip_prefix('\'')
        .and_then(|file| file.strip_suffix('\''))
        .ok_or_else(|| {
            CommandParseError::InvalidArgument("file path must be single-quoted".into())
        })?;
    Ok(Command::LoadTable {
        table: table.trim().to_string(),
        file: file.to_string(),
    })
}

/// `EXPLAIN [ANALYZE] query`.
fn parse_explain(rest: &str) -> Result<Command, CommandParseError> {
    if let Some(query) = strip_keyword(rest, "analyze") {
        if query.is_empty() {
            return Err(CommandParseError::MissingArgument("query".into()));
        }
        return Ok(Command::Explain {
            analyze: true,
            query: query.to_string(),
        });
    }
    if rest.is_empty() {
        return Err(CommandParseError::MissingArgument("query".into()));
    }
    Ok(Command::Explain {
        analyze: false,
        query: rest.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Command, CommandParseError};
    use std::str::FromStr;

    fn parse(input: &str) -> Command {
        Command::from_str(input).expect("input should parse")
    }

    #[test]
    fn parses_each_command() {
        assert!(matches!(parse("SELECT * FROM R;"), Command::Sql(_)));
        assert!(matches!(parse("exit;"), Command::Exit));
        assert!(matches!(parse("quit"), Command::Exit));
        assert!(matches!(parse("help;"), Command::Help));
        assert!(matches!(
            parse("SET frames = 100;"),
            Command::SetFrames { frames: 100 }
        ));
        assert!(matches!(
            parse("set FRAMES=7"),
            Command::SetFrames { frames: 7 }
        ));
        assert!(matches!(
            parse("EXPLAIN SELECT * FROM R;"),
            Command::Explain { analyze: false, .. }
        ));
        assert!(matches!(
            parse("EXPLAIN ANALYZE SELECT * FROM R;"),
            Command::Explain { analyze: true, .. }
        ));
        assert!(matches!(parse("ANALYZE TABLE R;"), Command::AnalyzeTable { name } if name == "R"));
        assert!(
            matches!(parse("LOAD TABLE R FROM 'file.csv';"), Command::LoadTable { table, file } if table == "R" && file == "file.csv")
        );
    }

    #[test]
    fn explain_keeps_the_query_text() {
        let Command::Explain { analyze, query } =
            parse("EXPLAIN ANALYZE SELECT a FROM R WHERE a > 1;")
        else {
            panic!("expected an EXPLAIN command");
        };
        assert!(analyze);
        assert_eq!(query, "SELECT a FROM R WHERE a > 1");
    }

    #[test]
    fn malformed_commands_report_their_arguments() {
        assert!(matches!(
            Command::from_str("SET x = 1"),
            Err(CommandParseError::InvalidArgument(_))
        ));
        assert!(matches!(
            Command::from_str("ANALYZE TABLE"),
            Err(CommandParseError::MissingArgument(_))
        ));
        assert!(matches!(
            Command::from_str("LOAD TABLE R file.csv"),
            Err(CommandParseError::InvalidArgument(_))
        ));
        assert!(matches!(
            Command::from_str("EXPLAIN"),
            Err(CommandParseError::MissingArgument(_))
        ));
        assert!(matches!(
            Command::from_str("   "),
            Err(CommandParseError::InvalidArgument(_))
        ));
    }
}
