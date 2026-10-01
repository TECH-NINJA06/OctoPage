use std::sync::Arc;

use octopage_git::ObjectId;

use crate::page::PageId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Transport(Arc<octopage_git::Error>),

    /// A page failed its self-checks (magic, page id, epoch, size).
    #[error("page {page} is corrupt: {reason}")]
    Corrupt { page: PageId, reason: String },

    /// The repository's structure is not an OctoPage database.
    #[error("not a valid OctoPage database: {0}")]
    Format(String),

    #[error("no database on {0}")]
    NoDatabase(String),

    #[error("a database already exists on {0}")]
    AlreadyExists(String),

    /// The network failed while `commit` was being published on top of `base`, so whether it
    /// landed is not known. `PageStore::settle` answers once the remote does.
    #[error("commit {commit} (on {base}) may or may not have landed: {reason}")]
    OutcomeUnknown {
        base: ObjectId,
        commit: ObjectId,
        reason: String,
    },

    /// The head moved to a commit that does not descend from the one this client last saw.
    /// Someone rewrote history; writes are refused until an operator reconciles.
    #[error("head moved to {head}, which does not descend from {base}: history was rewritten")]
    HistoryRewritten { base: ObjectId, head: ObjectId },

    #[error("page {0} is not allocated")]
    NotAllocated(PageId),

    /// GitHub's push protection (secret scanning) refused the commit because a blob looks like
    /// it holds a secret, such as an access token in a row. Resending cannot help; the data
    /// must change, or the secret be allowed through the link in `message`.
    #[error("GitHub push protection blocked the commit: {message}")]
    SecretBlocked {
        /// The blocked pages.
        pages: Vec<PageId>,
        /// Other blocked blobs, by path: `changelog`, `catalog`.
        others: Vec<String>,
        /// GitHub's message: the kind of secret, and a link to allow it.
        message: String,
    },

    /// The database is encrypted and was opened without a way to unlock it.
    #[error("the database is encrypted: open it with its passphrase, recovery key or key provider")]
    Locked,

    /// None of the database's keys opened with what was given.
    #[error("that passphrase or key does not open this database")]
    WrongKey,

    #[error("the 24-bit page id space is exhausted")]
    Full,

    #[error("invalid: {0}")]
    Invalid(String),
}

impl From<octopage_git::Error> for Error {
    fn from(e: octopage_git::Error) -> Self {
        Error::Transport(Arc::new(e))
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

pub(crate) fn format(message: impl Into<String>) -> Error {
    Error::Format(message.into())
}

/// A push rejection from GitHub's push protection, with the blocked pages and blobs from the
/// `path:` lines of its message (`pages/AA/BB/CC:1`, `changelog:3`).
pub(crate) fn secret_blocked(reason: &str) -> Option<Error> {
    let lower = reason.to_ascii_lowercase();
    if !lower.contains("gh013") && !lower.contains("push cannot contain secrets") {
        return None;
    }
    let mut pages = Vec::new();
    let mut others = Vec::new();
    for part in reason.split("path:").skip(1) {
        let path = part
            .trim_start()
            .split(|c: char| c == ':' || c == ';' || c == ')' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        match path.strip_prefix("pages/") {
            Some(fanout) => {
                let hex: String = fanout.split('/').collect();
                if let Some(id) = u32::from_str_radix(&hex, 16)
                    .ok()
                    .filter(|_| hex.len() == 6)
                    .and_then(|id| PageId::new(id).ok())
                    && !pages.contains(&id)
                {
                    pages.push(id);
                }
            }
            None if !path.is_empty() && !others.iter().any(|o| o == path) => {
                others.push(path.to_string())
            }
            None => {}
        }
    }
    Some(Error::SecretBlocked {
        pages,
        others,
        message: reason.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_protection_messages() {
        let reason = "refs/heads/main: pre-receive hook declined (error: GH013: Repository rule \
            violations found for refs/heads/main.; - GITHUB PUSH PROTECTION; Push cannot contain \
            secrets; —— GitHub Personal Access Token ——; locations:; - commit: 8728dbe6; \
            path: pages/00/02/1a:1; - commit: 8728dbe6; path: changelog:12; \
            https://github.com/o/r/security/secret-scanning/unblock-secret/2abc)";
        match secret_blocked(reason) {
            Some(Error::SecretBlocked { pages, others, .. }) => {
                assert_eq!(pages, [PageId::new(0x00021a).unwrap()]);
                assert_eq!(others, ["changelog"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(secret_blocked("refs/heads/main: failed").is_none());
    }
}
