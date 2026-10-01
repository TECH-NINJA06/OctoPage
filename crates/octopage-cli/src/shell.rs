use std::process::ExitCode;
use std::time::Instant;

use octopage::{Connection, Database, Rows, Transport, Value, params};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::runtime::Runtime;

const HELP: &str = "\
Statements end with a semicolon; each one outside BEGIN … COMMIT is a commit of its own.
Read the past with  SELECT … AS OF '<commit id or UTC time>';
Branches:  CREATE BRANCH name [FROM branch];  MERGE BRANCH name [INTO branch];  DROP BRANCH name;

.tables          list tables and views
.schema [NAME]   show CREATE statements
.log [N]         the last N commits (default 10) and the statements each ran
.branches        the repository's branches (* marks this one)
.head            read the head ref now
.stats           this session's counters
.timer on|off    show how long each statement took
.help            this text
.quit            leave (Ctrl-D works too)";

/// The widest a column gets before its values are cut short.
const MAX_WIDTH: usize = 60;

pub struct Shell<'r, T: Transport + 'static> {
    runtime: &'r Runtime,
    db: Database<T>,
    conn: Connection<T>,
    timer: bool,
}

/// `YYYY-MM-DD hh:mm:ss UTC` from Unix seconds.
fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

fn cell(value: &Value) -> String {
    let text = value.to_string().replace('\n', "\\n");
    if text.chars().count() > MAX_WIDTH {
        text.chars().take(MAX_WIDTH - 1).chain(['…']).collect()
    } else {
        text
    }
}

/// Rows as an aligned table, with a row count.
pub fn table(rows: &Rows) -> String {
    let cells: Vec<Vec<String>> = rows
        .rows
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    let mut widths: Vec<usize> = rows.columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (width, value) in widths.iter_mut().zip(row) {
            *width = (*width).max(value.chars().count());
        }
    }
    let line = |values: &[String]| {
        values
            .iter()
            .zip(&widths)
            .map(|(v, w)| format!("{v:<w$}"))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(&rows.columns)];
    out.push(
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    out.extend(cells.iter().map(|row| line(row)));
    out.push(match rows.len() {
        1 => "(1 row)".to_string(),
        n => format!("({n} rows)"),
    });
    out.join("\n")
}

impl<'r, T: Transport + 'static> Shell<'r, T> {
    pub fn new(runtime: &'r Runtime, db: Database<T>) -> octopage::Result<Self> {
        let conn = db.connect()?;
        Ok(Shell {
            runtime,
            db,
            conn,
            timer: false,
        })
    }

    /// Run SQL and dot commands from a script, stopping at the first error.
    pub fn script(&mut self, text: &str) -> ExitCode {
        let text = text.trim_start_matches('\u{feff}'); // PowerShell pipes add a byte-order mark
        let mut sql = String::new();
        for line in text.lines() {
            if sql.trim().is_empty() && line.trim_start().starts_with('.') {
                match self.dot(line.trim()) {
                    Ok(true) => continue,
                    Ok(false) => return ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            sql.push_str(line);
            sql.push('\n');
            if octopage::is_complete(&sql) {
                if !self.sql(&sql) {
                    return ExitCode::FAILURE;
                }
                sql.clear();
            }
        }
        if !sql.trim().is_empty() && !self.sql(&sql) {
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    }

    /// Read statements and dot commands at a prompt until `.quit` or end of input.
    pub fn interactive(&mut self) -> rustyline::Result<()> {
        let mut editor = DefaultEditor::new()?;
        println!(
            "octopage {} on {} (head {}). Type .help for help.",
            env!("CARGO_PKG_VERSION"),
            self.db.config().store.branch,
            &self.db.head().to_hex()[..12]
        );
        let mut sql = String::new();
        loop {
            let prompt = match (sql.is_empty(), self.conn.in_transaction()) {
                (false, _) => "     ...> ",
                (true, true) => "octopage*> ",
                (true, false) => "octopage> ",
            };
            let line = match editor.readline(prompt) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    sql.clear(); // Ctrl-C drops the statement being typed
                    continue;
                }
                Err(ReadlineError::Eof) => return Ok(()),
                Err(e) => return Err(e),
            };
            if sql.is_empty() && line.trim_start().starts_with('.') {
                let _ = editor.add_history_entry(line.as_str());
                match self.dot(line.trim()) {
                    Ok(true) => continue,
                    Ok(false) => return Ok(()),
                    Err(e) => {
                        eprintln!("error: {e}");
                        continue;
                    }
                }
            }
            if sql.is_empty() && line.trim().is_empty() {
                continue;
            }
            sql.push_str(&line);
            sql.push('\n');
            if octopage::is_complete(&sql) {
                let _ = editor.add_history_entry(sql.trim_end());
                self.sql(&sql);
                sql.clear();
            }
        }
    }

    /// Run the statements in `sql`, printing results. False after an error.
    fn sql(&mut self, sql: &str) -> bool {
        for statement in octopage::statements(sql) {
            let before = self.conn.last_commit();
            let started = Instant::now();
            match self.conn.run_statement(statement, params![]) {
                Ok(outcome) => {
                    if !outcome.rows.columns.is_empty() {
                        println!("{}", table(&outcome.rows));
                    } else if outcome.changed > 0 {
                        let s = if outcome.changed == 1 { "" } else { "s" };
                        println!("{} row{s} changed", outcome.changed);
                    }
                    if let Some(commit) = self.conn.last_commit()
                        && Some(commit) != before
                        && commit.published
                    {
                        println!("committed {}", &commit.head.to_hex()[..12]);
                    }
                    if self.timer {
                        println!("({:.1?})", started.elapsed());
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return false;
                }
            }
        }
        true
    }

    /// A dot command. `Ok(false)` means quit.
    fn dot(&mut self, line: &str) -> Result<bool, String> {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        let argument = words.next();
        let e = |e: octopage::Error| e.to_string();
        match command {
            ".quit" | ".exit" => return Ok(false),
            ".help" => println!("{HELP}"),
            ".tables" => {
                let rows = self
                    .conn
                    .query(
                        "SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') \
                         AND name NOT LIKE 'sqlite_%' ORDER BY name",
                        params![],
                    )
                    .map_err(e)?;
                for row in &rows.rows {
                    println!("{}", row[0]);
                }
            }
            ".schema" => {
                let rows = self
                    .conn
                    .query(
                        "SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL \
                         AND (?1 IS NULL OR name = ?1 OR tbl_name = ?1) ORDER BY rowid",
                        params![argument],
                    )
                    .map_err(e)?;
                for row in &rows.rows {
                    println!("{};", row[0]);
                }
            }
            ".log" => {
                let n = argument
                    .map_or(Ok(10), str::parse)
                    .map_err(|_| "usage: .log [N]")?;
                self.runtime.block_on(self.db.refresh()).map_err(e)?;
                for entry in self.runtime.block_on(self.db.log(n)).map_err(e)? {
                    println!("{}  {}", &entry.commit.to_hex()[..12], utc(entry.time));
                    for statement in &entry.changelog.statements {
                        let sql = statement.sql.replace('\n', " ");
                        if statement.params.is_empty() {
                            println!("    {sql}");
                        } else {
                            let params: Vec<String> = statement.params.iter().map(cell).collect();
                            println!("    {sql}    -- {}", params.join(", "));
                        }
                    }
                }
            }
            ".branches" => {
                for (name, head) in self.runtime.block_on(self.db.branches()).map_err(e)? {
                    let mark = if name == self.db.branch_name() {
                        '*'
                    } else {
                        ' '
                    };
                    println!("{mark} {name}  {}", &head.to_hex()[..12]);
                }
            }
            ".head" => {
                let head = self.runtime.block_on(self.db.refresh()).map_err(e)?;
                println!("{head}");
            }
            ".stats" => {
                let s = self.db.stats();
                let get = octopage::Stats::get;
                println!(
                    "transactions {}, commits published {}, rebased {}, refused {}, pages read {}, pages fetched ahead {}",
                    get(&s.transactions),
                    get(&s.published),
                    get(&s.rebases),
                    get(&s.conflicts),
                    get(&s.pages_read),
                    get(&s.prefetched_pages)
                );
            }
            ".timer" => match argument {
                Some("on") => self.timer = true,
                Some("off") => self.timer = false,
                _ => return Err("usage: .timer on|off".into()),
            },
            other => return Err(format!("unknown command {other}; try .help")),
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables() {
        let rows = Rows {
            columns: vec!["id".into(), "name".into()],
            rows: vec![
                vec![Value::Integer(1), Value::Text("ada".into())],
                vec![Value::Integer(20), Value::Null],
            ],
        };
        assert_eq!(
            table(&rows),
            "id | name\n---+-----\n1  | ada\n20 | NULL\n(2 rows)"
        );
        assert_eq!(utc(1_790_568_493), "2026-09-28 04:08:13 UTC");
    }
}
