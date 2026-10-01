use std::collections::HashMap;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use octopage::Value;
use serde::{Deserialize, Serialize};

/// One record of a test file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Case {
    /// Statements (one or several, run as `sqlite3_exec` does) that must succeed, or fail.
    Statement { line: usize, ok: bool, sql: String },
    /// A query and what it returned for SQLite. `sort`: `none`, `rows` or `values`.
    Query {
        line: usize,
        types: String,
        sort: String,
        label: Option<String>,
        sql: String,
        expected: Option<Vec<String>>,
    },
    /// From here on, compare results with more values than this by their hash.
    HashThreshold { line: usize, values: usize },
}

/// A test file's cases.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Suite {
    pub file: String,
    pub cases: Vec<Case>,
}

impl Suite {
    pub fn statements(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c, Case::Statement { .. }))
            .count()
    }

    pub fn queries(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c, Case::Query { .. }))
            .count()
    }
}

/// The engine `skipif` and `onlyif` lines are matched against.
const ENGINE: &str = "sqlite";

/// The records of `text` that apply to SQLite, up to a `halt`.
pub fn parse(file: &str, text: &str) -> Suite {
    let lines: Vec<&str> = text.lines().collect();
    let mut cases = Vec::new();
    let mut i = 0;
    let mut skip = false;
    while i < lines.len() {
        let words: Vec<&str> = lines[i].split_whitespace().collect();
        let line = i + 1;
        i += 1;
        let Some(&first) = words.first() else {
            skip = false; // a blank line ends a record's conditions
            continue;
        };
        let mut body = |stop_at_dashes: bool| {
            let mut sql = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                if stop_at_dashes && lines[i].trim_end() == "----" {
                    break;
                }
                sql.push(lines[i]);
                i += 1;
            }
            sql.join("\n")
        };
        let case = match first {
            _ if first.starts_with('#') => continue,
            "skipif" => {
                skip |= words.get(1) == Some(&ENGINE);
                continue;
            }
            "onlyif" => {
                skip |= words.get(1) != Some(&ENGINE);
                continue;
            }
            "halt" if !skip => break,
            "halt" => {
                skip = false;
                continue;
            }
            "hash-threshold" => Case::HashThreshold {
                line,
                values: words[1].parse().expect("a number"),
            },
            "statement" => Case::Statement {
                line,
                ok: words.get(1) == Some(&"ok"),
                sql: body(false),
            },
            "query" => {
                let sql = body(true);
                let expected = (i < lines.len() && lines[i].trim_end() == "----").then(|| {
                    i += 1;
                    let mut values = Vec::new();
                    while i < lines.len() && !lines[i].trim().is_empty() {
                        values.push(lines[i].trim_end().to_string());
                        i += 1;
                    }
                    values
                });
                Case::Query {
                    line,
                    types: words[1].to_string(),
                    sort: match words.get(2) {
                        Some(&"rowsort") => "rows",
                        Some(&"valuesort") => "values",
                        _ => "none",
                    }
                    .to_string(),
                    label: words.get(3).map(|s| s.to_string()),
                    sql,
                    expected,
                }
            }
            other => panic!("{file}:{line}: unknown record {other:?}"),
        };
        if !skip {
            cases.push(case);
        }
        skip = false;
    }
    Suite {
        file: file.to_string(),
        cases,
    }
}

/// Where the logic tests are: `crates/octopage/tests/slt`.
pub fn slt_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("octopage")
        .join("tests")
        .join("slt")
}

/// The gate's suite: OctoPage's own features, SQLite's evidence files (triggers, views,
/// REPLACE, UPDATE, aggregates, IN), and select1 (a thousand queries).
pub fn suites() -> Vec<Suite> {
    let dir = slt_dir();
    let mut files = vec![dir.join("octopage").join("features.test")];
    let mut evidence: Vec<PathBuf> = std::fs::read_dir(dir.join("sqlite").join("evidence"))
        .expect("the evidence files")
        .map(|e| e.expect("a file").path())
        .filter(|p| p.extension().is_some_and(|e| e == "test"))
        .collect();
    evidence.sort();
    files.extend(evidence);
    files.push(dir.join("sqlite").join("select1.test"));
    files
        .iter()
        .map(|path| {
            let text =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let name = path
                .strip_prefix(&dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            parse(&name, &text)
        })
        .collect()
}

/// The suites as JSON, for the runners in other languages.
pub fn suites_json() -> String {
    serde_json::to_string(&suites()).expect("serializes")
}

/// What a runner reports: counts, and one line per failure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub statements: usize,
    pub queries: usize,
    pub failures: Vec<String>,
}

/// A report printed as JSON by a runner in another language.
pub fn report_from_json(text: &str) -> Report {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("the runner's report ({e}): {text}"))
}

impl Report {
    pub fn add(&mut self, other: Report) {
        self.statements += other.statements;
        self.queries += other.queries;
        self.failures.extend(other.failures);
    }
}

pub fn md5_of(values: &[String]) -> String {
    let mut hash = Md5::new();
    for value in values {
        hash.update(value.as_bytes());
        hash.update(b"\n");
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Compares query results with what a file recorded, as the reference runner does.
pub struct Checker {
    convert: rusqlite::Connection,
    hash_threshold: usize,
    labels: HashMap<String, String>,
}

impl Default for Checker {
    fn default() -> Self {
        Checker {
            convert: rusqlite::Connection::open_in_memory().expect("SQLite"),
            hash_threshold: 0,
            labels: HashMap::new(),
        }
    }
}

impl Checker {
    pub fn set_hash_threshold(&mut self, values: usize) {
        self.hash_threshold = values;
    }

    fn text(&self, sql: &str, value: &Value) -> String {
        self.convert
            .query_row(sql, [value], |r| r.get::<_, String>(0))
            .expect("a conversion")
    }

    /// A value as the reference runner prints it for column type `ty`.
    pub fn format(&self, value: &Value, ty: char) -> String {
        let text = match (ty, value) {
            (_, Value::Null) => return "NULL".into(),
            ('I', Value::Integer(i)) => return i.to_string(),
            ('I', v) => {
                return self
                    .convert
                    .query_row("SELECT CAST(?1 AS INTEGER)", [v], |r| r.get::<_, i64>(0))
                    .expect("a conversion")
                    .to_string();
            }
            ('R', v) => return self.text("SELECT printf('%.3f', CAST(?1 AS REAL))", v),
            (_, Value::Text(s)) => s.clone(),
            (_, Value::Integer(i)) => i.to_string(),
            (_, v) => self.text("SELECT CAST(?1 AS TEXT)", v),
        };
        if text.is_empty() {
            return "(empty)".into();
        }
        text.bytes()
            .map(|b| {
                if (b' '..=b'~').contains(&b) {
                    b as char
                } else {
                    '@'
                }
            })
            .collect()
    }

    /// Check a query's `rows` against the case.
    pub fn check(
        &mut self,
        types: &str,
        sort: &str,
        label: Option<&str>,
        expected: Option<&[String]>,
        rows: &[Vec<Value>],
    ) -> Result<(), String> {
        let types: Vec<char> = types.chars().collect();
        if let Some(row) = rows.first()
            && row.len() != types.len()
        {
            return Err(format!(
                "{} columns, the test declares {}",
                row.len(),
                types.len()
            ));
        }
        let mut table: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&types)
                    .map(|(v, t)| self.format(v, *t))
                    .collect()
            })
            .collect();
        if sort == "rows" {
            table.sort();
        }
        let mut values: Vec<String> = table.into_iter().flatten().collect();
        if sort == "values" {
            values.sort();
        }
        let hash = md5_of(&values);
        if let Some(label) = label
            && let Some(previous) = self.labels.insert(label.to_string(), hash.clone())
            && previous != hash
        {
            return Err(format!("results differ from earlier ones labelled {label}"));
        }
        let Some(expected) = expected else {
            return Ok(());
        };
        // Results above the threshold are recorded as a hash (SQLite's files were made with a
        // threshold of 8 whether or not they say so, so a recorded hash is compared as one).
        let hashed = expected.len() == 1 && expected[0].contains(" values hashing to ");
        let got = if hashed || (self.hash_threshold > 0 && values.len() > self.hash_threshold) {
            vec![format!("{} values hashing to {hash}", values.len())]
        } else {
            values
        };
        if got != expected {
            let show = |v: &[String]| {
                let mut s = v.iter().take(12).cloned().collect::<Vec<_>>().join(" ");
                if v.len() > 12 {
                    s.push_str(" …");
                }
                s
            };
            return Err(format!(
                "expected [{}], got [{}]",
                show(expected),
                show(&got)
            ));
        }
        Ok(())
    }
}

/// Where a runner's SQL goes: one database per suite.
pub trait Target {
    /// Run a statement record (one or several statements, as `sqlite3_exec`).
    fn statements(&mut self, sql: &str) -> Result<(), String>;
    /// Run a query: its rows.
    fn query(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, String>;
}

/// Run `suite` on `target`.
pub fn run(suite: &Suite, target: &mut impl Target) -> Report {
    let mut checker = Checker::default();
    let mut report = Report::default();
    for case in &suite.cases {
        let outcome = match case {
            Case::HashThreshold { values, .. } => {
                checker.set_hash_threshold(*values);
                continue;
            }
            Case::Statement { line, ok, sql } => {
                report.statements += 1;
                match (target.statements(sql), ok) {
                    (Ok(()), true) | (Err(_), false) => Ok(()),
                    (Ok(()), false) => Err((
                        line,
                        sql,
                        "the statement succeeded but should fail".to_string(),
                    )),
                    (Err(e), true) => Err((line, sql, format!("the statement failed: {e}"))),
                }
            }
            Case::Query {
                line,
                types,
                sort,
                label,
                sql,
                expected,
            } => {
                report.queries += 1;
                match target.query(sql) {
                    Ok(rows) => checker
                        .check(types, sort, label.as_deref(), expected.as_deref(), &rows)
                        .map_err(|e| (line, sql, e)),
                    // The reference runner reports no rows for a query that fails while it
                    // steps through it (sum()'s integer overflow, say).
                    Err(_) if expected.as_ref().is_some_and(Vec::is_empty) => Ok(()),
                    Err(e) => Err((line, sql, format!("the query failed: {e}"))),
                }
            }
        };
        if let Err((line, sql, message)) = outcome {
            report.failures.push(format!(
                "{}:{line}: {message}\n    {}",
                suite.file,
                sql.replace('\n', "\n    ")
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suite_parses() {
        let suites = suites();
        assert_eq!(suites.len(), 14);
        let queries: usize = suites.iter().map(Suite::queries).sum();
        let statements: usize = suites.iter().map(Suite::statements).sum();
        assert!(queries > 1300 && statements > 200, "{queries} {statements}");
        let json = suites_json();
        let back: Vec<Suite> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0].cases, suites[0].cases);
    }
}
