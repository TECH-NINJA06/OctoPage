use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// A failure of the metadata store.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("the metadata store's worker stopped: {0}")]
    Worker(String),
}

pub type Result<T> = std::result::Result<T, MetaError>;

/// Someone who signed in with GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub github_id: i64,
    pub login: String,
}

/// An installation of the GitHub App on an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installation {
    /// GitHub's installation id.
    pub id: i64,
    /// The account it is installed on.
    pub account: String,
    pub suspended: bool,
}

/// A repository an installation reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// GitHub's repository id.
    pub id: i64,
    pub installation: i64,
    /// `OWNER/NAME`.
    pub full_name: String,
    pub private: bool,
}

/// How a database's key is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// Not encrypted.
    None,
    /// Wrapped by the service's key service (KMS): the service opens it by itself.
    Kms,
    /// Wrapped by the user's passphrase: open until the service restarts or evicts it, after
    /// the user unlocks it.
    Passphrase,
}

impl KeyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyMode::None => "none",
            KeyMode::Kms => "kms",
            KeyMode::Passphrase => "passphrase",
        }
    }

    pub fn parse(s: &str) -> Option<KeyMode> {
        match s {
            "none" => Some(KeyMode::None),
            "kms" => Some(KeyMode::Kms),
            "passphrase" => Some(KeyMode::Passphrase),
            _ => None,
        }
    }
}

/// A database the service serves: a branch of a repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseRecord {
    /// `db_` and 16 hex digits.
    pub id: String,
    /// `OWNER/NAME` (it changes when the database moves to a new generation).
    pub repository: String,
    pub installation: i64,
    /// The ref, `refs/heads/…`.
    pub branch: String,
    pub keys: KeyMode,
    pub created: i64,
}

/// What a key is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
    /// An API key: `opk_…`, long-lived, for applications.
    Api,
    /// A sign-in session: `ops_…`, expires, for the dashboard.
    Session,
}

/// A key, without its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    /// `key_` and 12 hex digits.
    pub id: String,
    pub user: i64,
    pub name: String,
    pub kind: KeyKind,
    /// The first characters of the key, to recognise it by.
    pub prefix: String,
    pub created: i64,
    pub expires: Option<i64>,
    pub last_used: Option<i64>,
}

/// Usage of one installation on one day (days since the Unix epoch).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub day: i64,
    pub requests: i64,
    pub github_requests: i64,
    pub commits: i64,
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the system's random source");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(key: &str) -> String {
    Sha256::digest(key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS users(
    id INTEGER PRIMARY KEY,
    github_id INTEGER NOT NULL UNIQUE,
    login TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS installations(
    id INTEGER PRIMARY KEY,
    account TEXT NOT NULL,
    suspended INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS memberships(
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    PRIMARY KEY(user_id, installation_id)
);
CREATE TABLE IF NOT EXISTS repositories(
    id INTEGER PRIMARY KEY,
    installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    full_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    private INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS databases(
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL COLLATE NOCASE,
    installation_id INTEGER NOT NULL REFERENCES installations(id) ON DELETE CASCADE,
    branch TEXT NOT NULL,
    keys TEXT NOT NULL,
    created INTEGER NOT NULL,
    UNIQUE(repository, branch)
);
CREATE TABLE IF NOT EXISTS keys(
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    prefix TEXT NOT NULL,
    created INTEGER NOT NULL,
    expires INTEGER,
    last_used INTEGER
);
CREATE TABLE IF NOT EXISTS usage(
    installation_id INTEGER NOT NULL,
    day INTEGER NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    github_requests INTEGER NOT NULL DEFAULT 0,
    commits INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(installation_id, day)
);
"#;

/// The metadata store. Cheap to clone.
#[derive(Clone)]
pub struct Meta {
    conn: Arc<Mutex<Connection>>,
}

fn database_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DatabaseRecord> {
    let keys: String = row.get(4)?;
    Ok(DatabaseRecord {
        id: row.get(0)?,
        repository: row.get(1)?,
        installation: row.get(2)?,
        branch: row.get(3)?,
        keys: KeyMode::parse(&keys).unwrap_or(KeyMode::None),
        created: row.get(5)?,
    })
}

const DATABASE_COLUMNS: &str = "id, repository, installation_id, branch, keys, created";

fn key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRecord> {
    let kind: String = row.get(3)?;
    Ok(KeyRecord {
        id: row.get(0)?,
        user: row.get(1)?,
        name: row.get(2)?,
        kind: if kind == "session" {
            KeyKind::Session
        } else {
            KeyKind::Api
        },
        prefix: row.get(4)?,
        created: row.get(5)?,
        expires: row.get(6)?,
        last_used: row.get(7)?,
    })
}

const KEY_COLUMNS: &str = "id, user_id, name, kind, prefix, created, expires, last_used";

impl Meta {
    /// The store in a file, created if needed.
    pub fn open(path: &Path) -> Result<Meta> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Meta::with(conn)
    }

    /// A store in memory (tests).
    pub fn memory() -> Result<Meta> {
        Meta::with(Connection::open_in_memory()?)
    }

    fn with(conn: Connection) -> Result<Meta> {
        conn.execute_batch(SCHEMA)?;
        Ok(Meta {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn run<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> rusqlite::Result<R> + Send + 'static,
    ) -> Result<R> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            work(&mut conn).map_err(MetaError::from)
        })
        .await
        .map_err(|e| MetaError::Worker(e.to_string()))?
    }

    /// Whether the store answers (for the readiness check).
    pub async fn ping(&self) -> Result<()> {
        self.run(|c| c.query_row("SELECT 1", [], |_| Ok(()))).await
    }

    // ------------------------------------------------------------------ users

    /// Record a GitHub user (or their new login); returns them.
    pub async fn upsert_user(&self, github_id: i64, login: &str) -> Result<User> {
        let login = login.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT INTO users(github_id, login, created) VALUES(?1, ?2, ?3)
                 ON CONFLICT(github_id) DO UPDATE SET login = excluded.login",
                params![github_id, login, now()],
            )?;
            c.query_row(
                "SELECT id, github_id, login FROM users WHERE github_id = ?1",
                [github_id],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        github_id: r.get(1)?,
                        login: r.get(2)?,
                    })
                },
            )
        })
        .await
    }

    pub async fn user(&self, id: i64) -> Result<Option<User>> {
        self.run(move |c| {
            c.query_row(
                "SELECT id, github_id, login FROM users WHERE id = ?1",
                [id],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        github_id: r.get(1)?,
                        login: r.get(2)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    pub async fn user_by_github_id(&self, github_id: i64) -> Result<Option<User>> {
        self.run(move |c| {
            c.query_row(
                "SELECT id, github_id, login FROM users WHERE github_id = ?1",
                [github_id],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        github_id: r.get(1)?,
                        login: r.get(2)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Forget a user, with their keys, sessions and memberships (installations and databases
    /// belong to the accounts that installed the App, and stay).
    pub async fn delete_user(&self, id: i64) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM users WHERE id = ?1", [id])
                .map(|_| ())
        })
        .await
    }

    // ------------------------------------------------------------------ installations

    pub async fn upsert_installation(&self, id: i64, account: &str) -> Result<()> {
        let account = account.to_string();
        self.run(move |c| {
            c.execute(
                "INSERT INTO installations(id, account, created) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET account = excluded.account",
                params![id, account, now()],
            )
            .map(|_| ())
        })
        .await
    }

    /// Forget an installation, with its repositories, databases and memberships.
    pub async fn delete_installation(&self, id: i64) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM installations WHERE id = ?1", [id])
                .map(|_| ())
        })
        .await
    }

    pub async fn suspend_installation(&self, id: i64, suspended: bool) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE installations SET suspended = ?2 WHERE id = ?1",
                params![id, suspended],
            )
            .map(|_| ())
        })
        .await
    }

    pub async fn installation(&self, id: i64) -> Result<Option<Installation>> {
        self.run(move |c| {
            c.query_row(
                "SELECT id, account, suspended FROM installations WHERE id = ?1",
                [id],
                |r| {
                    Ok(Installation {
                        id: r.get(0)?,
                        account: r.get(1)?,
                        suspended: r.get(2)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// Make `installations` exactly the ones `user` may use (as GitHub listed them at sign-in).
    pub async fn set_memberships(&self, user: i64, installations: Vec<i64>) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM memberships WHERE user_id = ?1", [user])?;
            for installation in installations {
                tx.execute(
                    "INSERT OR IGNORE INTO memberships(user_id, installation_id) VALUES(?1, ?2)",
                    params![user, installation],
                )?;
            }
            tx.commit()
        })
        .await
    }

    pub async fn add_membership(&self, user: i64, installation: i64) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO memberships(user_id, installation_id) VALUES(?1, ?2)",
                params![user, installation],
            )
            .map(|_| ())
        })
        .await
    }

    /// Whether `user` may use `installation` (and it is not suspended).
    pub async fn is_member(&self, user: i64, installation: i64) -> Result<bool> {
        self.run(move |c| {
            c.query_row(
                "SELECT count(*) FROM memberships m JOIN installations i ON i.id = m.installation_id
                 WHERE m.user_id = ?1 AND m.installation_id = ?2 AND i.suspended = 0",
                params![user, installation],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        })
        .await
    }

    /// The installations `user` may use, suspended ones included.
    pub async fn installations_of(&self, user: i64) -> Result<Vec<Installation>> {
        self.run(move |c| {
            c.prepare(
                "SELECT i.id, i.account, i.suspended FROM installations i
                 JOIN memberships m ON m.installation_id = i.id
                 WHERE m.user_id = ?1 ORDER BY i.account, i.id",
            )?
            .query_map([user], |r| {
                Ok(Installation {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    suspended: r.get(2)?,
                })
            })?
            .collect()
        })
        .await
    }

    // ------------------------------------------------------------------ repositories

    pub async fn add_repositories(&self, installation: i64, repos: Vec<Repository>) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            for repo in repos {
                tx.execute(
                    "INSERT INTO repositories(id, installation_id, full_name, private)
                     VALUES(?1, ?2, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET installation_id = excluded.installation_id,
                        full_name = excluded.full_name, private = excluded.private",
                    params![repo.id, installation, repo.full_name, repo.private],
                )?;
            }
            tx.commit()
        })
        .await
    }

    pub async fn remove_repositories(&self, installation: i64, ids: Vec<i64>) -> Result<()> {
        self.run(move |c| {
            let tx = c.transaction()?;
            for id in ids {
                tx.execute(
                    "DELETE FROM repositories WHERE id = ?1 AND installation_id = ?2",
                    params![id, installation],
                )?;
            }
            tx.commit()
        })
        .await
    }

    pub async fn repository(&self, full_name: &str) -> Result<Option<Repository>> {
        let full_name = full_name.to_string();
        self.run(move |c| {
            c.query_row(
                "SELECT id, installation_id, full_name, private FROM repositories
                 WHERE full_name = ?1",
                [full_name],
                |r| {
                    Ok(Repository {
                        id: r.get(0)?,
                        installation: r.get(1)?,
                        full_name: r.get(2)?,
                        private: r.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    /// The repositories `user`'s installations reach (not suspended ones).
    pub async fn repositories_of(&self, user: i64) -> Result<Vec<Repository>> {
        self.run(move |c| {
            c.prepare(
                "SELECT r.id, r.installation_id, r.full_name, r.private FROM repositories r
                 JOIN memberships m ON m.installation_id = r.installation_id
                 JOIN installations i ON i.id = r.installation_id
                 WHERE m.user_id = ?1 AND i.suspended = 0 ORDER BY r.full_name",
            )?
            .query_map([user], |r| {
                Ok(Repository {
                    id: r.get(0)?,
                    installation: r.get(1)?,
                    full_name: r.get(2)?,
                    private: r.get(3)?,
                })
            })?
            .collect()
        })
        .await
    }

    // ------------------------------------------------------------------ databases

    pub async fn add_database(&self, record: DatabaseRecord) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "INSERT INTO databases(id, repository, installation_id, branch, keys, created)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.id,
                    record.repository,
                    record.installation,
                    record.branch,
                    record.keys.as_str(),
                    record.created
                ],
            )
            .map(|_| ())
        })
        .await
    }

    pub async fn database(&self, id: &str) -> Result<Option<DatabaseRecord>> {
        let id = id.to_string();
        self.run(move |c| {
            c.query_row(
                &format!("SELECT {DATABASE_COLUMNS} FROM databases WHERE id = ?1"),
                [id],
                database_row,
            )
            .optional()
        })
        .await
    }

    pub async fn database_on(
        &self,
        repository: &str,
        branch: &str,
    ) -> Result<Option<DatabaseRecord>> {
        let (repository, branch) = (repository.to_string(), branch.to_string());
        self.run(move |c| {
            c.query_row(
                &format!(
                    "SELECT {DATABASE_COLUMNS} FROM databases WHERE repository = ?1 AND branch = ?2"
                ),
                [repository, branch],
                database_row,
            )
            .optional()
        })
        .await
    }

    /// The databases `user` may use.
    pub async fn databases_of(&self, user: i64) -> Result<Vec<DatabaseRecord>> {
        self.run(move |c| {
            let mut statement = c.prepare(&format!(
                "SELECT {} FROM databases d
                 JOIN memberships m ON m.installation_id = d.installation_id
                 JOIN installations i ON i.id = d.installation_id
                 WHERE m.user_id = ?1 AND i.suspended = 0 ORDER BY d.created, d.id",
                DATABASE_COLUMNS
                    .split(", ")
                    .map(|c| format!("d.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))?;
            statement.query_map([user], database_row)?.collect()
        })
        .await
    }

    /// Whether any database is on `repository`.
    pub async fn has_databases_on(&self, repository: &str) -> Result<bool> {
        let repository = repository.to_string();
        self.run(move |c| {
            c.query_row(
                "SELECT count(*) FROM databases WHERE repository = ?1",
                [repository],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        })
        .await
    }

    /// The repositories an installation reaches or has databases on.
    pub async fn repositories_of_installation(&self, installation: i64) -> Result<Vec<String>> {
        self.run(move |c| {
            c.prepare(
                "SELECT full_name FROM repositories WHERE installation_id = ?1
                 UNION SELECT repository FROM databases WHERE installation_id = ?1",
            )?
            .query_map([installation], |r| r.get(0))?
            .collect()
        })
        .await
    }

    /// The database's repository changed (it moved to a new generation).
    pub async fn move_database(&self, id: &str, repository: &str) -> Result<()> {
        let (id, repository) = (id.to_string(), repository.to_string());
        self.run(move |c| {
            c.execute(
                "UPDATE databases SET repository = ?2 WHERE id = ?1",
                params![id, repository],
            )
            .map(|_| ())
        })
        .await
    }

    pub async fn remove_database(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.run(move |c| {
            c.execute("DELETE FROM databases WHERE id = ?1", [id])
                .map(|_| ())
        })
        .await
    }

    // ------------------------------------------------------------------ keys

    /// A new key for `user`. Returns the key itself, which exists only here, and its record.
    pub async fn create_key(
        &self,
        user: i64,
        name: &str,
        kind: KeyKind,
        lifetime: Option<i64>,
    ) -> Result<(String, KeyRecord)> {
        let secret = match kind {
            KeyKind::Api => format!("opk_{}", random_hex(20)),
            KeyKind::Session => format!("ops_{}", random_hex(20)),
        };
        let record = KeyRecord {
            id: format!("key_{}", random_hex(6)),
            user,
            name: name.to_string(),
            kind,
            prefix: secret[..12].to_string(),
            created: now(),
            expires: lifetime.map(|l| now() + l),
            last_used: None,
        };
        let (hashed, stored) = (hash(&secret), record.clone());
        self.run(move |c| {
            c.execute(
                "INSERT INTO keys(id, user_id, name, kind, hash, prefix, created, expires)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    stored.id,
                    stored.user,
                    stored.name,
                    match stored.kind {
                        KeyKind::Api => "api",
                        KeyKind::Session => "session",
                    },
                    hashed,
                    stored.prefix,
                    stored.created,
                    stored.expires
                ],
            )
            .map(|_| ())
        })
        .await?;
        Ok((secret, record))
    }

    /// The user and key a presented key belongs to, if it is valid; notes its use.
    pub async fn authenticate(&self, secret: &str) -> Result<Option<(User, KeyRecord)>> {
        let hashed = hash(secret);
        self.run(move |c| {
            let found = c
                .query_row(
                    &format!("SELECT {KEY_COLUMNS} FROM keys WHERE hash = ?1"),
                    [&hashed],
                    key_row,
                )
                .optional()?;
            let Some(key) = found else { return Ok(None) };
            if key.expires.is_some_and(|e| e <= now()) {
                return Ok(None);
            }
            c.execute(
                "UPDATE keys SET last_used = ?2 WHERE id = ?1",
                params![key.id, now()],
            )?;
            let user = c.query_row(
                "SELECT id, github_id, login FROM users WHERE id = ?1",
                [key.user],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        github_id: r.get(1)?,
                        login: r.get(2)?,
                    })
                },
            )?;
            Ok(Some((user, key)))
        })
        .await
    }

    /// Delete keys (sign-in sessions) that have expired; returns how many.
    pub async fn delete_expired_keys(&self) -> Result<usize> {
        self.run(|c| {
            c.execute(
                "DELETE FROM keys WHERE expires IS NOT NULL AND expires <= ?1",
                [now()],
            )
        })
        .await
    }

    pub async fn keys_of(&self, user: i64) -> Result<Vec<KeyRecord>> {
        self.run(move |c| {
            c.prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM keys WHERE user_id = ?1 AND kind = 'api' ORDER BY created"
            ))?
            .query_map([user], key_row)?
            .collect()
        })
        .await
    }

    /// Revoke one of `user`'s keys; returns whether there was one.
    pub async fn revoke_key(&self, user: i64, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.run(move |c| {
            c.execute(
                "DELETE FROM keys WHERE id = ?1 AND user_id = ?2",
                params![id, user],
            )
            .map(|n| n > 0)
        })
        .await
    }

    // ------------------------------------------------------------------ usage

    /// Add to an installation's usage today.
    pub async fn add_usage(
        &self,
        installation: i64,
        requests: i64,
        github_requests: i64,
        commits: i64,
    ) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "INSERT INTO usage(installation_id, day, requests, github_requests, commits)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(installation_id, day) DO UPDATE SET
                    requests = requests + excluded.requests,
                    github_requests = github_requests + excluded.github_requests,
                    commits = commits + excluded.commits",
                params![
                    installation,
                    now() / 86_400,
                    requests,
                    github_requests,
                    commits
                ],
            )
            .map(|_| ())
        })
        .await
    }

    /// An installation's usage over its last `days` days.
    pub async fn usage(&self, installation: i64, days: i64) -> Result<Vec<Usage>> {
        self.run(move |c| {
            c.prepare(
                "SELECT day, requests, github_requests, commits FROM usage
                 WHERE installation_id = ?1 AND day > ?2 ORDER BY day",
            )?
            .query_map(params![installation, now() / 86_400 - days], |r| {
                Ok(Usage {
                    day: r.get(0)?,
                    requests: r.get(1)?,
                    github_requests: r.get(2)?,
                    commits: r.get(3)?,
                })
            })?
            .collect()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tenancy_and_keys() {
        let meta = Meta::memory().unwrap();
        let ada = meta.upsert_user(1, "ada").await.unwrap();
        let bob = meta.upsert_user(2, "bob").await.unwrap();
        assert_eq!(meta.upsert_user(1, "ada2").await.unwrap().id, ada.id);
        meta.upsert_installation(10, "ada").await.unwrap();
        meta.upsert_installation(20, "bob").await.unwrap();
        meta.set_memberships(ada.id, vec![10]).await.unwrap();
        meta.set_memberships(bob.id, vec![20]).await.unwrap();
        meta.add_repositories(
            10,
            vec![Repository {
                id: 100,
                installation: 10,
                full_name: "ada/db".into(),
                private: true,
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            meta.repository("ADA/DB")
                .await
                .unwrap()
                .unwrap()
                .installation,
            10,
            "names are case-insensitive, as on GitHub"
        );
        let record = DatabaseRecord {
            id: "db_1".into(),
            repository: "ada/db".into(),
            installation: 10,
            branch: "refs/heads/main".into(),
            keys: KeyMode::Kms,
            created: now(),
        };
        meta.add_database(record.clone()).await.unwrap();
        assert!(
            meta.add_database(record.clone()).await.is_err(),
            "one per branch"
        );
        assert_eq!(
            meta.databases_of(ada.id).await.unwrap(),
            std::slice::from_ref(&record)
        );
        assert!(meta.databases_of(bob.id).await.unwrap().is_empty());
        assert!(meta.is_member(ada.id, 10).await.unwrap());
        assert!(!meta.is_member(bob.id, 10).await.unwrap());
        let reachable = meta.repositories_of(ada.id).await.unwrap();
        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].full_name, "ada/db");
        assert!(meta.repositories_of(bob.id).await.unwrap().is_empty());
        assert_eq!(
            meta.installations_of(bob.id).await.unwrap()[0].account,
            "bob"
        );
        assert_eq!(meta.user_by_github_id(2).await.unwrap().unwrap().id, bob.id);
        assert!(meta.user_by_github_id(3).await.unwrap().is_none());
        meta.suspend_installation(10, true).await.unwrap();
        assert!(!meta.is_member(ada.id, 10).await.unwrap());
        meta.suspend_installation(10, false).await.unwrap();

        let (secret, key) = meta
            .create_key(ada.id, "ci", KeyKind::Api, None)
            .await
            .unwrap();
        assert!(secret.starts_with("opk_") && secret.len() == 44);
        let (who, found) = meta.authenticate(&secret).await.unwrap().unwrap();
        assert_eq!((who.id, found.id.clone()), (ada.id, key.id.clone()));
        assert!(meta.authenticate("opk_nope").await.unwrap().is_none());
        let (session, _) = meta
            .create_key(ada.id, "s", KeyKind::Session, Some(-1))
            .await
            .unwrap();
        assert!(
            meta.authenticate(&session).await.unwrap().is_none(),
            "expired"
        );
        assert_eq!(meta.delete_expired_keys().await.unwrap(), 1);
        assert_eq!(meta.keys_of(ada.id).await.unwrap().len(), 1);
        assert!(
            !meta.revoke_key(bob.id, &key.id).await.unwrap(),
            "not bob's"
        );
        assert!(meta.revoke_key(ada.id, &key.id).await.unwrap());
        assert!(meta.authenticate(&secret).await.unwrap().is_none());

        meta.add_usage(10, 3, 5, 1).await.unwrap();
        meta.add_usage(10, 1, 1, 0).await.unwrap();
        let usage = meta.usage(10, 7).await.unwrap();
        assert_eq!((usage[0].requests, usage[0].github_requests), (4, 6));

        meta.delete_installation(10).await.unwrap();
        assert!(
            meta.database("db_1").await.unwrap().is_none(),
            "gone with it"
        );
    }
}
