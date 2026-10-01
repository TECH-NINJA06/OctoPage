use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// A keyword or bare identifier, upper-cased.
    Word(String),
    /// A '…' string literal, unescaped.
    Str(String),
    /// A quoted identifier ("…", `…` or […]).
    Ident,
    Semicolon,
    Other,
}

/// Tokens with their byte ranges, skipping whitespace and comments.
fn lex(sql: &str) -> Vec<(Token, Range<usize>)> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                let mut text = Vec::new();
                while i < bytes.len() {
                    if bytes[i] == close {
                        // A doubled quote is an escaped quote (not inside […]).
                        if c != b'[' && bytes.get(i + 1) == Some(&close) {
                            text.push(close);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    text.push(bytes[i]);
                    i += 1;
                }
                let token = if c == b'\'' {
                    Token::Str(String::from_utf8_lossy(&text).into_owned())
                } else {
                    Token::Ident
                };
                out.push((token, start..i));
            }
            b';' => {
                i += 1;
                out.push((Token::Semicolon, start..i));
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b'$'
                        || bytes[i] >= 0x80)
                {
                    i += 1;
                }
                out.push((Token::Word(sql[start..i].to_ascii_uppercase()), start..i));
            }
            _ => {
                i += 1;
                out.push((Token::Other, start..i));
            }
        }
    }
    out
}

/// What kind of statement this is, as far as transactions and the changelog care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keyword {
    Begin,
    /// `COMMIT` or `END`.
    Commit,
    Rollback,
    RollbackTo,
    Savepoint,
    Release,
    Other,
}

pub(crate) fn leading_keyword(sql: &str) -> Keyword {
    let tokens = lex(sql);
    let word = |i: usize| match tokens.get(i) {
        Some((Token::Word(w), _)) => w.as_str(),
        _ => "",
    };
    match word(0) {
        "BEGIN" => Keyword::Begin,
        "COMMIT" | "END" => Keyword::Commit,
        "SAVEPOINT" => Keyword::Savepoint,
        "RELEASE" => Keyword::Release,
        "ROLLBACK" => {
            let next = if word(1) == "TRANSACTION" { 2 } else { 1 };
            if word(next) == "TO" {
                Keyword::RollbackTo
            } else {
                Keyword::Rollback
            }
        }
        _ => Keyword::Other,
    }
}

/// A query ending in `AS OF '<target>'` (before any semicolons): the query without it, and
/// the target. Only for queries (`SELECT`, `WITH`, `VALUES`); anything else is left alone.
pub(crate) fn split_as_of(sql: &str) -> Option<(&str, String)> {
    let mut tokens = lex(sql);
    while matches!(tokens.last(), Some((Token::Semicolon, _))) {
        tokens.pop();
    }
    if !matches!(tokens.first(), Some((Token::Word(w), _)) if ["SELECT", "WITH", "VALUES"].contains(&w.as_str()))
    {
        return None;
    }
    let n = tokens.len();
    if n < 4 {
        return None;
    }
    match (&tokens[n - 3], &tokens[n - 2], &tokens[n - 1]) {
        ((Token::Word(a), range), (Token::Word(of), _), (Token::Str(target), _))
            if a == "AS" && of == "OF" =>
        {
            Some((sql[..range.start].trim_end(), target.clone()))
        }
        _ => None,
    }
}

/// OctoPage's own statements about branches, which never reach SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BranchCommand {
    /// `CREATE BRANCH name [FROM branch]`
    Create { name: String, from: Option<String> },
    /// `DROP BRANCH name`
    Drop { name: String },
    /// `MERGE BRANCH source [INTO target]`
    Merge {
        source: String,
        into: Option<String>,
    },
}

/// A branch name as written: a word, a quoted name, or anything up to the next keyword
/// (`feature-2`, `team/alice`).
fn name(sql: &str, tokens: &[(Token, Range<usize>)]) -> Option<String> {
    let first = tokens.first()?;
    let last = tokens.last()?;
    let text = sql[first.1.start..last.1.end].trim();
    let unquoted = match (text.chars().next(), text.chars().last()) {
        (Some('"' | '\'' | '`'), Some('"' | '\'' | '`')) | (Some('['), Some(']')) => {
            &text[1..text.len() - 1]
        }
        _ => text,
    };
    (!unquoted.is_empty()).then(|| unquoted.to_string())
}

pub(crate) fn branch_command(sql: &str) -> Option<BranchCommand> {
    let mut tokens = lex(sql);
    while matches!(tokens.last(), Some((Token::Semicolon, _))) {
        tokens.pop();
    }
    let word = |i: usize| match tokens.get(i) {
        Some((Token::Word(w), _)) => w.as_str(),
        _ => "",
    };
    if word(1) != "BRANCH" {
        return None;
    }
    let verb = word(0);
    let rest = &tokens[2..];
    // Split the rest at a keyword: `name KEYWORD other`.
    let split = |keyword: &str| match rest
        .iter()
        .position(|(t, _)| matches!(t, Token::Word(w) if w == keyword))
    {
        Some(i) => (name(sql, &rest[..i]), name(sql, &rest[i + 1..]).map(Some)),
        None => (name(sql, rest), Some(None)),
    };
    match verb {
        "CREATE" => {
            let (name, from) = split("FROM");
            Some(BranchCommand::Create {
                name: name?,
                from: from?,
            })
        }
        "DROP" => Some(BranchCommand::Drop {
            name: name(sql, rest)?,
        }),
        "MERGE" => {
            let (source, into) = split("INTO");
            Some(BranchCommand::Merge {
                source: source?,
                into: into?,
            })
        }
        _ => None,
    }
}

/// The statements of a batch, each with its terminating semicolon. A semicolon ends a statement
/// only where SQLite agrees (`sqlite3_complete`), so trigger bodies stay whole.
pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (token, range) in lex(sql) {
        if token == Token::Semicolon && octopage_sqlite::is_complete(&sql[start..range.end]) {
            if lex(&sql[start..range.start])
                .iter()
                .any(|(t, _)| *t != Token::Semicolon)
            {
                out.push(&sql[start..range.end]);
            }
            start = range.end;
        }
    }
    if lex(&sql[start..])
        .iter()
        .any(|(t, _)| *t != Token::Semicolon)
    {
        out.push(&sql[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords() {
        assert_eq!(leading_keyword("  begin immediate"), Keyword::Begin);
        assert_eq!(leading_keyword("-- note\nEND"), Keyword::Commit);
        assert_eq!(
            leading_keyword("ROLLBACK TRANSACTION TO sp"),
            Keyword::RollbackTo
        );
        assert_eq!(leading_keyword("rollback to sp"), Keyword::RollbackTo);
        assert_eq!(leading_keyword("ROLLBACK"), Keyword::Rollback);
        assert_eq!(leading_keyword("/* x */ release sp"), Keyword::Release);
        assert_eq!(leading_keyword("SELECT 1"), Keyword::Other);
    }

    #[test]
    fn as_of() {
        assert_eq!(
            split_as_of("SELECT * FROM t AS OF '2026-09-01';"),
            Some(("SELECT * FROM t", "2026-09-01".to_string()))
        );
        assert_eq!(
            split_as_of("select a as of_x from t as of 'a1b2c3d'"),
            Some(("select a as of_x from t", "a1b2c3d".to_string()))
        );
        // A string that merely looks like it, or no AS OF at all.
        assert_eq!(split_as_of("SELECT 'AS OF ''x'''"), None);
        assert_eq!(split_as_of("SELECT x AS \"OF\" FROM t"), None);
        assert_eq!(split_as_of("INSERT INTO t SELECT 1 AS OF 'x'"), None);
        assert_eq!(split_as_of("SELECT 1 -- AS OF 'x'"), None);
    }

    #[test]
    fn branch_commands() {
        assert_eq!(
            branch_command("CREATE BRANCH feature-2 FROM main;"),
            Some(BranchCommand::Create {
                name: "feature-2".into(),
                from: Some("main".into())
            })
        );
        assert_eq!(
            branch_command("create branch \"team/alice\""),
            Some(BranchCommand::Create {
                name: "team/alice".into(),
                from: None
            })
        );
        assert_eq!(
            branch_command("MERGE BRANCH b INTO main"),
            Some(BranchCommand::Merge {
                source: "b".into(),
                into: Some("main".into())
            })
        );
        assert_eq!(
            branch_command("DROP BRANCH old"),
            Some(BranchCommand::Drop { name: "old".into() })
        );
        assert_eq!(branch_command("CREATE TABLE branch(x)"), None);
        assert_eq!(branch_command("CREATE BRANCH"), None);
        assert_eq!(branch_command("MERGE BRANCH b INTO"), None);
    }

    #[test]
    fn batches() {
        let sql = "CREATE TABLE t(x); INSERT INTO t VALUES(';');
            CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; SELECT 2; END;
            -- a comment
            SELECT 1";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 4, "{parts:?}");
        assert!(parts[2].contains("SELECT 2; END;"));
        assert_eq!(
            parts[3]
                .trim_start()
                .trim_start_matches("-- a comment")
                .trim(),
            "SELECT 1"
        );
        assert!(split_statements(" ;; -- nothing\n").is_empty());
    }
}
