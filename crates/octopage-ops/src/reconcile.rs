use octopage::{Database, Error, ObjectId, Result, Transport};
use octopage_git::RefUpdate;

use crate::records::{checkpoint_ref, read_ref};
use crate::walk::Commits;

/// How a branch's head and its checkpoint differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub branch: String,
    /// The head the maintenance job checked last, and the head now.
    pub checkpoint: ObjectId,
    pub head: ObjectId,
    /// The last commit both histories have, if any.
    pub base: Option<ObjectId>,
    /// Commits only the old history has, oldest first: what the rewrite dropped.
    pub lost: Vec<ObjectId>,
    /// Commits only the new history has, oldest first.
    pub added: Vec<ObjectId>,
}

/// How `branch`'s head differs from its checkpoint; `None` when it descends from it (or there
/// is no checkpoint yet).
pub async fn divergence<T: Transport>(t: &T, branch: &str) -> Result<Option<Divergence>> {
    let Some(checkpoint) = read_ref(t, &checkpoint_ref(branch)).await? else {
        return Ok(None);
    };
    let head = read_ref(t, branch)
        .await?
        .ok_or_else(|| Error::Invalid(format!("there is no branch {branch}")))?;
    let mut commits = Commits::default();
    let new = commits.chain(t, head, None).await?;
    if new.contains(&checkpoint) {
        return Ok(None);
    }
    let old = commits.chain(t, checkpoint, None).await?;
    let base = old.iter().find(|id| new.contains(id)).copied();
    let only = |chain: &[ObjectId]| -> Vec<ObjectId> {
        let mut out: Vec<ObjectId> = chain
            .iter()
            .take_while(|id| Some(**id) != base)
            .copied()
            .collect();
        out.reverse();
        out
    };
    Ok(Some(Divergence {
        branch: branch.to_string(),
        checkpoint,
        head,
        base,
        lost: only(&old),
        added: only(&new),
    }))
}

/// Accept the rewritten history: move the checkpoint to the head, which lets the dropped
/// commits go (GitHub prunes them in time).
pub async fn accept<T: Transport>(t: &T, d: &Divergence) -> Result<()> {
    t.push(
        &[RefUpdate::update(
            checkpoint_ref(&d.branch),
            d.checkpoint,
            d.head,
        )],
        &[],
    )
    .await?;
    Ok(())
}

/// Run the dropped commits' changelogs again on the current head, oldest first, one commit
/// each, then move the checkpoint to the result. Returns each dropped commit with the commit
/// that restored it (or `None` if it recorded nothing to run). `db` is the database on
/// `d.branch`; for an encrypted one it needs its key.
///
/// Call it off the async runtime (it uses a SQLite connection).
pub fn restore<T: Transport + 'static>(
    db: &Database<T>,
    d: &Divergence,
) -> Result<Vec<(ObjectId, Option<ObjectId>)>> {
    let runtime = tokio::runtime::Handle::current();
    let conn = db.connect()?;
    let mut restored = Vec::new();
    for commit in &d.lost {
        let snapshot = runtime.block_on(db.store().snapshot(*commit))?;
        let bytes = runtime.block_on(snapshot.changelog())?;
        let changelog = octopage::changelog::decode(&bytes).unwrap_or_default();
        if changelog.statements.is_empty() {
            restored.push((*commit, None));
            continue;
        }
        conn.replay(&changelog).map_err(|e| {
            Error::Invalid(format!(
                "restoring commit {commit} failed ({e}); the commits before it were restored"
            ))
        })?;
        restored.push((*commit, conn.last_commit().map(|c| c.head)));
    }
    let head = runtime.block_on(db.refresh())?;
    runtime.block_on(db.store().transport().push(
        &[RefUpdate::update(
            checkpoint_ref(&d.branch),
            d.checkpoint,
            head,
        )],
        &[],
    ))?;
    Ok(restored)
}
