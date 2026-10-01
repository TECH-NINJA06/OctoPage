use octopage_git::ObjectId;
use serde::{Deserialize, Serialize};

use crate::Value;

pub(crate) const FORMAT: &str = "octopage-changelog-1";

/// One recorded statement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    /// The statement's SQL.
    pub sql: String,
    /// Its parameters, in order (`?1`, `?2`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Value>,
}

impl Statement {
    /// A statement with its parameters.
    pub fn new(sql: impl Into<String>, params: &[Value]) -> Self {
        Statement {
            sql: sql.into(),
            params: params.to_vec(),
        }
    }
}

/// What one commit's transaction ran.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changelog {
    /// The transaction's time, in milliseconds since the Unix epoch: what `datetime('now')` and
    /// `CURRENT_TIMESTAMP` saw.
    pub now: i64,
    /// The seed `random()` and `randomblob()` drew from.
    pub seed: u64,
    /// The statements that changed data, in order, with the savepoints around them.
    pub statements: Vec<Statement>,
    /// For a commit made by a merge: the commit on the other branch it replays.
    pub origin: Option<ObjectId>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    format: String,
    #[serde(default)]
    now: i64,
    #[serde(default)]
    seed: String,
    statements: Vec<Statement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
}

pub(crate) fn encode(changelog: &Changelog) -> Vec<u8> {
    let stored = Stored {
        format: FORMAT.into(),
        now: changelog.now,
        // As hex: a JSON number above 2^53 loses digits in many readers.
        seed: format!("{:016x}", changelog.seed),
        statements: changelog.statements.clone(),
        origin: changelog.origin.map(|id| id.to_hex()),
    };
    let mut json = serde_json::to_vec_pretty(&stored).expect("values serialize");
    json.push(b'\n');
    json
}

/// A changelog blob; `None` if it is not one of ours. An empty blob (a commit that recorded
/// nothing) is an empty changelog.
pub fn decode(bytes: &[u8]) -> Option<Changelog> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Some(Changelog::default());
    }
    let stored: Stored = serde_json::from_slice(bytes).ok()?;
    if stored.format != FORMAT {
        return None;
    }
    Some(Changelog {
        now: stored.now,
        seed: u64::from_str_radix(&stored.seed, 16).unwrap_or_default(),
        statements: stored.statements,
        origin: stored.origin.and_then(|hex| ObjectId::from_hex(&hex).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let changelog = Changelog {
            now: 1_790_583_065_123,
            seed: u64::MAX - 1,
            statements: vec![
                Statement::new(
                    "INSERT INTO t VALUES(?1, ?2)",
                    &[Value::Integer(1), Value::Blob(vec![7])],
                ),
                Statement::new("DELETE FROM t", &[]),
            ],
            origin: Some(ObjectId::from_array([7; 20])),
        };
        let bytes = encode(&changelog);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\"sql\": \"DELETE FROM t\""), "{text}");
        assert!(text.contains("\"seed\": \"fffffffffffffffe\""), "{text}");
        assert_eq!(decode(&bytes), Some(changelog));
        assert_eq!(decode(b""), Some(Changelog::default()));
        assert_eq!(decode(b"{}"), None);
    }
}
