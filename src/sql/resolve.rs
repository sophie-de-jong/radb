//! Name resolution: the scope a `FROM` clause builds and the rules that turn a
//! written name into exactly one column (§3).
//!
//! A [`Scope`] is the ordered list of [`Entry`] the `FROM` clause contributed.
//! Each entry is keyed by its alias, or by its table name when it has none, and
//! holds that entry's columns with fully qualified names (`Emp.age`, `e.name`,
//! `t.n`). Resolution is then the three rules of §3, in order:
//!
//! - a **qualified** name `t.c` needs `t` in scope and `c` in `t`, and the error
//!   names which part failed;
//! - an **unqualified** name `c` needs exactly one entry whose columns include
//!   a column whose bare name is `c`; zero is unknown, two or more is ambiguous
//!   and the message lists the candidates;
//! - **`SELECT *`** is not an error when two entries share a bare column name,
//!   because expansion qualifies every column, so the scope exposes its columns
//!   in order rather than resolving them.
//!
//! After resolution no unqualified name survives: a [`Column`] is returned with
//! its qualified name, which is what the logical plan carries.

use super::tokenizer::Position;
use crate::schema::Column;

use super::ast::{ColumnRef, Located};
use super::error::SqlError;

/// One `FROM` entry: a key and the columns it contributes, fully qualified.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The alias, or the table name when there is no alias.
    pub key: String,
    /// The entry's columns, qualified by `key`, in schema order.
    pub columns: Vec<Column>,
    /// Where the entry was written, for a duplicate-key error.
    pub at: Position,
}

/// The scope a `FROM` clause builds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    entries: Vec<Entry>,
}

impl Scope {
    /// An empty scope.
    pub fn new() -> Self {
        Scope::default()
    }

    /// Add an entry.
    ///
    /// Errors: a `name` error if `key` is already in scope, which is §3's
    /// duplicate alias rule (`FROM Emp, Emp`).
    pub fn push(
        &mut self,
        key: String,
        columns: Vec<Column>,
        at: Position,
    ) -> Result<(), SqlError> {
        if self.entries.iter().any(|e| e.key == key) {
            return Err(SqlError::name(
                format!("duplicate table name or alias '{key}'"),
                at,
            ));
        }
        self.entries.push(Entry { key, columns, at });
        Ok(())
    }

    /// Every column of every entry, in `FROM` order then schema order, fully
    /// qualified — the expansion of `SELECT *` (§3).
    pub fn all_columns(&self) -> Vec<Column> {
        self.entries
            .iter()
            .flat_map(|entry| entry.columns.iter().cloned())
            .collect()
    }

    /// Resolve one written column reference to exactly one column.
    ///
    /// Errors: [`SqlError::name`] for an unknown table part, an unknown column
    /// part, or an unqualified name no entry has; [`SqlError::ambiguity`] when
    /// an unqualified name matches more than one entry.
    pub fn resolve(&self, column: &Located<ColumnRef>) -> Result<Column, SqlError> {
        match &column.qualifier {
            Some(qualifier) => {
                let entry = self
                    .entries
                    .iter()
                    .find(|entry| &entry.key == qualifier)
                    .ok_or_else(|| {
                        SqlError::name(format!("unknown table or alias '{qualifier}'"), column.at())
                    })?;
                let qualified = format!("{qualifier}.{}", column.name);
                entry
                    .columns
                    .iter()
                    .find(|c| c.name == qualified)
                    .cloned()
                    .ok_or_else(|| {
                        SqlError::name(
                            format!("table '{qualifier}' has no column '{}'", column.name),
                            column.at(),
                        )
                    })
            }
            None => {
                let matches: Vec<&Column> = self
                    .entries
                    .iter()
                    .flat_map(|entry| entry.columns.iter())
                    .filter(|c| bare_name(&c.name) == column.name)
                    .collect();
                match matches.len() {
                    0 => Err(SqlError::name(
                        format!("unknown column '{}'", column.name),
                        column.at(),
                    )),
                    1 => Ok(matches[0].clone()),
                    _ => {
                        let candidates: Vec<&str> =
                            matches.iter().map(|c| c.name.as_str()).collect();
                        Err(SqlError::ambiguity(
                            format!(
                                "column '{}' is ambiguous: {}",
                                column.name,
                                candidates.join(", ")
                            ),
                            column.at(),
                        ))
                    }
                }
            }
        }
    }
}

/// The part of a qualified name after the last dot.
pub fn bare_name(qualified: &str) -> &str {
    match qualified.rsplit_once('.') {
        Some((_, bare)) => bare,
        None => qualified,
    }
}

/// Re-qualify `columns` under `key`, keeping their bare names and types.
///
/// This is how a table alias (`FROM Emp e`) or a `FROM` subquery alias turns an
/// inner schema into the schema the outer query addresses.
pub fn requalify(key: &str, columns: impl IntoIterator<Item = Column>) -> Vec<Column> {
    columns
        .into_iter()
        .map(|column| Column {
            name: format!("{key}.{}", bare_name(&column.name)),
            ty: column.ty,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColumnType;

    fn col(name: &str, ty: ColumnType) -> Column {
        Column {
            name: name.to_string(),
            ty,
        }
    }

    fn position(col: usize) -> Position {
        Position { line: 1, col }
    }

    fn colref(qualifier: Option<&str>, name: &str) -> Located<ColumnRef> {
        Located::new(
            ColumnRef {
                qualifier: qualifier.map(str::to_string),
                name: name.to_string(),
            },
            position(0),
        )
    }

    fn scope() -> Scope {
        let mut scope = Scope::new();
        scope
            .push(
                "Emp".into(),
                vec![col("Emp.a", ColumnType::Int), col("Emp.b", ColumnType::Int)],
                position(0),
            )
            .unwrap();
        scope
            .push(
                "Dept".into(),
                vec![
                    col("Dept.a", ColumnType::Int),
                    col("Dept.c", ColumnType::Int),
                ],
                position(10),
            )
            .unwrap();
        scope
    }

    /// A qualified name resolves exactly.
    #[test]
    fn qualified_names_resolve() {
        let got = scope().resolve(&colref(Some("Emp"), "a")).unwrap();
        assert_eq!(got.name, "Emp.a");
    }

    /// An unqualified name in two entries is ambiguous, and the message names
    /// both candidates.
    #[test]
    fn unqualified_ambiguity_names_the_candidates() {
        let err = scope().resolve(&colref(None, "a")).unwrap_err();
        assert_eq!(err.category, super::super::error::Category::Ambiguity);
        assert!(err.message.contains("Emp.a"), "{err}");
        assert!(err.message.contains("Dept.a"), "{err}");
    }

    /// An unqualified name in exactly one entry resolves to it.
    #[test]
    fn unqualified_unique_name_resolves() {
        assert_eq!(scope().resolve(&colref(None, "b")).unwrap().name, "Emp.b");
        assert_eq!(scope().resolve(&colref(None, "c")).unwrap().name, "Dept.c");
    }

    /// An unknown table part and an unknown column part are different errors.
    #[test]
    fn unknown_table_and_column_parts_are_distinct() {
        let missing_table = scope().resolve(&colref(Some("Nope"), "a")).unwrap_err();
        assert!(missing_table.message.contains("table"), "{missing_table}");
        let missing_column = scope().resolve(&colref(Some("Emp"), "zz")).unwrap_err();
        assert!(
            missing_column.message.contains("column"),
            "{missing_column}"
        );
    }

    /// `SELECT *` expansion has every qualified column, shared bare names and
    /// all.
    #[test]
    fn star_expansion_qualifies_every_column() {
        let names: Vec<String> = scope().all_columns().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["Emp.a", "Emp.b", "Dept.a", "Dept.c"]);
    }

    /// A duplicate key is rejected.
    #[test]
    fn duplicate_keys_are_rejected() {
        let mut scope = Scope::new();
        scope
            .push(
                "Emp".into(),
                vec![col("Emp.a", ColumnType::Int)],
                position(0),
            )
            .unwrap();
        let err = scope
            .push(
                "Emp".into(),
                vec![col("Emp.a", ColumnType::Int)],
                position(9),
            )
            .unwrap_err();
        assert_eq!(err.at, position(9));
    }
}
