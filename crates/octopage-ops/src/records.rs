use std::time::Duration;

use octopage::{Error, ObjectId, Result, Transport};
use octopage_git::{Commit, Object, RefUpdate, Signature, Tree};
use serde::{Deserialize, Serialize};

/// The maintenance lease: a job takes it first, and a second job that finds it held exits
/// (spec: "Two maintenance jobs overlap").
pub const MAINTENANCE_REF: &str = "refs/octopage/maintenance";

/// The writer lease (`octopage_pagestore::LEASE_REF`): cooperating writers wait while it is
/// held, so the job takes it for the moment it seals a repository.
pub const WRITER_LEASE_REF: &str = octopage_pagestore::LEASE_REF;

/// Where a repository's generation is recorded (in every repository after the first).
pub const GENERATION_REF: &str = "refs/octopage/generation";

/// Where the maintenance job records the last head it checked, per database:
/// `refs/octopage/checkpoint/<branch>`. It also keeps that commit reachable, so a history
/// rewritten by hand can be reconciled.
pub const CHECKPOINT_PREFIX: &str = "refs/octopage/checkpoint/";

pub(crate) fn signature(now: i64) -> Signature {
    Signature::new("octopage-maintenance", "octopage@example.invalid", now)
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The value of ref `name`.
pub(crate) async fn read_ref<T: Transport>(t: &T, name: &str) -> Result<Option<ObjectId>> {
    Ok(t.list_refs(&[name])
        .await?
        .into_iter()
        .find(|r| r.name == name)
        .map(|r| r.id))
}

/// One object by id.
pub(crate) async fn object<T: Transport>(t: &T, id: ObjectId) -> Result<Object> {
    t.fetch(&[id])
        .await?
        .into_iter()
        .find(|o| o.id() == id)
        .ok_or_else(|| Error::from(octopage_git::Error::MissingObjects(vec![id])))
}

/// A record commit: the empty tree, a subject, and a body.
fn record_commit(parent: Option<ObjectId>, message: String) -> Result<(Object, Object)> {
    let tree = Tree::new().to_object()?;
    let commit = Commit {
        tree: tree.id(),
        parents: parent.into_iter().collect(),
        author: signature(now()),
        committer: signature(now()),
        message: message.into_bytes(),
    }
    .to_object()?;
    Ok((tree, commit))
}

/// A lease held in a ref: a commit whose message names the holder and an expiry, in the writer
/// lease's format, changed only by compare-and-swap.
#[derive(Clone, Debug)]
pub struct RefLease {
    pub name: String,
    pub holder: String,
    /// Milliseconds since the Unix epoch.
    pub expires: i64,
    commit: ObjectId,
}

fn parse_lease(name: &str, id: ObjectId, object: &Object) -> Option<RefLease> {
    let commit = Commit::decode(object.data()).ok()?;
    let message = String::from_utf8_lossy(&commit.message);
    let field = |key: &str| {
        message
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(str::trim)
    };
    Some(RefLease {
        name: name.to_string(),
        holder: field("holder:")?.to_string(),
        expires: field("expires:")?.parse().ok()?,
        commit: id,
    })
}

/// Take the lease in ref `name` for `ttl`, waiting up to `wait` for another holder's live
/// lease to be released or to expire. `None`: someone else still holds it.
pub async fn take_lease<T: Transport>(
    t: &T,
    name: &str,
    holder: &str,
    ttl: Duration,
    wait: Duration,
) -> Result<Option<RefLease>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let current = read_ref(t, name).await?;
        let lease = match current {
            Some(id) => parse_lease(name, id, &object(t, id).await?),
            None => None,
        };
        if let Some(lease) = &lease
            && lease.holder != holder
            && lease.expires > unix_millis()
        {
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let expires = unix_millis() + ttl.as_millis() as i64;
        let (tree, commit) = record_commit(
            None,
            format!("octopage lease\n\nholder: {holder}\nexpires: {expires}\n"),
        )?;
        let update = match current {
            Some(old) => RefUpdate::update(name, old, commit.id()),
            None => RefUpdate::create(name, commit.id()),
        };
        match t
            .push(&[update], &[tree.into(), commit.clone().into()])
            .await
        {
            Ok(()) => {
                return Ok(Some(RefLease {
                    name: name.to_string(),
                    holder: holder.to_string(),
                    expires,
                    commit: commit.id(),
                }));
            }
            // Someone moved it first: look again.
            Err(octopage_git::Error::Conflict { .. } | octopage_git::Error::Rejected(_)) => {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

impl RefLease {
    /// Extend the lease by `ttl` from now. Fails if someone else took it meanwhile.
    pub async fn renew<T: Transport>(&mut self, t: &T, ttl: Duration) -> Result<()> {
        let expires = unix_millis() + ttl.as_millis() as i64;
        let (tree, commit) = record_commit(
            None,
            format!(
                "octopage lease\n\nholder: {}\nexpires: {expires}\n",
                self.holder
            ),
        )?;
        t.push(
            &[RefUpdate::update(&self.name, self.commit, commit.id())],
            &[tree.into(), commit.clone().into()],
        )
        .await?;
        self.commit = commit.id();
        self.expires = expires;
        Ok(())
    }

    /// Give the lease up. Failing is harmless: it expires.
    pub async fn release<T: Transport>(self, t: &T) {
        if let Err(error) = t
            .push(&[RefUpdate::delete(&self.name, self.commit)], &[])
            .await
        {
            tracing::debug!(%error, lease = %self.name, "could not release a lease");
        }
    }
}

/// What a repository records about its generation (`refs/octopage/generation`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRecord {
    /// This repository's generation (the first repository is 1 and has no record).
    pub generation: u64,
    /// The repository the databases moved out of.
    pub previous: Option<String>,
    /// When the copy began, and when the previous repository was sealed (seconds since the
    /// Unix epoch).
    pub created: i64,
    pub sealed: Option<i64>,
    /// The maintenance job deleted the previous repository after the grace period.
    pub previous_deleted: bool,
}

impl GenerationRecord {
    /// The record, and the commit holding it, if the repository has one.
    pub async fn read<T: Transport>(t: &T) -> Result<Option<(ObjectId, GenerationRecord)>> {
        let Some(id) = read_ref(t, GENERATION_REF).await? else {
            return Ok(None);
        };
        let commit = Commit::decode(object(t, id).await?.data()).map_err(Error::from)?;
        let message = String::from_utf8_lossy(&commit.message);
        let json = message
            .find('{')
            .map(|at| &message[at..])
            .ok_or_else(|| Error::Invalid(format!("{GENERATION_REF} holds no record")))?;
        let record = serde_json::from_str(json.trim())
            .map_err(|e| Error::Invalid(format!("{GENERATION_REF}: {e}")))?;
        Ok(Some((id, record)))
    }

    /// Write the record over the commit `previous` (compare-and-swap). Returns the new commit.
    pub async fn write<T: Transport>(&self, t: &T, previous: Option<ObjectId>) -> Result<ObjectId> {
        let json = serde_json::to_string_pretty(self).expect("serializes");
        let (tree, commit) = record_commit(
            previous,
            format!("octopage generation {}\n\n{json}\n", self.generation),
        )?;
        let update = match previous {
            Some(old) => RefUpdate::update(GENERATION_REF, old, commit.id()),
            None => RefUpdate::create(GENERATION_REF, commit.id()),
        };
        t.push(&[update], &[tree.into(), commit.clone().into()])
            .await?;
        Ok(commit.id())
    }
}

/// The checkpoint ref for branch `refs/heads/<name>`.
pub fn checkpoint_ref(branch: &str) -> String {
    format!(
        "{CHECKPOINT_PREFIX}{}",
        branch.strip_prefix("refs/heads/").unwrap_or(branch)
    )
}

/// The branch a checkpoint ref belongs to.
pub fn checkpoint_branch(checkpoint: &str) -> Option<String> {
    checkpoint
        .strip_prefix(CHECKPOINT_PREFIX)
        .map(|name| format!("refs/heads/{name}"))
}

/// The name of the next generation's repository: `OWNER/NAME-g<n>`, replacing an earlier
/// generation's suffix.
pub fn next_location(location: &str, generation: u64) -> String {
    let (dir, name) = match location.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, location),
    };
    let base = match name.rsplit_once("-g") {
        Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => name,
    };
    let next = format!("{base}-g{generation}");
    match dir {
        Some(dir) => format!("{dir}/{next}"),
        None => next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_names() {
        assert_eq!(next_location("o/db", 2), "o/db-g2");
        assert_eq!(next_location("o/db-g2", 3), "o/db-g3");
        assert_eq!(next_location("db-g12", 13), "db-g13");
        assert_eq!(next_location("o/my-good-db", 2), "o/my-good-db-g2");
        assert_eq!(next_location("o/db-gx", 2), "o/db-gx-g2");
    }

    #[test]
    fn checkpoint_names() {
        assert_eq!(
            checkpoint_ref("refs/heads/main"),
            "refs/octopage/checkpoint/main"
        );
        assert_eq!(
            checkpoint_branch("refs/octopage/checkpoint/a/b").as_deref(),
            Some("refs/heads/a/b")
        );
    }
}
