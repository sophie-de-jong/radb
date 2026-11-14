//! The bulk loader's CSV reader (M1 §6).
//!
//! One record per line, fields separated by commas, a field with a comma wrapped
//! in double quotes, `""` inside a quoted field meaning one literal quote. A
//! field that does not match its column type, or a record too long for a page,
//! is an error naming the line. Shared by `radb-storage load` and the shell's
//! `LOAD TABLE`.
//!
//! [`CsvReader`] borrows the whole input once and yields typed rows, so a
//! caller does not copy the file or split it into an intermediate `Vec<String>`.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::str::Lines;

use crate::Value;
use crate::schema::{ColumnType, Schema};

/// A malformed CSV row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvError {
    line: u64,
    column: usize,
    message: String,
}

impl CsvError {
    fn new(line: u64, column: usize, message: impl Into<String>) -> Self {
        CsvError {
            line,
            column,
            message: message.into(),
        }
    }

    /// The 1-based line number.
    pub fn line(&self) -> u64 {
        self.line
    }

    /// The 1-based column number.
    pub fn column(&self) -> usize {
        self.column
    }
}

impl fmt::Display for CsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl Error for CsvError {}

/// An iterator of typed rows over CSV text.
///
/// The reader borrows `input` and the schema; each item owns only the values it
/// produces.
pub struct CsvReader<'a> {
    schema: &'a Schema,
    lines: Lines<'a>,
    line_no: u64,
}

impl<'a> CsvReader<'a> {
    /// Read `input` as rows shaped by `schema`.
    pub fn new(schema: &'a Schema, input: &'a str) -> Self {
        CsvReader {
            schema,
            lines: input.lines(),
            line_no: 0,
        }
    }
}

impl Iterator for CsvReader<'_> {
    type Item = Result<Vec<Value>, CsvError>;

    fn next(&mut self) -> Option<Self::Item> {
        let line = self.lines.next()?;
        self.line_no += 1;
        let bytes = line.as_bytes();
        let mut values = Vec::with_capacity(self.schema.arity());
        let mut pos = 0usize;
        let mut field_index = 0usize;

        while pos < bytes.len() {
            let start = pos;
            let field: Cow<'_, str>;
            if bytes[start] != b'"' {
                // An unquoted field runs to the next comma.
                let end = bytes[start..]
                    .iter()
                    .position(|&b| b == b',')
                    .map_or(bytes.len(), |off| start + off);
                pos = if end == bytes.len() {
                    bytes.len()
                } else {
                    end + 1
                };
                field = Cow::Borrowed(&line[start..end]);
            } else {
                // A quoted field: find the closing quote, treating `""` as one
                // literal `"`.
                let mut i = start + 1;
                let mut escaped = false;
                loop {
                    match bytes.get(i) {
                        None => {
                            return Some(Err(CsvError::new(
                                self.line_no,
                                1,
                                "unterminated quoted field",
                            )));
                        }
                        Some(&b'"') if bytes.get(i + 1) == Some(&b'"') => {
                            escaped = true;
                            i += 2;
                        }
                        Some(&b'"') => break,
                        Some(_) => i += 1,
                    }
                }
                let close = i;
                // Anything between the closing quote and the next comma is
                // literal.
                let end = bytes[close + 1..]
                    .iter()
                    .position(|&b| b == b',')
                    .map_or(bytes.len(), |off| close + 1 + off);
                pos = if end == bytes.len() {
                    bytes.len()
                } else {
                    end + 1
                };

                field = if escaped || close + 1 != end {
                    let mut owned = String::new();
                    let mut rest = &line[start + 1..close];
                    while let Some(at) = rest.find('"') {
                        owned.push_str(&rest[..at]);
                        owned.push('"');
                        let skip = if rest.as_bytes().get(at + 1) == Some(&b'"') {
                            2
                        } else {
                            1
                        };
                        rest = &rest[at + skip..];
                    }
                    owned.push_str(rest);
                    owned.push_str(&line[close + 1..end]);
                    Cow::Owned(owned)
                } else {
                    Cow::Borrowed(&line[start + 1..close])
                };
            }

            if let Some(col) = self.schema.columns().get(field_index) {
                let column = field_index + 1;
                match col.ty {
                    ColumnType::Int => {
                        let parsed = field.trim().parse::<i32>().map_err(|_| {
                            CsvError::new(
                                self.line_no,
                                column,
                                format!("column '{}' is not a 32-bit INT: '{field}'", col.name),
                            )
                        });
                        match parsed {
                            Ok(value) => values.push(Value::Int(value)),
                            Err(error) => return Some(Err(error)),
                        }
                    }
                    ColumnType::Varchar(n) => {
                        if field.len() > n as usize {
                            return Some(Err(CsvError::new(
                                self.line_no,
                                column,
                                format!(
                                    "column '{}' is {} bytes but is declared VARCHAR({n})",
                                    col.name,
                                    field.len()
                                ),
                            )));
                        }
                        values.push(Value::Varchar(field.into_owned()));
                    }
                }
            }
            field_index += 1;
        }

        if field_index != self.schema.arity() {
            return Some(Err(CsvError::new(
                self.line_no,
                field_index.min(self.schema.arity()) + 1,
                format!(
                    "expected {} field(s) for {}, found {field_index}",
                    self.schema.arity(),
                    self.schema.names().collect::<Vec<_>>().join(",")
                ),
            )));
        }
        Some(Ok(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        "a:INT,b:VARCHAR(20)".parse().unwrap()
    }

    #[test]
    fn reads_quoted_and_escaped_fields() {
        let rows: Vec<Vec<Value>> =
            CsvReader::new(&schema(), "1,\"hello, world\"\n2,\"say \"\"hi\"\"\"\n")
                .collect::<Result<_, _>>()
                .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Varchar("hello, world".into())],
                vec![Value::Int(2), Value::Varchar("say \"hi\"".into())],
            ]
        );
    }

    #[test]
    fn bad_int_names_the_line_and_column() {
        let error = CsvReader::new(&schema(), "1,ok\nnope,x\n")
            .nth(1)
            .unwrap()
            .unwrap_err();
        assert_eq!(error.line(), 2);
        assert_eq!(error.column(), 1);
        assert!(error.to_string().contains("line 2, column 1"));
    }

    #[test]
    fn unterminated_quote_names_the_line() {
        let error = CsvReader::new(&schema(), "1,\"oops\n")
            .next()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.line(), 1);
        assert!(error.to_string().contains("unterminated"));
    }
}
