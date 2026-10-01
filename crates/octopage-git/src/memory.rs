use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::object::{Commit, EntryMode, Object, ObjectKind, Tree};
use crate::oid::ObjectId;
use crate::transport::{NewObject, Ref, RefUpdate, Transport};

/// A failure to inject into the next push, for testing the layers above the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The connection drops before the push reaches the server: nothing is applied,
    /// but the caller cannot tell (it sees `PushOutcomeUnknown`).
    DropBeforeApply,
    /// The push is applied, then the response is lost (`PushOutcomeUnknown`).
    DropAfterApply,
    /// GitHub throttles the push; nothing is applied.
    RateLimit,
    /// The push is rejected with a bare "failed", as github.com sometimes words it; nothing is applied.
    Reject,
    /// If the push loses its compare-and-swap, report it as github.com does for some races:
    /// a bare "failed" rejection instead of a conflict.
    StaleAsRejected,
}

#[derive(Default)]
struct State {
    objects: HashMap<ObjectId, Object>,
    refs: BTreeMap<String, ObjectId>,
    faults: VecDeque<Fault>,
    deleted: bool,
}

type Repo = Arc<Mutex<State>>;

/// The repositories of one in-memory host, by name.
#[derive(Default)]
struct Host {
    repos: Mutex<HashMap<String, Repo>>,
}

/// An in-process remote with GitHub's semantics: atomic multi-ref compare-and-swap, and a
/// connectivity check that rejects pushes referring to objects the remote does not have.
///
/// Each one is a repository on an in-memory host, which can hold others: see
/// [`InMemory::create_repository`] and [`Transport::relocate`].
pub struct InMemory {
    state: Repo,
    host: Arc<Host>,
    name: String,
}

impl Default for InMemory {
    fn default() -> Self {
        let host = Arc::new(Host::default());
        let name = "memory".to_string();
        let state = Repo::default();
        host.repos
            .lock()
            .unwrap()
            .insert(name.clone(), state.clone());
        InMemory { state, host, name }
    }
}

impl InMemory {
    /// A new host with one empty repository, `memory`.
    pub fn new() -> Self {
        InMemory::default()
    }

    /// This repository's name on its host.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Create an empty repository `name` on the same host.
    pub fn create_repository(&self, name: &str) -> Result<InMemory> {
        let mut repos = self.host.repos.lock().unwrap();
        if repos.contains_key(name) {
            return Err(Error::Invalid(format!("repository {name} already exists")));
        }
        let state = Repo::default();
        repos.insert(name.to_string(), state.clone());
        Ok(InMemory {
            state,
            host: self.host.clone(),
            name: name.to_string(),
        })
    }

    /// Delete repository `name` from the host; transports to it fail from then on.
    pub fn delete_repository(&self, name: &str) -> Result<()> {
        let repo = self
            .host
            .repos
            .lock()
            .unwrap()
            .remove(name)
            .ok_or_else(|| Error::NotFound(format!("repository {name}")))?;
        let mut state = repo.lock().unwrap();
        state.deleted = true;
        state.objects.clear();
        state.refs.clear();
        Ok(())
    }

    /// The names of the host's repositories.
    pub fn repositories(&self) -> Vec<String> {
        let mut names: Vec<String> = self.host.repos.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    /// Make the next push fail this way (faults queue up in order).
    pub fn inject(&self, fault: Fault) {
        self.state.lock().unwrap().faults.push_back(fault);
    }

    pub fn ref_value(&self, name: &str) -> Option<ObjectId> {
        self.state.lock().unwrap().refs.get(name).copied()
    }

    pub fn contains(&self, id: ObjectId) -> bool {
        self.state.lock().unwrap().objects.contains_key(&id)
    }

    pub fn object_count(&self) -> usize {
        self.state.lock().unwrap().objects.len()
    }

    /// The bytes of every object held, uncompressed: the repository's size, roughly.
    pub fn object_bytes(&self) -> u64 {
        let state = self.state.lock().unwrap();
        state.objects.values().map(|o| o.data().len() as u64).sum()
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        let state = self.state.lock().unwrap();
        if state.deleted {
            return Err(Error::NotFound(format!(
                "{}: repository not found",
                self.name
            )));
        }
        Ok(state)
    }
}

/// Ids an object points at: a commit's tree and parents, a tree's entries (except submodules).
fn references(object: &Object) -> Result<Vec<ObjectId>> {
    Ok(match object.kind() {
        ObjectKind::Commit => {
            let commit = Commit::decode(object.data())?;
            std::iter::once(commit.tree).chain(commit.parents).collect()
        }
        ObjectKind::Tree => Tree::decode(object.data())?
            .entries()
            .iter()
            .filter(|e| e.mode != EntryMode::Other(0o160000))
            .map(|e| e.id)
            .collect(),
        ObjectKind::Blob | ObjectKind::Tag => Vec::new(),
    })
}

impl Transport for InMemory {
    async fn list_refs(&self, prefixes: &[&str]) -> Result<Vec<Ref>> {
        let state = self.state()?;
        Ok(state
            .refs
            .iter()
            .filter(|(name, _)| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p)))
            .map(|(name, id)| Ref {
                name: name.clone(),
                id: *id,
            })
            .collect())
    }

    async fn fetch(&self, ids: &[ObjectId]) -> Result<Vec<Object>> {
        let state = self.state()?;
        let missing: Vec<ObjectId> = ids
            .iter()
            .copied()
            .filter(|id| !state.objects.contains_key(id))
            .collect();
        if !missing.is_empty() {
            return Err(Error::MissingObjects(missing));
        }
        Ok(ids.iter().map(|id| state.objects[id].clone()).collect())
    }

    async fn history(
        &self,
        tip: ObjectId,
        have: Option<ObjectId>,
        with_trees: bool,
    ) -> Result<Vec<Object>> {
        let state = self.state()?;
        let commit = |id: ObjectId| -> Result<(Object, Vec<ObjectId>)> {
            let object = state
                .objects
                .get(&id)
                .filter(|o| o.kind() == ObjectKind::Commit)
                .ok_or(Error::MissingObjects(vec![id]))?;
            Ok((object.clone(), Commit::decode(object.data())?.parents))
        };
        // Every tree under `root` whose id is not in `stop`, pruning unchanged subtrees.
        let trees_under =
            |root: ObjectId, stop: &HashSet<ObjectId>, out: &mut Vec<Object>| -> Result<()> {
                let mut queue = vec![root];
                let mut seen = HashSet::new();
                while let Some(id) = queue.pop() {
                    if stop.contains(&id) || !seen.insert(id) {
                        continue;
                    }
                    let tree = state
                        .objects
                        .get(&id)
                        .ok_or(Error::MissingObjects(vec![id]))?;
                    queue.extend(
                        Tree::decode(tree.data())?
                            .entries()
                            .iter()
                            .filter(|e| e.mode == EntryMode::Tree)
                            .map(|e| e.id),
                    );
                    out.push(tree.clone());
                }
                Ok(())
            };
        let root_of =
            |object: &Object| -> Result<ObjectId> { Ok(Commit::decode(object.data())?.tree) };
        let Some(have) = have else {
            let (tip_commit, _) = commit(tip)?;
            let mut out = Vec::new();
            if with_trees {
                trees_under(root_of(&tip_commit)?, &HashSet::new(), &mut out)?;
            }
            out.push(tip_commit);
            return Ok(out);
        };
        let mut known_trees = HashSet::new();
        if with_trees && let Ok((have_commit, _)) = commit(have) {
            let mut all = Vec::new();
            trees_under(root_of(&have_commit)?, &HashSet::new(), &mut all)?;
            known_trees.extend(all.iter().map(Object::id));
        }
        let mut stop = HashSet::new();
        let mut queue = vec![have];
        while let Some(id) = queue.pop() {
            if stop.insert(id)
                && let Ok((_, parents)) = commit(id)
            {
                queue.extend(parents);
            }
        }
        let (mut out, mut seen, mut queue) = (Vec::new(), HashSet::new(), vec![tip]);
        while let Some(id) = queue.pop() {
            if !stop.contains(&id) && seen.insert(id) {
                let (object, parents) = commit(id)?;
                out.push(object);
                queue.extend(parents);
            }
        }
        if with_trees {
            let roots: Vec<ObjectId> = out.iter().map(root_of).collect::<Result<_>>()?;
            for root in roots {
                let mut trees = Vec::new();
                trees_under(root, &known_trees, &mut trees)?;
                known_trees.extend(trees.iter().map(Object::id));
                out.extend(trees);
            }
        }
        Ok(out)
    }

    async fn push(&self, updates: &[RefUpdate], objects: &[NewObject]) -> Result<()> {
        let mut state = self.state()?;
        let fault = state.faults.pop_front();
        match fault {
            Some(Fault::DropBeforeApply) => {
                return Err(Error::PushOutcomeUnknown(
                    "injected: connection dropped".into(),
                ));
            }
            Some(Fault::RateLimit) => return Err(Error::RateLimited { retry_after: None }),
            Some(Fault::Reject) => {
                return Err(Error::Rejected(format!(
                    "{}: failed",
                    updates.first().map_or("", |u| u.name.as_str())
                )));
            }
            _ => {}
        }
        if updates.is_empty() {
            return Err(Error::Invalid(
                "a push needs at least one ref update".into(),
            ));
        }

        let incoming: HashMap<ObjectId, &Object> =
            objects.iter().map(|o| (o.object.id(), &o.object)).collect();
        let known = |id: &ObjectId| state.objects.contains_key(id) || incoming.contains_key(id);
        for object in incoming.values() {
            if let Some(missing) = references(object)?.into_iter().find(|id| !known(id)) {
                return Err(Error::Rejected(format!(
                    "missing necessary objects: {missing} (referenced by {})",
                    object.id()
                )));
            }
        }
        for u in updates {
            if let Some(new) = u.new.filter(|id| !known(id)) {
                return Err(Error::Rejected(format!(
                    "{} would point at missing object {new}",
                    u.name
                )));
            }
        }
        for u in updates {
            let current = state.refs.get(&u.name).copied();
            if current != u.old {
                let reason = match (current, u.old) {
                    (Some(c), Some(e)) => format!("is at {c} but expected {e}"),
                    (Some(c), None) => format!("reference already exists at {c}"),
                    (None, _) => "reference does not exist".to_string(),
                };
                if fault == Some(Fault::StaleAsRejected) {
                    return Err(Error::Rejected(format!("{}: failed", u.name)));
                }
                return Err(Error::Conflict {
                    refname: u.name.clone(),
                    reason,
                });
            }
        }

        for (id, object) in incoming {
            state.objects.entry(id).or_insert_with(|| object.clone());
        }
        for u in updates {
            match u.new {
                Some(new) => state.refs.insert(u.name.clone(), new),
                None => state.refs.remove(&u.name),
            };
        }
        if fault == Some(Fault::DropAfterApply) {
            return Err(Error::PushOutcomeUnknown(
                "injected: response lost after the push applied".into(),
            ));
        }
        Ok(())
    }

    fn location(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn relocate(&self, location: &str) -> Result<Self> {
        let state = self
            .host
            .repos
            .lock()
            .unwrap()
            .get(location)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("{location}: repository not found")))?;
        Ok(InMemory {
            state,
            host: self.host.clone(),
            name: location.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Signature, TreeEntry};

    fn commit_with_page(data: &[u8], parent: Option<ObjectId>) -> (ObjectId, Vec<NewObject>) {
        let blob = Object::blob(data.to_vec()).unwrap();
        let mut tree = Tree::new();
        tree.insert(TreeEntry::blob("page", blob.id())).unwrap();
        let tree = tree.to_object().unwrap();
        let commit = Commit {
            tree: tree.id(),
            parents: parent.into_iter().collect(),
            author: Signature::new("t", "t@x", 0),
            committer: Signature::new("t", "t@x", 0),
            message: b"t\n".to_vec(),
        }
        .to_object()
        .unwrap();
        (commit.id(), vec![blob.into(), tree.into(), commit.into()])
    }

    #[tokio::test]
    async fn compare_and_swap_and_atomicity() {
        let remote = InMemory::new();
        let (a, objs) = commit_with_page(b"a", None);
        remote
            .push(&[RefUpdate::create("refs/heads/db", a)], &objs)
            .await
            .unwrap();
        let (b, objs_b) = commit_with_page(b"b", Some(a));
        let (c, objs_c) = commit_with_page(b"c", Some(a));
        remote
            .push(&[RefUpdate::update("refs/heads/db", a, b)], &objs_b)
            .await
            .unwrap();
        // c was built on a: stale.
        let err = remote
            .push(
                &[
                    RefUpdate::update("refs/heads/db", a, c),
                    RefUpdate::create("refs/octopage/x", c),
                ],
                &objs_c,
            )
            .await;
        assert!(matches!(err, Err(Error::Conflict { .. })));
        assert_eq!(remote.ref_value("refs/heads/db"), Some(b));
        assert_eq!(
            remote.ref_value("refs/octopage/x"),
            None,
            "atomic: nothing applied"
        );
    }

    #[tokio::test]
    async fn rejects_missing_objects_and_injects_faults() {
        let remote = InMemory::new();
        let (a, objs) = commit_with_page(b"a", None);
        let err = remote
            .push(&[RefUpdate::create("refs/heads/db", a)], &objs[1..])
            .await;
        assert!(matches!(err, Err(Error::Rejected(_))), "blob missing");

        remote.inject(Fault::DropBeforeApply);
        assert!(matches!(
            remote
                .push(&[RefUpdate::create("refs/heads/db", a)], &objs)
                .await,
            Err(Error::PushOutcomeUnknown(_))
        ));
        assert_eq!(remote.ref_value("refs/heads/db"), None);

        remote.inject(Fault::DropAfterApply);
        assert!(matches!(
            remote
                .push(&[RefUpdate::create("refs/heads/db", a)], &objs)
                .await,
            Err(Error::PushOutcomeUnknown(_))
        ));
        assert_eq!(
            remote.ref_value("refs/heads/db"),
            Some(a),
            "applied despite the lost response"
        );

        assert_eq!(remote.fetch(&[a]).await.unwrap()[0].id(), a);
        assert!(matches!(
            remote.fetch(&[ObjectId::ZERO]).await,
            Err(Error::MissingObjects(_))
        ));
    }

    #[tokio::test]
    async fn repositories_on_one_host() {
        let first = InMemory::new();
        let (a, objs) = commit_with_page(b"a", None);
        let second = first.create_repository("second").unwrap();
        second
            .push(&[RefUpdate::create("refs/heads/db", a)], &objs)
            .await
            .unwrap();
        assert!(first.create_repository("second").is_err());
        let again = first.relocate("second").unwrap();
        assert_eq!(again.ref_value("refs/heads/db"), Some(a));
        assert_eq!(first.ref_value("refs/heads/db"), None);
        assert!(matches!(first.relocate("third"), Err(Error::NotFound(_))));
        first.delete_repository("second").unwrap();
        assert!(matches!(
            again.list_refs(&[]).await,
            Err(Error::NotFound(_))
        ));
        assert_eq!(first.repositories(), ["memory"]);
    }

    #[tokio::test]
    async fn history_between_heads() {
        let remote = InMemory::new();
        let (a, objs_a) = commit_with_page(b"a", None);
        let (b, objs_b) = commit_with_page(b"b", Some(a));
        let (c, objs_c) = commit_with_page(b"c", Some(b));
        remote
            .push(&[RefUpdate::create("refs/heads/db", a)], &objs_a)
            .await
            .unwrap();
        remote
            .push(&[RefUpdate::update("refs/heads/db", a, b)], &objs_b)
            .await
            .unwrap();
        remote
            .push(&[RefUpdate::update("refs/heads/db", b, c)], &objs_c)
            .await
            .unwrap();
        let ids = |v: Vec<Object>| v.into_iter().map(|o| o.id()).collect::<HashSet<_>>();
        assert_eq!(
            ids(remote.history(c, Some(a), false).await.unwrap()),
            HashSet::from([b, c])
        );
        assert_eq!(
            ids(remote.history(c, None, false).await.unwrap()),
            HashSet::from([c])
        );
        assert!(remote.history(c, Some(c), false).await.unwrap().is_empty());
        // With trees: each commit here has its own one-entry root tree.
        let with_trees = remote.history(c, Some(a), true).await.unwrap();
        assert_eq!(
            with_trees
                .iter()
                .filter(|o| o.kind() == ObjectKind::Tree)
                .count(),
            2
        );
        assert_eq!(remote.history(c, None, true).await.unwrap().len(), 2);
    }
}
