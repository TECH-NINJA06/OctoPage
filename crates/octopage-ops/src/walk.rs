use std::collections::{HashMap, HashSet};

use octopage::{Error, ObjectId, Result, Transport};
use octopage_git::{Commit, EntryMode, Object, ObjectKind, Tree};
use octopage_pagestore::Root;

/// Commits read so far, by id.
#[derive(Default)]
pub(crate) struct Commits {
    map: HashMap<ObjectId, (Object, Commit)>,
}

impl Commits {
    pub fn get(&self, id: ObjectId) -> Option<&(Object, Commit)> {
        self.map.get(&id)
    }

    fn absorb(&mut self, objects: Vec<Object>) -> Result<()> {
        for object in objects {
            if object.kind() == ObjectKind::Commit && !self.map.contains_key(&object.id()) {
                let commit = Commit::decode(object.data()).map_err(Error::from)?;
                self.map.insert(object.id(), (object, commit));
            }
        }
        Ok(())
    }

    /// Commit `id`, fetched if needed. Over smart HTTP one fetch brings its ancestors along.
    pub async fn load<T: Transport>(&mut self, t: &T, id: ObjectId) -> Result<&(Object, Commit)> {
        if !self.map.contains_key(&id) {
            let fetched = t.fetch(&[id]).await?;
            self.absorb(fetched)?;
        }
        self.map
            .get(&id)
            .ok_or_else(|| Error::from(octopage_git::Error::MissingObjects(vec![id])))
    }

    /// The first-parent chain from `head`, newest first, stopping before `stop`.
    pub async fn chain<T: Transport>(
        &mut self,
        t: &T,
        head: ObjectId,
        stop: Option<ObjectId>,
    ) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        let mut next = Some(head);
        while let Some(id) = next {
            if Some(id) == stop {
                break;
            }
            let (_, commit) = self.load(t, id).await?;
            next = commit.parents.first().copied();
            out.push(id);
        }
        Ok(out)
    }

    /// Every commit reachable from `head` (all parents), minus those in `have`.
    pub async fn closure<T: Transport>(
        &mut self,
        t: &T,
        head: ObjectId,
        have: &HashSet<ObjectId>,
    ) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = vec![head];
        while let Some(id) = queue.pop() {
            if have.contains(&id) || !seen.insert(id) {
                continue;
            }
            let (_, commit) = self.load(t, id).await?;
            queue.extend(commit.parents.iter().copied());
            out.push(id);
        }
        Ok(out)
    }
}

/// A commit's root tree, decoded.
pub(crate) async fn root_tree<T: Transport>(t: &T, tree: ObjectId) -> Result<Tree> {
    let object = crate::records::object(t, tree).await?;
    Tree::decode(object.data()).map_err(Error::from)
}

/// Whether a root tree is a database's: it has a page map.
pub(crate) fn database_root(tree: &Tree) -> Option<Root> {
    Root::decode(tree).ok()
}

/// The objects reachable from tree `root` that `have` lacks: the trees, and the ids of the
/// blobs (fetched separately, in batches, by whoever needs them). Trees already in `trees`
/// are not fetched again.
pub(crate) async fn tree_closure<T: Transport>(
    t: &T,
    root: ObjectId,
    have: &HashSet<ObjectId>,
    trees: &mut HashMap<ObjectId, Object>,
) -> Result<(Vec<Object>, Vec<ObjectId>)> {
    let mut out = Vec::new();
    let mut blobs = Vec::new();
    let mut seen = HashSet::new();
    let mut frontier = vec![root];
    while !frontier.is_empty() {
        let level: Vec<ObjectId> = std::mem::take(&mut frontier)
            .into_iter()
            .filter(|id| !have.contains(id) && seen.insert(*id))
            .collect();
        let missing: Vec<ObjectId> = level
            .iter()
            .filter(|id| !trees.contains_key(id))
            .copied()
            .collect();
        for chunk in missing.chunks(256) {
            for object in t.fetch(chunk).await? {
                trees.insert(object.id(), object);
            }
        }
        for id in level {
            let object = trees
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::from(octopage_git::Error::MissingObjects(vec![id])))?;
            let tree = Tree::decode(object.data()).map_err(Error::from)?;
            for entry in tree.entries() {
                match entry.mode {
                    EntryMode::Tree => frontier.push(entry.id),
                    EntryMode::Other(0o160000) => {} // a submodule: not ours to copy
                    _ => {
                        if !have.contains(&entry.id) && seen.insert(entry.id) {
                            blobs.push(entry.id);
                        }
                    }
                }
            }
            out.push(object);
        }
    }
    Ok((out, blobs))
}
