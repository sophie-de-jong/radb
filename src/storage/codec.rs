//! The storage codecs.
//!
//! Two formats live here:
//!
//! - [`RecordCodec`] — the spec-exact heap record format (§2). A record is the
//!   concatenation of its field encodings in schema order, with no header and
//!   no null bitmap:
//!
//!   ```text
//!   INT          4 bytes, signed, two's complement, little-endian
//!   VARCHAR(n)   2-byte unsigned length L, then L bytes of UTF-8, L <= n
//!   ```
//!
//!   The slotted page's free-space and capacity formulas depend on these exact
//!   bytes, so this codec is deliberately schema-driven rather than a generic
//!   serde format.
//!
//! - `BinaryCodec` — the catalog blob format (§6). The catalog format is ours
//!   to design, so it is bincode with fixed-width little-endian integers and
//!   trailing-byte rejection.
//!
//! This module also owns the storage-facing spellings of [`Schema`] and
//! [`ColumnType`] (`spec.parse::<Schema>()`), because the schema type itself
//! lives in the shared [`crate::schema`] layer.

use bincode::Options;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::schema::{ColumnType, Schema};
use crate::value::Value;

use super::error::StorageError;

/// The encoded size of one `INT`, in bytes.
const INT_SIZE: usize = 4;
/// The size of the length prefix on a `VARCHAR`, in bytes.
const VARCHAR_LEN_SIZE: usize = 2;

/// The spec-exact record codec (§2).
pub struct RecordCodec;

impl RecordCodec {
    /// Encode one tuple as a record.
    ///
    /// Errors: a `VARCHAR` longer than its declared length, or a value whose
    /// kind does not match its column.
    pub fn encode(schema: &Schema, fields: &[Value]) -> Result<Vec<u8>, StorageError> {
        if fields.len() != schema.arity() {
            return Err(StorageError::Decode {
                detail: format!(
                    "tuple has {} values but the schema has {} columns",
                    fields.len(),
                    schema.arity()
                ),
            });
        }
        let mut out = Vec::new();
        for (col, value) in schema.columns().iter().zip(fields) {
            match (col.ty, value) {
                (ColumnType::Int, Value::Int(i)) => {
                    out.extend_from_slice(&i.to_le_bytes());
                }
                (ColumnType::Varchar(n), Value::Varchar(s)) => {
                    if s.len() > n as usize {
                        return Err(StorageError::Decode {
                            detail: format!(
                                "string of {} bytes does not fit VARCHAR({n}) column '{}'",
                                s.len(),
                                col.name
                            ),
                        });
                    }
                    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
                    out.extend_from_slice(s.as_bytes());
                }
                (ty, value) => {
                    return Err(StorageError::Decode {
                        detail: format!("cannot store {value:?} in a {ty} column '{}'", col.name),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Decode a record back into its values. The record must consume exactly
    /// its bytes; a trailing byte or a truncated field is a decode error.
    pub fn decode(schema: &Schema, bytes: &[u8]) -> Result<Vec<Value>, StorageError> {
        let mut values = Vec::with_capacity(schema.arity());
        let mut at = 0usize;
        for col in schema.columns() {
            match col.ty {
                ColumnType::Int => {
                    let end = at + INT_SIZE;
                    let raw = bytes.get(at..end).ok_or_else(|| StorageError::Decode {
                        detail: format!("record ends inside INT column '{}'", col.name),
                    })?;
                    let v = i32::from_le_bytes(raw.try_into().unwrap());
                    values.push(Value::Int(v));
                    at = end;
                }
                ColumnType::Varchar(n) => {
                    let end = at + VARCHAR_LEN_SIZE;
                    let raw = bytes.get(at..end).ok_or_else(|| StorageError::Decode {
                        detail: format!(
                            "record ends inside length of VARCHAR column '{}'",
                            col.name
                        ),
                    })?;
                    let len = u16::from_le_bytes(raw.try_into().unwrap()) as usize;
                    if len > n as usize {
                        return Err(StorageError::Decode {
                            detail: format!(
                                "VARCHAR column '{}' holds {len} bytes but is declared VARCHAR({n})",
                                col.name
                            ),
                        });
                    }
                    at = end;

                    let end = at + len;
                    let raw = bytes.get(at..end).ok_or_else(|| StorageError::Decode {
                        detail: format!("record ends inside VARCHAR column '{}'", col.name),
                    })?;
                    let s = std::str::from_utf8(raw).map_err(|_| StorageError::Decode {
                        detail: format!("column '{}' is not valid UTF-8", col.name),
                    })?;
                    values.push(Value::Varchar(s.to_string()));
                    at = end;
                }
            }
        }
        if at != bytes.len() {
            return Err(StorageError::Decode {
                detail: format!("record has {} trailing byte(s)", bytes.len() - at),
            });
        }
        Ok(values)
    }
}

/// The catalog's bincode configuration and helpers (§6).
pub(crate) struct BinaryCodec;

impl BinaryCodec {
    /// Fixed-width, little-endian, no trailing bytes.
    fn options() -> impl Options {
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
    }

    /// Serialize `value` into a catalog blob.
    pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
        Self::options()
            .serialize(value)
            .map_err(|error| StorageError::Corrupt {
                detail: format!("cannot encode catalog: {error}"),
            })
    }

    /// Deserialize a catalog blob.
    pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
        Self::options()
            .deserialize(bytes)
            .map_err(|error| StorageError::Corrupt {
                detail: format!("cannot decode catalog: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_worked_schema() {
        let schema = "a:INT,b:VARCHAR(20),c:INT".parse::<Schema>().unwrap();
        assert_eq!(schema.arity(), 3);
        assert_eq!(schema.columns()[0].name, "a");
        assert_eq!(schema.columns()[0].ty, ColumnType::Int);
        assert_eq!(schema.columns()[1].ty, ColumnType::Varchar(20));
        assert_eq!(schema.columns()[2].ty, ColumnType::Int);
    }

    #[test]
    fn rejects_bad_schemas() {
        for bad in [
            "a",
            "a:INT,",
            ":INT",
            "a:INT,a:INT",
            "a:VARCHAR",
            "a:VARCHAR(0)",
            "a:VARCHAR(256)",
            "a:VARCHAR(x)",
            "a:BLOB",
        ] {
            assert!(bad.parse::<Schema>().is_err(), "'{bad}' should not parse");
        }
    }

    #[test]
    fn parses_column_types() {
        assert_eq!("INT".parse::<ColumnType>().unwrap(), ColumnType::Int);
        assert_eq!("int".parse::<ColumnType>().unwrap(), ColumnType::Int);
        assert_eq!(
            "VARCHAR(20)".parse::<ColumnType>().unwrap(),
            ColumnType::Varchar(20)
        );
        assert!("BLOB".parse::<ColumnType>().is_err());
        assert!("VARCHAR(0)".parse::<ColumnType>().is_err());
    }

    #[test]
    fn json_shape_matches_the_spec() {
        let schema = "a:INT,b:VARCHAR(20),c:INT".parse::<Schema>().unwrap();
        assert_eq!(
            schema.json(),
            serde_json::json!([["a", "INT"], ["b", "VARCHAR", 20], ["c", "INT"]])
        );
    }

    #[test]
    fn encodes_the_worked_example() {
        // Schema (a INT, b VARCHAR(5), c INT), tuple (42, 'bob', 7).
        let schema = "a:INT,b:VARCHAR(5),c:INT".parse::<Schema>().unwrap();
        let bytes = RecordCodec::encode(
            &schema,
            &[Value::Int(42), Value::Varchar("bob".into()), Value::Int(7)],
        )
        .unwrap();
        assert_eq!(
            bytes,
            vec![
                0x2A, 0x00, 0x00, 0x00, // a
                0x03, 0x00, 0x62, 0x6F, 0x62, // b
                0x07, 0x00, 0x00, 0x00, // c
            ]
        );
        assert_eq!(bytes.len(), 13);
    }

    #[test]
    fn round_trips_int_and_varchar() {
        let schema = "a:INT,b:VARCHAR(20),c:INT".parse::<Schema>().unwrap();
        let row = vec![
            Value::Int(-5),
            Value::Varchar("héllo".into()),
            Value::Int(2_000_000_000),
        ];
        let bytes = RecordCodec::encode(&schema, &row).unwrap();
        assert_eq!(RecordCodec::decode(&schema, &bytes).unwrap(), row);
    }

    #[test]
    fn rejects_overlong_and_mismatched_values() {
        let schema = "a:INT,b:VARCHAR(3)".parse::<Schema>().unwrap();
        // Every i32 encodes: the CLI already rejects out-of-range values when
        // it parses as i32, so the record codec's boundary is the extremes.
        assert!(
            RecordCodec::encode(&schema, &[Value::Int(i32::MIN), Value::Varchar("x".into())])
                .is_ok()
        );
        assert!(
            RecordCodec::encode(&schema, &[Value::Int(i32::MAX), Value::Varchar("x".into())])
                .is_ok()
        );
        // A VARCHAR longer than declared is rejected.
        assert!(
            RecordCodec::encode(&schema, &[Value::Int(1), Value::Varchar("abcd".into())]).is_err()
        );
        // A value of the wrong kind is rejected too.
        assert!(
            RecordCodec::encode(
                &schema,
                &[Value::Varchar("a".into()), Value::Varchar("ab".into())]
            )
            .is_err()
        );
        // A truncated or trailing-byte record is a decode error.
        let good =
            RecordCodec::encode(&schema, &[Value::Int(1), Value::Varchar("ab".into())]).unwrap();
        assert!(RecordCodec::decode(&schema, &good[..good.len() - 1]).is_err());
        let mut extra = good.clone();
        extra.push(0);
        assert!(RecordCodec::decode(&schema, &extra).is_err());
    }
}
