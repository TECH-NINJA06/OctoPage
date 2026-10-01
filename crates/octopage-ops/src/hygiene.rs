use octopage::{Result, Transport};
use octopage_git::RefUpdate;

use crate::walk::Commits;

pub const STAGE_PREFIX: &str = "refs/octopage/stage/";

/// A staging ref younger than this may belong to a transaction still running.
pub const STAGE_MAX_AGE: i64 = 24 * 3600;

/// Delete staging refs whose last push is older than `max_age` seconds at time `now`. Returns
/// the refs deleted.
pub async fn clean_staging<T: Transport>(t: &T, now: i64, max_age: i64) -> Result<Vec<String>> {
    let mut commits = Commits::default();
    let mut deleted = Vec::new();
    for stage in t.list_refs(&[STAGE_PREFIX]).await? {
        let (_, commit) = commits.load(t, stage.id).await?;
        if now - commit.committer.seconds < max_age {
            continue;
        }
        match t
            .push(&[RefUpdate::delete(&stage.name, stage.id)], &[])
            .await
        {
            Ok(()) => deleted.push(stage.name),
            // It moved: its transaction is alive after all.
            Err(octopage_git::Error::Conflict { .. }) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(deleted)
}
