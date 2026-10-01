use std::fmt;

use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A SQL value, with SQLite's five storage classes.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Value {
    /// `NULL`.
    #[default]
    Null,
    /// A 64-bit signed integer.
    Integer(i64),
    /// A 64-bit floating-point number.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Bytes.
    Blob(Vec<u8>),
}

impl Value {
    /// Whether this is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The integer, if this is one.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// A real, or an integer as a real.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Real(f) => Some(*f),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// The text, if this is text.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// The bytes, if this is a blob.
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

impl From<ValueRef<'_>> for Value {
    fn from(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Integer(i),
            ValueRef::Real(f) => Value::Real(f),
            ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        }
    }
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Value::Null => ValueRef::Null,
            Value::Integer(i) => ValueRef::Integer(*i),
            Value::Real(f) => ValueRef::Real(*f),
            Value::Text(s) => ValueRef::Text(s.as_bytes()),
            Value::Blob(b) => ValueRef::Blob(b),
        }))
    }
}

macro_rules! from_integer {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::Integer(v.into())
            }
        }
    )*};
}
from_integer!(i8, i16, i32, i64, u8, u16, u32);

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Integer(v.into())
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Value::Real(v.into())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Blob(v)
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Value::Blob(v.to_vec())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// Parameters for a statement: `params![1, "two", None::<i64>]`.
#[macro_export]
macro_rules! params {
    () => {
        &[] as &[$crate::Value]
    };
    ($($value:expr),+ $(,)?) => {
        &[$($crate::Value::from($value)),+] as &[$crate::Value]
    };
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// As SQLite's shell shows values: `NULL`, `2.0` for a whole real, `x'00ff'` for a blob.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(r) if r.is_finite() && r.fract() == 0.0 && r.abs() < 1e15 => {
                write!(f, "{r:.1}")
            }
            Value::Real(r) => write!(f, "{r}"),
            Value::Text(s) => f.write_str(s),
            Value::Blob(b) => write!(f, "x'{}'", hex(b)),
        }
    }
}

/// In the changelog: JSON null, number or string, and `{"blob": "<hex>"}` for a blob.
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Integer(i) => serializer.serialize_i64(*i),
            Value::Real(r) => serializer.serialize_f64(*r),
            Value::Text(s) => serializer.serialize_str(s),
            Value::Blob(b) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("blob", &hex(b))?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        Ok(match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Integer(b.into()),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Integer(i),
                None => Value::Real(n.as_f64().ok_or_else(|| D::Error::custom("bad number"))?),
            },
            serde_json::Value::String(s) => Value::Text(s),
            serde_json::Value::Object(map) => {
                let blob = map
                    .get("blob")
                    .and_then(|v| v.as_str())
                    .and_then(unhex)
                    .ok_or_else(|| D::Error::custom("expected {\"blob\": \"<hex>\"}"))?;
                Value::Blob(blob)
            }
            serde_json::Value::Array(_) => return Err(D::Error::custom("unexpected array")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changelog_form_round_trips() {
        let values = vec![
            Value::Null,
            Value::Integer(-7),
            Value::Real(1.0),
            Value::Real(0.25),
            Value::Text("it's".into()),
            Value::Blob(vec![0, 255, 16]),
        ];
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, r#"[null,-7,1.0,0.25,"it's",{"blob":"00ff10"}]"#);
        assert_eq!(serde_json::from_str::<Vec<Value>>(&json).unwrap(), values);
    }

    #[test]
    fn display_like_sqlite() {
        assert_eq!(Value::Real(2.0).to_string(), "2.0");
        assert_eq!(Value::Real(2.5).to_string(), "2.5");
        assert_eq!(Value::Blob(vec![1, 171]).to_string(), "x'01ab'");
        assert_eq!(Value::from(None::<i64>).to_string(), "NULL");
        assert_eq!(params![1, "a", true].len(), 3);
    }
}
