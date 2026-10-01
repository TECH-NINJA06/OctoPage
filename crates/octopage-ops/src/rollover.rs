use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use octopage::{Error, ObjectId, Result, Settings, Transport};
use octopage_git::{Commit, NewObject, Object, ObjectKind, Ref, RefUpdate, Tree, TreeEntry};
use octopage_pagestore::{GEN_PREFIX, Moved, REWRITTEN_FROM, Root};

use crate::host::Host;
use crate::records::{
    self, GenerationRecord, RefLease, WRITER_LEASE_REF, next_location, read_ref, signature,
    take_lease,
};
use crate::retention::{self, Dated};
use crate::walk::{Commits, database_root, root_tree, tree_closure};

/// Where objects too many for one push wait, reachable, until their commits go up.
const STAGE_REF: &str = "refs/octopage/stage/rollover";

#[derive(Clone, Debug)]
pub struct RolloverOptions {
    /// The new repository. By default `<name>-g<n+1>` beside the old one.
    pub location: Option<String>,
    /// The time retention is measured from (seconds since the Unix epoch); by default, now.
    pub now: Option<i64>,
    /// Once this many bytes of objects are waiting, they go up ahead of their commits.
    pub batch_bytes: usize,
    /// How long cooperating writers pause while the job seals the old repository.
    pub seal_lease: Duration,
    /// Rounds of catching up with writers before giving up.
    pub max_rounds: u32,
}

impl Default for RolloverOptions {
    fn default() -> Self {
        RolloverOptions {
            location: None,
            now: None,
            batch_bytes: 32 << 20,
            seal_lease: Duration::from_secs(15),
            max_rounds: 30,
        }
    }
}

/// One branch after a rollover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchReport {
    pub branch: String,
    /// A database (with a page map), as opposed to a branch copied exactly.
    pub database: bool,
    /// Commits in its history before, and kept.
    pub commits: usize,
    pub kept: usize,
    /// Its head in the new repository.
    pub head: ObjectId,
}

/// What a rollover did.
#[derive(Clone, Debug)]
pub struct RolloverReport {
    pub from: String,
    pub to: String,
    /// The new repository's generation.
    pub generation: u64,
    pub branches: Vec<BranchReport>,
    /// Objects and bytes copied.
    pub objects: usize,
    pub bytes: u64,
    /// Rounds of copying: 2 means nobody committed during the final catch-up.
    pub rounds: u32,
    /// Tags that were not copied (annotated tags on dropped history, say).
    pub skipped: Vec<String>,
}

/// Move every database in `source`'s repository to a new generation. See the module docs.
pub async fn rollover<T: Transport + 'static, H: Host>(
    source: Arc<T>,
    host: &H,
    options: &RolloverOptions,
) -> Result<RolloverReport> {
    let from = source
        .location()
        .ok_or_else(|| Error::Invalid("the transport cannot say where its repository is".into()))?;
    let all = source.list_refs(&[]).await?;
    if let Some(pointer) = all.iter().find(|r| r.name.starts_with(GEN_PREFIX)) {
        return Err(Error::Invalid(format!(
            "{from} has moved to a new generation already ({})",
            pointer.name
        )));
    }
    let mut commits = Commits::default();
    check_checkpoints(&*source, &all, &mut commits).await?;
    let generation = GenerationRecord::read(&*source)
        .await?
        .map_or(1, |(_, r)| r.generation);
    let next = generation + 1;
    let to = options
        .location
        .clone()
        .unwrap_or_else(|| next_location(&from, next));
    let public = source.is_public().await.ok().flatten().unwrap_or(false);
    let target = if host.exists(&to).await? {
        // Resume an interrupted rollover into it, if that is what it is.
        let target = Arc::new((*source).relocate(&to)?);
        match GenerationRecord::read(&*target).await? {
            Some((_, r)) if r.previous.as_deref() == Some(from.as_str()) && r.sealed.is_none() => {
                target
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "{to} exists already, and is not an unfinished rollover of {from}"
                )));
            }
        }
    } else {
        host.create_repository(&to, public).await?;
        let target = Arc::new((*source).relocate(&to)?);
        GenerationRecord {
            generation: next,
            previous: Some(from.clone()),
            created: records::now(),
            sealed: None,
            previous_deleted: false,
        }
        .write(&*target, None)
        .await?;
        target
    };
    tracing::info!(%from, %to, generation = next, "rolling over");

    let mut copy = Copy::new(
        source.clone(),
        target.clone(),
        options.now.unwrap_or_else(records::now),
        options.batch_bytes,
        commits,
    )
    .await?;
    let holder = format!("octopage-maintenance-{:016x}", fastrand::u64(..));
    let mut lease: Option<RefLease> = None;
    let mut asked_for_lease = false;
    let mut rounds = 0;
    let sealed = loop {
        rounds += 1;
        if rounds > options.max_rounds {
            break Err(Error::Invalid(format!(
                "writers kept committing: gave up sealing {from} after {} rounds (nothing \
                 changed there; run the rollover again)",
                options.max_rounds
            )));
        }
        let refs = source.list_refs(&["refs/heads/", "refs/tags/"]).await?;
        if let Err(e) = copy.sync(&refs).await {
            break Err(e);
        }
        if !asked_for_lease {
            // Pause cooperating writers for the last catch-up and the seal. If a writer holds
            // the lease past the wait, seal anyway: the compare-and-swap still protects.
            asked_for_lease = true;
            lease = take_lease(
                &*source,
                WRITER_LEASE_REF,
                &holder,
                options.seal_lease,
                options.seal_lease,
            )
            .await?;
            continue;
        }
        if let Some(lease) = &mut lease
            && let Err(error) = lease.renew(&*source, options.seal_lease).await
        {
            tracing::debug!(%error, "could not renew the writer lease");
        }
        let databases: Vec<(String, ObjectId)> = refs
            .iter()
            .filter(|r| copy.branches.get(&r.name).is_some_and(|b| b.database))
            .map(|r| (r.name.clone(), r.id))
            .collect();
        match seal(&*source, &mut copy.commits, &databases, &to, next).await {
            Ok(true) => break Ok(()),
            Ok(false) => {
                tracing::info!(round = rounds, "a writer committed meanwhile; catching up")
            }
            Err(e) => break Err(e),
        }
    };
    if let Some(lease) = lease {
        lease.release(&*source).await;
    }
    sealed?;
    copy.finish().await?;
    if let Some((commit, mut record)) = GenerationRecord::read(&*target).await? {
        record.sealed = Some(records::now());
        record.write(&*target, Some(commit)).await?;
    }
    if let Ok(Some(branch)) = host.default_branch(&from).await
        && copy
            .target_refs
            .contains_key(&format!("refs/heads/{branch}"))
        && let Err(error) = host.set_default_branch(&to, &branch).await
    {
        tracing::warn!(%error, "could not set the new repository's default branch");
    }
    let branches = copy
        .branches
        .iter()
        .map(|(name, b)| BranchReport {
            branch: name.clone(),
            database: b.database,
            commits: b.commits,
            kept: b.kept,
            head: copy.target_refs[name],
        })
        .collect();
    Ok(RolloverReport {
        from,
        to,
        generation: next,
        branches,
        objects: copy.objects,
        bytes: copy.bytes,
        rounds,
        skipped: copy.skipped,
    })
}

/// Refuse to roll over a database whose history was rewritten by hand and not reconciled: the
/// checkpoint keeps the lost commits, and the old repository will be deleted.
pub(crate) async fn check_checkpoints<T: Transport>(
    source: &T,
    refs: &[Ref],
    commits: &mut Commits,
) -> Result<()> {
    for checkpoint in refs
        .iter()
        .filter(|r| r.name.starts_with(records::CHECKPOINT_PREFIX))
    {
        let Some(branch) = records::checkpoint_branch(&checkpoint.name) else {
            continue;
        };
        let Some(head) = refs.iter().find(|r| r.name == branch) else {
            continue;
        };
        let chain = commits.chain(source, head.id, Some(checkpoint.id)).await?;
        let reached = chain
            .last()
            .and_then(|last| commits.get(*last))
            .is_some_and(|(_, c)| c.parents.first() == Some(&checkpoint.id))
            || head.id == checkpoint.id;
        if !reached {
            return Err(Error::Invalid(format!(
                "the history of {branch} was rewritten since the maintenance job last checked \
                 it: run `octopage reconcile` before rolling over"
            )));
        }
    }
    Ok(())
}

struct BranchState {
    database: bool,
    /// The source head copied so far.
    copied: Option<ObjectId>,
    commits: usize,
    kept: usize,
}

pub(crate) struct Copy<T> {
    source: Arc<T>,
    target: Arc<T>,
    now: i64,
    batch_bytes: usize,
    commits: Commits,
    /// Objects the target has (or that are on their way in `pending`).
    present: HashSet<ObjectId>,
    /// Kept database commits, and their copies.
    rewritten: HashMap<ObjectId, ObjectId>,
    kept: HashSet<ObjectId>,
    branches: BTreeMap<String, BranchState>,
    target_refs: HashMap<String, ObjectId>,
    pending: Vec<NewObject>,
    pending_commits: Vec<NewObject>,
    pending_bytes: usize,
    stage_tip: Option<ObjectId>,
    objects: usize,
    bytes: u64,
    skipped: Vec<String>,
}

impl<T: Transport + 'static> Copy<T> {
    pub(crate) async fn new(
        source: Arc<T>,
        target: Arc<T>,
        now: i64,
        batch_bytes: usize,
        commits: Commits,
    ) -> Result<Self> {
        let target_refs = target
            .list_refs(&["refs/heads/", "refs/tags/"])
            .await?
            .into_iter()
            .map(|r| (r.name, r.id))
            .collect();
        Ok(Copy {
            source,
            target,
            now,
            batch_bytes,
            commits,
            present: HashSet::new(),
            rewritten: HashMap::new(),
            kept: HashSet::new(),
            branches: BTreeMap::new(),
            target_refs,
            pending: Vec::new(),
            pending_commits: Vec::new(),
            pending_bytes: 0,
            stage_tip: None,
            objects: 0,
            bytes: 0,
            skipped: Vec::new(),
        })
    }

    /// Copy every branch and tag of `refs` to where it is now.
    pub(crate) async fn sync(&mut self, refs: &[Ref]) -> Result<()> {
        let heads: Vec<&Ref> = refs
            .iter()
            .filter(|r| r.name.starts_with("refs/heads/"))
            .collect();
        let tagged: HashSet<ObjectId> = refs
            .iter()
            .filter(|r| r.name.starts_with("refs/tags/"))
            .map(|r| r.id)
            .collect();
        // First, what to keep of every database not seen before: all of them before copying
        // any, so history they share is copied the same way for all.
        for head in &heads {
            if self.branches.contains_key(&head.name) {
                continue;
            }
            let (_, commit) = self.commits.load(&*self.source, head.id).await?;
            let tree = root_tree(&*self.source, commit.tree).await?;
            let Some(root) = database_root(&tree) else {
                self.branches.insert(
                    head.name.clone(),
                    BranchState {
                        database: false,
                        copied: None,
                        commits: 0,
                        kept: 0,
                    },
                );
                continue;
            };
            if root.moved.is_some() {
                return Err(Error::Invalid(format!(
                    "{} is sealed already: the repository is half rolled over",
                    head.name
                )));
            }
            let settings = match root.settings {
                Some(id) => Settings::parse(records::object(&*self.source, id).await?.data())
                    .map_err(Error::Invalid)?,
                None => Settings::default(),
            };
            let chain = self.commits.chain(&*self.source, head.id, None).await?;
            let dated: Vec<Dated> = chain
                .iter()
                .map(|id| Dated {
                    commit: *id,
                    time: self
                        .commits
                        .get(*id)
                        .map_or(0, |(_, c)| c.committer.seconds),
                })
                .collect();
            let kept = retention::kept(&dated, &settings, &tagged, self.now);
            self.kept.extend(kept.iter().copied());
            self.branches.insert(
                head.name.clone(),
                BranchState {
                    database: true,
                    copied: None,
                    commits: chain.len(),
                    kept: kept.len(),
                },
            );
        }
        for head in &heads {
            let state = &self.branches[&head.name];
            if state.copied == Some(head.id) {
                continue;
            }
            if state.database {
                self.copy_database(&head.name, head.id).await?;
            } else {
                self.copy_exactly(&head.name, head.id).await?;
            }
        }
        // Branches deleted in the old repository meanwhile.
        let gone: Vec<String> = self
            .branches
            .keys()
            .filter(|name| !heads.iter().any(|h| &&h.name == name))
            .cloned()
            .collect();
        for name in gone {
            self.branches.remove(&name);
            if let Some(old) = self.target_refs.remove(&name) {
                self.publish(vec![RefUpdate::delete(&name, old)]).await?;
            }
        }
        // Tags on kept or exactly copied commits.
        for tag in refs.iter().filter(|r| r.name.starts_with("refs/tags/")) {
            let target = match self.rewritten.get(&tag.id) {
                Some(copy) => *copy,
                None if self.present.contains(&tag.id) => tag.id,
                None => {
                    if !self.skipped.contains(&tag.name) {
                        self.skipped.push(tag.name.clone());
                    }
                    continue;
                }
            };
            let update = match self.target_refs.get(&tag.name) {
                Some(old) if *old == target => continue,
                Some(old) => RefUpdate::update(&tag.name, *old, target),
                None => RefUpdate::create(&tag.name, target),
            };
            self.publish(vec![update]).await?;
            self.target_refs.insert(tag.name.clone(), target);
        }
        Ok(())
    }

    /// Copy a database branch's kept commits up to `head`.
    async fn copy_database(&mut self, name: &str, head: ObjectId) -> Result<()> {
        let copied = self.branches[name].copied;
        let chain = self.commits.chain(&*self.source, head, copied).await?;
        if let Some(copied) = copied {
            let reached = chain
                .last()
                .and_then(|last| self.commits.get(*last))
                .is_some_and(|(_, c)| c.parents.first() == Some(&copied));
            if !reached {
                return Err(Error::Invalid(format!(
                    "the history of {name} was rewritten during the rollover"
                )));
            }
            // Everything that landed since the copy began is recent: keep it all.
            self.kept.extend(chain.iter().copied());
            let state = self.branches.get_mut(name).expect("known branch");
            state.commits += chain.len();
            state.kept += chain.len();
        }
        let mut parent = copied.map(|c| self.rewritten[&c]);
        let mut previous: Option<ObjectId> = copied;
        for id in chain.iter().rev() {
            if !self.kept.contains(id) {
                continue;
            }
            if let Some(copy) = self.rewritten.get(id) {
                parent = Some(*copy); // shared with a branch copied already
                previous = Some(*id);
                continue;
            }
            let (object, commit) = self.commits.get(*id).cloned().expect("in the chain");
            // The trees it adds over the previous kept commit, in one round trip, when that is
            // its parent (with a gap, the walk below fetches what it needs).
            let hint = match previous {
                None => Some(None),
                Some(p) if commit.parents.first() == Some(&p) => Some(Some(p)),
                Some(_) => None,
            };
            self.copy_tree(commit.tree, *id, hint).await?;
            let copy = rewrite(&object, &commit, *id, parent)?;
            self.rewritten.insert(*id, copy.id());
            self.add_commit(copy.clone());
            parent = Some(copy.id());
            previous = Some(*id);
        }
        let new_head = parent.expect("the head is always kept");
        self.set_ref(name, new_head).await?;
        self.branches.get_mut(name).expect("known branch").copied = Some(head);
        Ok(())
    }

    /// Copy a branch that is not a database exactly: every commit, tree and blob.
    async fn copy_exactly(&mut self, name: &str, head: ObjectId) -> Result<()> {
        let missing = self
            .commits
            .closure(&*self.source, head, &self.present)
            .await?;
        let state = self.branches.get_mut(name).expect("known branch");
        state.commits += missing.len();
        state.kept += missing.len();
        for id in missing {
            let (object, commit) = self.commits.get(id).cloned().expect("loaded");
            self.copy_tree(commit.tree, id, None).await?;
            self.add_commit(object);
        }
        self.set_ref(name, head).await?;
        self.branches.get_mut(name).expect("known branch").copied = Some(head);
        Ok(())
    }

    /// Point `name` at `id` in the target, with whatever is pending.
    async fn set_ref(&mut self, name: &str, id: ObjectId) -> Result<()> {
        let update = match self.target_refs.get(name) {
            Some(old) if *old == id => return self.publish(Vec::new()).await,
            Some(old) => RefUpdate::update(name, *old, id),
            None => RefUpdate::create(name, id),
        };
        self.publish(vec![update]).await?;
        self.target_refs.insert(name.to_string(), id);
        Ok(())
    }

    /// Queue the objects of tree `tree` (of commit `commit`) that the target lacks. `hint`:
    /// fetch the trees through the history since `Some(parent)` (or all of them, `Some(None)`)
    /// in one round trip.
    async fn copy_tree(
        &mut self,
        tree: ObjectId,
        commit: ObjectId,
        hint: Option<Option<ObjectId>>,
    ) -> Result<()> {
        let mut trees: HashMap<ObjectId, Object> = HashMap::new();
        if let Some(have) = hint
            && !self.present.contains(&tree)
        {
            for object in self.source.history(commit, have, true).await? {
                if object.kind() == ObjectKind::Tree {
                    trees.insert(object.id(), object);
                }
            }
        }
        let (new_trees, blobs) =
            tree_closure(&*self.source, tree, &self.present, &mut trees).await?;
        for object in new_trees {
            self.add(object).await?;
        }
        for chunk in blobs.chunks(256) {
            for object in self.source.fetch(chunk).await? {
                self.add(object).await?;
            }
        }
        Ok(())
    }

    async fn add(&mut self, object: Object) -> Result<()> {
        if !self.present.insert(object.id()) {
            return Ok(());
        }
        let len = object.data().len();
        self.objects += 1;
        self.bytes += len as u64;
        self.pending_bytes += len;
        self.pending.push(object.into());
        if self.pending_bytes >= self.batch_bytes {
            self.stage().await?;
        }
        Ok(())
    }

    fn add_commit(&mut self, commit: Object) {
        if self.present.insert(commit.id()) {
            self.objects += 1;
            self.bytes += commit.data().len() as u64;
            self.pending_commits.push(commit.into());
        }
    }

    /// Send the waiting trees and blobs ahead of their commits, reachable from the stage ref.
    async fn stage(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut tree = Tree::new();
        for object in &self.pending {
            let id = object.object.id();
            let entry = match object.object.kind() {
                ObjectKind::Tree => TreeEntry::tree(id.to_hex(), id),
                _ => TreeEntry::blob(id.to_hex(), id),
            };
            tree.insert(entry).map_err(Error::from)?;
        }
        let tree = tree.to_object()?;
        let commit = Commit {
            tree: tree.id(),
            parents: self.stage_tip.into_iter().collect(),
            author: signature(records::now()),
            committer: signature(records::now()),
            message: b"octopage rollover: objects on their way\n".to_vec(),
        }
        .to_object()?;
        let update = match self.stage_tip {
            None => RefUpdate::create(STAGE_REF, commit.id()),
            Some(tip) => RefUpdate::update(STAGE_REF, tip, commit.id()),
        };
        let mut objects = std::mem::take(&mut self.pending);
        objects.push(tree.into());
        objects.push(commit.clone().into());
        self.target.push(&[update], &objects).await?;
        self.stage_tip = Some(commit.id());
        self.pending_bytes = 0;
        Ok(())
    }

    /// Push everything waiting, with `updates`.
    async fn publish(&mut self, updates: Vec<RefUpdate>) -> Result<()> {
        let mut objects = std::mem::take(&mut self.pending);
        objects.append(&mut self.pending_commits);
        if updates.is_empty() {
            if objects.is_empty() {
                return Ok(());
            }
            // Nothing to point at them yet: keep them for the next push.
            let (commits, others): (Vec<NewObject>, Vec<NewObject>) = objects
                .into_iter()
                .partition(|o| o.object.kind() == ObjectKind::Commit);
            self.pending = others;
            self.pending_commits = commits;
            return Ok(());
        }
        self.target.push(&updates, &objects).await?;
        self.pending_bytes = 0;
        Ok(())
    }

    /// Remove the stage ref.
    pub(crate) async fn finish(&mut self) -> Result<()> {
        if let Some(tip) = self.stage_tip.take() {
            self.target
                .push(&[RefUpdate::delete(STAGE_REF, tip)], &[])
                .await?;
        }
        Ok(())
    }
}

/// Move each database's head in the old repository (`databases`: branch and the head that was
/// copied) to a commit that says where it went, and write the generation pointer, in one
/// atomic push. `false`: a head moved since it was copied (nothing was applied).
pub(crate) async fn seal<T: Transport>(
    source: &T,
    commits: &mut Commits,
    databases: &[(String, ObjectId)],
    to: &str,
    generation: u64,
) -> Result<bool> {
    let moved = Moved {
        generation,
        location: to.to_string(),
    };
    let blob = Object::blob(moved.to_bytes())?;
    let (pointer_tree, pointer) = moved.pointer(signature(records::now()))?;
    let mut objects: Vec<NewObject> = vec![
        blob.clone().into(),
        pointer_tree.into(),
        pointer.clone().into(),
    ];
    let mut updates = Vec::new();
    for (branch, head) in databases {
        let (_, commit) = commits.load(source, *head).await?.clone();
        let mut root = Root::decode(&root_tree(source, commit.tree).await?)?;
        root.moved = Some(blob.id());
        let tree = root.to_tree().to_object()?;
        let seal = Commit {
            tree: tree.id(),
            parents: vec![*head],
            author: signature(records::now()),
            committer: signature(records::now()),
            message: format!(
                "octopage: moved to {to} (generation {generation})
"
            )
            .into_bytes(),
        }
        .to_object()?;
        objects.push(tree.into());
        objects.push(seal.clone().into());
        updates.push(RefUpdate::update(branch, *head, seal.id()));
    }
    updates.push(RefUpdate::create(moved.ref_name(), pointer.id()));
    match source.push(&updates, &objects).await {
        Ok(()) => Ok(true),
        Err(octopage_git::Error::Conflict { .. } | octopage_git::Error::Rejected(_)) => Ok(false),
        Err(octopage_git::Error::PushOutcomeUnknown(reason)) => {
            tracing::warn!(%reason, "the seal's outcome is unknown; reading the pointer");
            Ok(read_ref(source, &moved.ref_name()).await? == Some(pointer.id()))
        }
        Err(e) => Err(e.into()),
    }
}

/// The copy of `commit` (id `id`) with first parent `parent`: the original itself when that
/// is its parent already, otherwise a new commit with the same tree, authors and message, and
/// a trailer naming the original.
fn rewrite(
    original: &Object,
    commit: &Commit,
    id: ObjectId,
    parent: Option<ObjectId>,
) -> Result<Object> {
    if commit.parents.first().copied() == parent && commit.parents.len() <= 1 {
        return Ok(original.clone());
    }
    let text = String::from_utf8_lossy(&commit.message);
    let body: Vec<&str> = text
        .trim_end()
        .lines()
        .filter(|line| !line.starts_with(REWRITTEN_FROM))
        .collect();
    let body = body.join("\n");
    let message = format!("{}\n\n{REWRITTEN_FROM}{}\n", body.trim_end(), id.to_hex());
    Ok(Commit {
        tree: commit.tree,
        parents: parent.into_iter().collect(),
        author: commit.author.clone(),
        committer: commit.committer.clone(),
        message: message.into_bytes(),
    }
    .to_object()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octopage_git::Signature;

    #[test]
    fn copies_name_their_originals() {
        let commit = Commit {
            tree: ObjectId::from_array([1; 20]),
            parents: vec![ObjectId::from_array([2; 20])],
            author: Signature::new("a", "a@x", 5),
            committer: Signature::new("a", "a@x", 5),
            message: b"octopage txn 42\n".to_vec(),
        };
        let original = commit.to_object().unwrap();
        let id = original.id();
        // Same parent: the very same commit.
        let same = rewrite(&original, &commit, id, Some(ObjectId::from_array([2; 20]))).unwrap();
        assert_eq!(same.id(), id);
        // New parent: a new commit that names the original.
        let copy = rewrite(&original, &commit, id, None).unwrap();
        let decoded = Commit::decode(copy.data()).unwrap();
        assert!(decoded.parents.is_empty());
        assert_eq!(decoded.tree, commit.tree);
        assert_eq!(
            octopage_pagestore::rewritten_from(&decoded.message),
            Some(id)
        );
        // Copying a copy names only the latest original.
        let again = rewrite(
            &copy,
            &decoded,
            copy.id(),
            Some(ObjectId::from_array([3; 20])),
        )
        .unwrap();
        let message = String::from_utf8(Commit::decode(again.data()).unwrap().message).unwrap();
        assert_eq!(message.matches(REWRITTEN_FROM).count(), 1);
        assert!(message.starts_with("octopage txn 42\n\n"));
    }
}
