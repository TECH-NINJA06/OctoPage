pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong. New kinds may be added, so match with a catch-all arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Sql(rusqlite::Error),

    #[error(
        "the commit was refused: another client changed the same data first; run the transaction again"
    )]
    Conflict,

    #[error("the transaction lost {attempts} races in a row to other writers")]
    Serialization {
        attempts: u32,
    },

    #[error("the network failed while committing; whether the commit landed is unknown")]
    OutcomeUnknown,

    #[error("GitHub push protection blocked the commit: something that looks like a secret is in {}", places.join("; "))]
    SecretBlocked {
        places: Vec<String>,
        message: String,
    },
    #[error("the repository is unavailable; nothing was committed: {0}")]
    Unavailable(String),

    #[error("the database is full: {0}")]
    Full(String),
    #[error("the repository is public, and this encrypted database requires a private one")]
    PublicRepository,

    #[error(
        "the merge stopped after {merged} commits: replaying {commit} failed at `{statement}`: {reason}"
    )]
    Merge {
        /// Commits merged before this one.
        merged: usize,
        /// The other branch's commit that failed here.
        commit: octopage_git::ObjectId,
        /// Its statement that failed (`COMMIT` if the commit itself did).
        statement: String,
        /// Why.
        reason: String,
    },

    /// An `AS OF` target that names no commit.
    #[error("AS OF '{target}': {reason}")]
    AsOf {
        /// The target as given.
        target: String,
        /// Why it names no commit.
        reason: String,
    },

    /// The page store failed: no database on the branch, a corrupt page, a wrong key.
    #[error(transparent)]
    Store(octopage_pagestore::Error),

    /// The repository refused in a way that retrying will not fix: authentication, a
    /// repository that does not exist.
    #[error(transparent)]
    Transport(octopage_git::Error),

    /// A request that cannot be carried out as asked: a bad branch name, invalid settings.
    #[error("{0}")]
    Invalid(String),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        if octopage_sqlite::is_conflict(&e) {
            Error::Conflict
        } else if octopage_sqlite::is_outcome_unknown(&e) {
            Error::OutcomeUnknown
        } else if octopage_sqlite::is_blocked(&e) {
            // The connection that ran the statement fills in where (`last_blocked`).
            Error::SecretBlocked {
                places: Vec::new(),
                message: String::new(),
            }
        } else if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) {
            Error::Full(
                "the write would grow it past its live-size limit; delete data, or raise the \
                 limit in its settings"
                    .into(),
            )
        } else if e.sqlite_error_code() == Some(rusqlite::ErrorCode::SystemIoFailure) {
            // The VFS reports page-store failures (reads, or a commit that applied nothing) as
            // I/O errors, and keeps the cause for this thread.
            match octopage_sqlite::take_last_error() {
                Some(cause) => Error::from(cause),
                None => Error::Unavailable(e.to_string()),
            }
        } else {
            Error::Sql(e)
        }
    }
}

// A network failure, a rate limit or a server error is `Unavailable` wherever it comes from (a
// statement, `AS OF`, opening a branch), so callers have one error to retry on.
impl From<octopage_pagestore::Error> for Error {
    fn from(e: octopage_pagestore::Error) -> Self {
        match &e {
            octopage_pagestore::Error::Transport(t) if is_transient(t) => {
                Error::Unavailable(t.to_string())
            }
            _ => Error::Store(e),
        }
    }
}

impl From<octopage_git::Error> for Error {
    fn from(e: octopage_git::Error) -> Self {
        if is_transient(&e) {
            Error::Unavailable(e.to_string())
        } else {
            Error::Transport(e)
        }
    }
}

fn is_transient(e: &octopage_git::Error) -> bool {
    use octopage_git::Error as E;
    match e {
        E::Network(_) | E::RateLimited { .. } | E::Remote(_) => true,
        E::Http { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

impl Error {
    /// Whether running the transaction again may succeed: a refused commit or a run of them.
    pub fn is_conflict(&self) -> bool {
        matches!(self, Error::Conflict | Error::Serialization { .. })
    }
}
