use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

use bytes::Bytes;
use octopage_git::{
    Commit, EntryMode, NewObject, Object, ObjectId, RefUpdate, Signature, Transport, Tree,
    TreeEntry,
};

use crate::codec::Kind;
use crate::error::{Error, Result, invalid};
use crate::layout::Root;
use crate::page::{MAX_PAGE_ID, Page, PageBuf, PageId, PageType, map_bit, set_map_bit};
use crate::store::{PageStore, Snapshot, SnapshotInfo, now};

pub struct WriteTxn<T: Transport + 'static> {
    store: PageStore<T>,
    base: Snapshot<T>,
    txid: String,
    started: Instant,
    /// `None`: the page is freed.
    writes: BTreeMap<PageId, Option<PageBuf>>,
    reads: BTreeSet<PageId>,
    allocated: BTreeSet<PageId>,
    alloc_cursor: u32,
    catalog: Option<Bytes>,
    read_catalog: bool,
    changelog: Bytes,
    unstaged: usize,
    staged: HashSet<ObjectId>,
    stage_tip: Option<ObjectId>,
    /// Stored forms of this transaction's pages, with the stamped bytes they came from. With
    /// random nonces, sealing a page twice gives two different blobs, so a staged page would
    /// otherwise be sent again with the commit.
    sealed: Mutex<HashMap<PageId, (Bytes, Object)>>,
    /// New maintenance settings, stored as they are.
    settings: Option<Bytes>,
    /// New root entries besides the database's own (files such as workflows), and the objects
    /// they need.
    extra: Option<(Vec<TreeEntry>, Vec<Object>)>,
    /// The store's move count when this transaction began: a transaction that began before the
    /// database moved to a new repository is carried over by comparing page maps.
    moves: u64,
}

/// The result of a commit attempt.
#[allow(clippy::large_enum_variant)] // returned once per commit attempt; boxing only adds noise
pub enum Outcome<T: Transport + 'static> {
    Committed(Committed),
    /// Another writer moved the head first. Nothing was published; `rebase` onto `head`.
    Conflict(Conflict<T>),
}

#[derive(Clone, Debug)]
pub struct Committed {
    /// The new head (or the unchanged head, for a transaction that wrote nothing).
    pub head: ObjectId,
    pub epoch: u64,
    /// Objects sent in the final push.
    pub objects: usize,
}

pub struct Conflict<T: Transport + 'static> {
    pub txn: WriteTxn<T>,
    pub head: ObjectId,
}

#[allow(clippy::large_enum_variant)] // returned once per lost race; boxing only adds noise
pub enum Rebase<T: Transport + 'static> {
    /// Nothing this transaction touched changed: its writes now apply on top of the new head.
    Ready(WriteTxn<T>),
    /// The new head changed pages this transaction read or wrote (or the catalog it used).
    /// Re-run the transaction's logic on `head`.
    Overlap {
        head: ObjectId,
        pages: Vec<PageId>,
        catalog: bool,
    },
}

struct Built {
    commit: Object,
    objects: Vec<NewObject>,
    cache: Vec<(Object, bool)>,
    updates: Vec<RefUpdate>,
    info: SnapshotInfo,
}

enum Published {
    Landed,
    /// Landed in the repository the database then moved out of; the head is the new one's.
    LandedBeforeMove(ObjectId),
    Lost(ObjectId),
}

impl<T: Transport + 'static> WriteTxn<T> {
    pub(crate) fn new(base: Snapshot<T>) -> Self {
        WriteTxn {
            store: base.store.clone(),
            txid: format!("{:016x}", fastrand::u64(..)),
            started: Instant::now(),
            writes: BTreeMap::new(),
            reads: BTreeSet::new(),
            allocated: BTreeSet::new(),
            alloc_cursor: base.superblock().first_data_page(),
            catalog: None,
            read_catalog: false,
            changelog: Bytes::new(),
            unstaged: 0,
            staged: HashSet::new(),
            stage_tip: None,
            sealed: Mutex::new(HashMap::new()),
            settings: None,
            extra: None,
            moves: base.store.moves(),
            base,
        }
    }

    /// The blob for a stamped page, sealed as the database stores pages.
    fn seal(&self, id: PageId, stamped: Bytes) -> Result<Object> {
        let mut sealed = self.sealed.lock().unwrap();
        if let Some((plain, object)) = sealed.get(&id)
            && *plain == stamped
        {
            return Ok(object.clone());
        }
        let object = Object::blob(self.store.seal_page(id, stamped.clone())?)?;
        sealed.insert(id, (stamped, object.clone()));
        Ok(object)
    }

    /// The snapshot this transaction reads from.
    pub fn base(&self) -> &Snapshot<T> {
        &self.base
    }

    pub fn id(&self) -> &str {
        &self.txid
    }

    /// The epoch this transaction's commit will have.
    pub fn epoch(&self) -> u64 {
        self.base.epoch() + 1
    }

    pub fn page_size(&self) -> usize {
        self.base.page_size()
    }

    /// Page ids read so far (the read set used to validate a rebase).
    pub fn reads(&self) -> impl Iterator<Item = PageId> + '_ {
        self.reads.iter().copied()
    }

    /// Page ids written or freed so far.
    pub fn writes(&self) -> impl Iterator<Item = PageId> + '_ {
        self.writes.keys().copied()
    }

    fn stage_ref(&self) -> String {
        format!("refs/octopage/stage/{}", self.txid)
    }

    /// Read a page, seeing this transaction's own writes.
    pub async fn get(&mut self, id: PageId) -> Result<Option<Page>> {
        if let Some(write) = self.writes.get(&id) {
            return match write {
                Some(buf) => Page::parse(
                    buf.stamp(id, self.epoch()),
                    self.page_size(),
                    id,
                    self.epoch(),
                )
                .map(Some),
                None => Ok(None),
            };
        }
        if !self.base.superblock().is_system(id) {
            self.reads.insert(id);
        }
        self.base.get(id).await
    }

    /// Write a page. It must exist in the base snapshot or have been allocated by this transaction.
    pub fn put(&mut self, id: PageId, page: PageBuf) -> Result<()> {
        if self.base.superblock().is_system(id) {
            return Err(invalid(format!("page {id} belongs to the page store")));
        }
        if page.len() != self.page_size() {
            return Err(invalid(format!(
                "page {id} is {} bytes; pages are {}",
                page.len(),
                self.page_size()
            )));
        }
        self.unstaged += page.len();
        self.writes.insert(id, Some(page));
        Ok(())
    }

    /// Free a page: it disappears from the new page map and its id becomes reusable.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        if self.base.superblock().is_system(id) {
            return Err(invalid(format!("page {id} belongs to the page store")));
        }
        if self.allocated.remove(&id) {
            self.writes.remove(&id); // allocated and freed within this transaction: nothing happened
        } else {
            self.writes.insert(id, None);
        }
        Ok(())
    }

    /// A page id no other page uses: the lowest free id below the high-water mark, or the next
    /// new one. The caller must `put` it before committing.
    pub async fn allocate(&mut self) -> Result<PageId> {
        let sb = self.base.superblock();
        let bits = sb.bits_per_map_page();
        while self.alloc_cursor < sb.next_free {
            let (map_id, _) = sb.map_slot(PageId::new(self.alloc_cursor)?);
            let map = self.base.get(map_id).await?;
            let range_end = ((map_id.get() - 1) * bits + bits).min(sb.next_free);
            let mut id = self.alloc_cursor;
            while id < range_end {
                let bit = id % bits;
                let used = match &map {
                    Some(page) => {
                        let byte = page.payload()[(bit / 8) as usize];
                        if byte == 0xff && bit.is_multiple_of(8) {
                            id += 8; // a full byte: skip it
                            continue;
                        }
                        map_bit(page.payload(), bit)
                    }
                    None => false,
                };
                let candidate = PageId::new(id)?;
                if !used
                    && !self.allocated.contains(&candidate)
                    && !self.writes.contains_key(&candidate)
                {
                    self.alloc_cursor = id + 1;
                    self.allocated.insert(candidate);
                    return Ok(candidate);
                }
                id += 1;
            }
            self.alloc_cursor = range_end;
        }
        let next = self
            .allocated
            .last()
            .map_or(sb.next_free, |last| (last.get() + 1).max(sb.next_free));
        if next > MAX_PAGE_ID {
            return Err(Error::Full);
        }
        let id = PageId::new(next)?;
        self.allocated.insert(id);
        Ok(id)
    }

    /// Allocate a specific page id, for a layer that chooses its own page numbers (such as
    /// SQLite, which grows its file at the end). The id must not exist in the base snapshot.
    pub async fn claim(&mut self, id: PageId) -> Result<()> {
        if self.base.superblock().is_system(id) {
            return Err(invalid(format!("page {id} belongs to the page store")));
        }
        if self.allocated.contains(&id) {
            return Ok(());
        }
        if self.writes.contains_key(&id)
            || self
                .store
                .resolve(self.base.info.pages(), id)
                .await?
                .is_some()
        {
            return Err(invalid(format!("page {id} already exists")));
        }
        self.allocated.insert(id);
        Ok(())
    }

    /// Whether page `id` exists as this transaction sees it (without recording a read).
    pub async fn exists(&self, id: PageId) -> Result<bool> {
        if let Some(write) = self.writes.get(&id) {
            return Ok(write.is_some());
        }
        if self.allocated.contains(&id) {
            return Ok(true);
        }
        Ok(self
            .store
            .resolve(self.base.info.pages(), id)
            .await?
            .is_some())
    }

    /// The catalog blob. Reading it makes any concurrent catalog change a conflict.
    pub async fn catalog(&mut self) -> Result<Option<Bytes>> {
        if let Some(catalog) = &self.catalog {
            return Ok(Some(catalog.clone()));
        }
        self.read_catalog = true;
        self.base.catalog().await
    }

    pub fn set_catalog(&mut self, catalog: impl Into<Bytes>) {
        self.catalog = Some(catalog.into());
    }

    /// What this commit did, for the history (`changelog` blob). Replaced, not appended.
    /// Replace the maintenance settings (stored as they are: a maintenance job that holds no
    /// keys must be able to read them).
    pub fn set_settings(&mut self, settings: impl Into<Bytes>) {
        self.settings = Some(settings.into());
    }

    /// Write a file into the commit's tree beside the database, at `path` (`a/b/c`), such as a
    /// workflow under `.github/workflows/`; `None` removes it. Files stay from commit to commit.
    pub async fn put_file(&mut self, path: &str, contents: Option<Bytes>) -> Result<()> {
        let parts: Vec<&str> = path.split('/').collect();
        let reserved = [
            &b"pages"[..],
            b"catalog",
            b"changelog",
            b"keys",
            b"settings",
            b"moved",
        ];
        if parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
            || reserved.contains(&parts[0].as_bytes())
        {
            return Err(invalid(format!(
                "{path:?} is not a file path OctoPage can write"
            )));
        }
        let (mut entries, mut objects) = match self.extra.take() {
            Some(extra) => extra,
            None => (self.base.root().extra.clone(), Vec::new()),
        };
        let blob = match contents {
            Some(bytes) => {
                let blob = Object::blob(bytes)?;
                let id = blob.id();
                objects.push(blob);
                Some(id)
            }
            None => None,
        };
        let mut top = Tree::new();
        for entry in entries.drain(..) {
            top.insert(entry).map_err(|e| invalid(e.to_string()))?;
        }
        let top = self.place_file(top, &parts, blob, &mut objects).await?;
        self.extra = Some((top.entries().to_vec(), objects));
        Ok(())
    }

    /// `tree` with the file at `parts` set (or removed), writing new subtrees into `objects`.
    async fn place_file(
        &self,
        mut tree: Tree,
        parts: &[&str],
        blob: Option<ObjectId>,
        objects: &mut Vec<Object>,
    ) -> Result<Tree> {
        let name = parts[0].as_bytes();
        if parts.len() == 1 {
            match blob {
                Some(id) => tree
                    .insert(TreeEntry::blob(name, id))
                    .map_err(|e| invalid(e.to_string()))?,
                None => {
                    tree.remove(name);
                }
            }
            return Ok(tree);
        }
        let child = match tree.get(name) {
            Some(e) if e.mode == EntryMode::Tree => {
                let id = e.id;
                match objects.iter().find(|o| o.id() == id) {
                    Some(o) => Tree::decode(o.data()).map_err(|e| invalid(e.to_string()))?,
                    None => self.store.tree(id).await?.tree.clone(),
                }
            }
            _ => Tree::new(),
        };
        let child = Box::pin(self.place_file(child, &parts[1..], blob, objects)).await?;
        if child.entries().is_empty() {
            tree.remove(name);
        } else {
            let object = child.to_object()?;
            tree.insert(TreeEntry::tree(name, object.id()))
                .map_err(|e| invalid(e.to_string()))?;
            objects.push(object);
        }
        Ok(tree)
    }

    pub fn set_changelog(&mut self, changelog: impl Into<Bytes>) {
        self.changelog = changelog.into();
    }

    /// Push this transaction's pages under its staging ref if they exceed `stage_bytes` or it has
    /// been open longer than `stage_age`, so the final commit carries only trees and the commit.
    pub async fn checkpoint(&mut self) -> Result<()> {
        let config = self.store.config();
        if self.unstaged == 0
            || (self.unstaged < config.stage_bytes && self.started.elapsed() < config.stage_age)
        {
            return Ok(());
        }
        let epoch = self.epoch();
        let mut tree = Tree::new();
        let mut objects: Vec<NewObject> = Vec::new();
        let mut ids = Vec::new();
        for (id, write) in &self.writes {
            let Some(buf) = write else { continue };
            let blob = self.seal(*id, buf.stamp(*id, epoch))?;
            if self.staged.contains(&blob.id()) || ids.contains(&blob.id()) {
                continue;
            }
            tree.insert(TreeEntry::blob(blob.id().to_hex(), blob.id()))
                .map_err(|e| invalid(e.to_string()))?;
            ids.push(blob.id());
            objects.push(blob.into());
        }
        if objects.is_empty() {
            self.unstaged = 0;
            return Ok(());
        }
        let tree = tree.to_object()?;
        let signature = Signature::new(
            config.committer_name.clone(),
            config.committer_email.clone(),
            now(),
        );
        let commit = Commit {
            tree: tree.id(),
            parents: self.stage_tip.into_iter().collect(),
            author: signature.clone(),
            committer: signature,
            message: format!("octopage stage {}\n", self.txid).into_bytes(),
        }
        .to_object()?;
        objects.push(tree.into());
        objects.push(commit.clone().into());
        let update = match self.stage_tip {
            None => RefUpdate::create(self.stage_ref(), commit.id()),
            Some(tip) => RefUpdate::update(self.stage_ref(), tip, commit.id()),
        };
        match self.store.transport().push(&[update], &objects).await {
            Ok(()) => {}
            Err(octopage_git::Error::PushOutcomeUnknown(_)) => {
                // Only this transaction writes its staging ref, so reading it settles the question.
                let stage = self.stage_ref();
                let now_at = self
                    .store
                    .transport()
                    .list_refs(&[&stage])
                    .await?
                    .into_iter()
                    .find(|r| r.name == stage)
                    .map(|r| r.id);
                if now_at != Some(commit.id()) {
                    return Err(octopage_git::Error::PushOutcomeUnknown(
                        "staging push did not land".into(),
                    )
                    .into());
                }
            }
            Err(e) => return Err(e.into()),
        }
        self.staged.extend(ids);
        self.stage_tip = Some(commit.id());
        self.unstaged = 0;
        Ok(())
    }

    /// Publish this transaction as one commit, moving the head from the base by compare-and-swap.
    ///
    /// A push whose outcome is unknown is settled by reading the head: if it is (or descends
    /// from) this commit, the commit landed; if it is still the base, the identical commit is
    /// sent again; otherwise another writer won.
    pub async fn commit(mut self) -> Result<Outcome<T>> {
        if self.base.root().moved.is_some() {
            // The base is a database's last commit in a repository it moved out of: nothing may
            // be built on it. Follow the database; the layer above rebases onto its head there.
            let head = self.store.refresh().await?;
            return Ok(Outcome::Conflict(Conflict { txn: self, head }));
        }
        if self.writes.is_empty()
            && self.catalog.is_none()
            && self.settings.is_none()
            && self.extra.is_none()
        {
            // Read-only: the snapshot was consistent; there is nothing to publish.
            return Ok(Outcome::Committed(Committed {
                head: self.base.id(),
                epoch: self.base.epoch(),
                objects: 0,
            }));
        }
        // Wait out another writer's lease, or take it after too many lost races.
        self.store.before_commit().await?;
        self.checkpoint().await?;
        let built = self.build().await?;
        let published = match self.publish(&built).await {
            Ok(published) => published,
            Err((error, false)) => return Err(error), // nothing was applied
            Err((error, true)) => {
                return Err(Error::OutcomeUnknown {
                    base: self.base.id(),
                    commit: built.commit.id(),
                    reason: error.to_string(),
                });
            }
        };
        match published {
            Published::LandedBeforeMove(head) => {
                // The commit is in the old repository's history, which the new one copied.
                self.store.after_commit(true).await;
                self.store.observe_head(head);
                Ok(Outcome::Committed(Committed {
                    head: built.commit.id(),
                    epoch: built.info.epoch,
                    objects: built.objects.len(),
                }))
            }
            Published::Landed => {
                let committed = Committed {
                    head: built.commit.id(),
                    epoch: built.info.epoch,
                    objects: built.objects.len(),
                };
                self.store
                    .published(built.info, &built.commit, &built.cache);
                self.store.after_commit(true).await;
                Ok(Outcome::Committed(committed))
            }
            Published::Lost(head) => {
                self.store.after_commit(false).await;
                Ok(Outcome::Conflict(Conflict { txn: self, head }))
            }
        }
    }

    async fn build(&self) -> Result<Built> {
        let base = &self.base.info;
        let epoch = base.epoch + 1;
        for (id, write) in &self.writes {
            if !self.allocated.contains(id)
                && self.store.resolve(base.pages(), *id).await?.is_none()
            {
                return Err(Error::NotAllocated(*id));
            }
            debug_assert!(write.is_some() || !self.allocated.contains(id));
        }
        if let Some(id) = self
            .allocated
            .iter()
            .find(|id| !self.writes.contains_key(id))
        {
            return Err(invalid(format!(
                "page {id} was allocated but never written"
            )));
        }

        // Superblock and free-map: the base's, plus this transaction's allocations and frees.
        let mut sb = base.superblock;
        if let Some(last) = self.allocated.last() {
            sb.next_free = sb.next_free.max(last.get() + 1);
        }
        let mut maps: BTreeMap<PageId, PageBuf> = BTreeMap::new();
        let flips = self.allocated.iter().map(|id| (*id, true)).chain(
            self.writes
                .iter()
                .filter(|(_, w)| w.is_none())
                .map(|(id, _)| (*id, false)),
        );
        for (id, used) in flips {
            let (map_id, bit) = sb.map_slot(id);
            let map = match maps.entry(map_id) {
                std::collections::btree_map::Entry::Occupied(slot) => slot.into_mut(),
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(match self.base.get(map_id).await? {
                        Some(page) => page.to_buf(),
                        None => PageBuf::new(PageType::FreeMap, sb.page_size as usize),
                    })
                }
            };
            set_map_bit(map.payload_mut(), bit, used);
        }

        let mut changes: BTreeMap<PageId, Option<ObjectId>> = BTreeMap::new();
        let mut objects: Vec<NewObject> = Vec::new();
        let mut cache = Vec::new();
        let mut add = |id: PageId, buf: &PageBuf| -> Result<()> {
            let blob = self.seal(id, buf.stamp(id, epoch))?;
            changes.insert(id, Some(blob.id()));
            if !self.staged.contains(&blob.id()) {
                objects.push(blob.clone().into());
            }
            cache.push((blob, buf.page_type().is_hot()));
            Ok(())
        };
        for (id, write) in &self.writes {
            if let Some(buf) = write {
                add(*id, buf)?;
            }
        }
        for (id, buf) in &maps {
            add(*id, buf)?;
        }
        add(PageId::SUPERBLOCK, &sb.encode())?;
        for (id, write) in &self.writes {
            if write.is_none() {
                changes.insert(*id, None);
            }
        }

        let (pages, trees) = self
            .store
            .build_page_map(Some(base.pages()), &changes)
            .await?;
        objects.extend(trees);
        let mut root = Root {
            pages: Some(pages),
            ..base.root.clone()
        };
        let codec = self.store.codec();
        if let Some(settings) = &self.settings {
            let blob = Object::blob(settings.clone())?;
            root.settings = Some(blob.id());
            objects.push(blob.into());
        }
        if let Some((entries, files)) = &self.extra {
            root.extra = entries.clone();
            objects.extend(files.iter().cloned().map(NewObject::from));
        }
        if let Some(catalog) = &self.catalog {
            let blob = Object::blob(codec.seal(Kind::Catalog, catalog)?)?;
            root.catalog = Some(blob.id());
            objects.push(blob.into());
        }
        let changelog = Object::blob(codec.seal(Kind::Changelog, &self.changelog)?)?;
        root.changelog = Some(changelog.id());
        objects.push(changelog.into());
        let root_tree = root.to_tree().to_object()?;
        let base_root = self.store.tree(base.tree).await?;
        objects.push(NewObject::with_delta_base(
            root_tree.clone(),
            base_root.object.clone(),
        ));
        self.store.remember_tree(root_tree.clone())?;

        // Built once per attempt: a resent push carries exactly these bytes, so the commit id
        // is deterministic and the client can recognise its own commit after a lost response.
        let time = now();
        let message = format!("octopage txn {}", self.txid);
        let commit = self
            .store
            .commit_object(root_tree.id(), vec![base.commit], &message, time)?;
        objects.push(commit.clone().into());

        let mut seen = HashSet::new();
        objects.retain(|o| seen.insert(o.object.id()));
        let mut updates = vec![RefUpdate::update(
            &self.store.config().branch,
            base.commit,
            commit.id(),
        )];
        if let Some(tip) = self.stage_tip {
            updates.push(RefUpdate::delete(self.stage_ref(), tip));
        }
        let info = SnapshotInfo {
            commit: commit.id(),
            parents: vec![base.commit],
            tree: root_tree.id(),
            root: root.clone(),
            superblock: sb,
            epoch,
            time,
            message,
        };
        Ok(Built {
            commit,
            objects,
            cache,
            updates,
            info,
        })
    }

    /// Push the built commit. An error comes with whether the commit may have landed anyway:
    /// `false` only when every attempt was answered and none applied.
    async fn publish(&self, built: &Built) -> std::result::Result<Published, (Error, bool)> {
        let ours = built.commit.id();
        let base = self.base.id();
        let transport = self.store.transport();
        let moves = self.store.moves();
        let mut last_rejection = None;
        // Some attempt went out and its outcome was never learned: it may still land.
        let mut unanswered = false;
        // Where the head is now: ours (or built on ours), another writer's, or still the base.
        let settle =
            async |unanswered: bool| -> std::result::Result<Option<Published>, (Error, bool)> {
                let fail = |e: Error| (e, unanswered);
                let head = self.store.refresh().await.map_err(fail)?;
                if self.store.moves() != moves {
                    // The database moved while this commit was in flight. The old repository's
                    // head stopped at a commit that says so: the commit landed if it is behind it.
                    let branch = &self.store.config().branch;
                    let last = transport
                        .list_refs(&[branch])
                        .await
                        .map_err(|e| fail(e.into()))?
                        .into_iter()
                        .find(|r| &r.name == branch)
                        .map(|r| r.id);
                    let landed = match last {
                        Some(last) => self
                            .store
                            .is_descendant_on(&transport, last, ours)
                            .await
                            .map_err(fail)?,
                        None => false,
                    };
                    return Ok(Some(if landed {
                        Published::LandedBeforeMove(head)
                    } else {
                        Published::Lost(head)
                    }));
                }
                if head == base {
                    return Ok(None);
                }
                Ok(Some(
                    if self.store.is_descendant(head, ours).await.map_err(fail)? {
                        Published::Landed
                    } else {
                        Published::Lost(head)
                    },
                ))
            };
        for _ in 0..self.store.config().push_attempts.max(1) {
            match transport.push(&built.updates, &built.objects).await {
                Ok(()) => return Ok(Published::Landed),
                Err(octopage_git::Error::Conflict { .. }) => {
                    // A resend of a push that had in fact landed conflicts with itself. (A head
                    // back at the base counts as a lost race: rebasing onto it changes nothing.)
                    return Ok(settle(unanswered).await?.unwrap_or(Published::Lost(base)));
                }
                Err(octopage_git::Error::PushOutcomeUnknown(reason)) => {
                    tracing::warn!(%reason, commit = %ours, "push outcome unknown; reading the head");
                    unanswered = true;
                    if let Some(published) = settle(true).await? {
                        return Ok(published);
                    }
                    // Not applied (yet): send the identical commit again.
                }
                Err(octopage_git::Error::Rejected(reason)) => {
                    if let Some(blocked) = crate::error::secret_blocked(&reason) {
                        // Push protection: sending the same objects again cannot help.
                        return Err((blocked, unanswered));
                    }
                    // A rejected push applied nothing. github.com words a lost race in ways stock
                    // git does not ("failed", "missing necessary objects"), so ask the head: if it
                    // moved, another writer won; if not, pause briefly and send the commit again.
                    tracing::warn!(%reason, commit = %ours, "push rejected; reading the head");
                    if let Some(published) = settle(unanswered).await? {
                        return Ok(published);
                    }
                    last_rejection = Some(reason);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        200 + fastrand::u64(0..400),
                    ))
                    .await;
                }
                Err(e) => return Err((e.into(), unanswered)),
            }
        }
        let error = match last_rejection {
            Some(reason) => octopage_git::Error::Rejected(reason),
            None => octopage_git::Error::PushOutcomeUnknown(format!(
                "commit {ours} did not land after repeated attempts"
            )),
        };
        Err((error.into(), unanswered))
    }

    /// Move this transaction onto `head` after a lost race, if nothing it touched changed.
    pub async fn rebase(self, head: ObjectId) -> Result<Rebase<T>> {
        if head == self.base.id() {
            return Ok(Rebase::Ready(self));
        }
        // A transaction begun before the database moved to a new repository has a base the new
        // repository does not know by id (its commits were copied under new ids). Comparing the
        // page maps is enough to carry it over: what it read is unchanged or it runs again.
        let moved = self.moves != self.store.moves();
        // Check ancestry before reading anything from a head that may not be ours to trust.
        if !moved && !self.store.is_descendant(head, self.base.id()).await? {
            return Err(Error::HistoryRewritten {
                base: self.base.id(),
                head,
            });
        }
        let onto = self.store.snapshot(head).await?;
        let sb = self.base.superblock();
        let changed = self
            .store
            .changed_pages(&self.base.info, &onto.info)
            .await?;
        let pages: Vec<PageId> = changed
            .into_iter()
            .filter(|id| !sb.is_system(*id))
            .filter(|id| {
                self.reads.contains(id)
                    || self.writes.contains_key(id)
                    || self.allocated.contains(id)
            })
            .collect();
        let catalog_changed = self.base.info.root.catalog != onto.info.root.catalog;
        let catalog = catalog_changed && (self.read_catalog || self.catalog.is_some());
        if !pages.is_empty() || catalog {
            return Ok(Rebase::Overlap {
                head,
                pages,
                catalog,
            });
        }
        let unstaged = self.writes.values().flatten().map(PageBuf::len).sum();
        let (staged, stage_tip) = if moved {
            // Staged blobs are in the old repository.
            (HashSet::new(), None)
        } else {
            (self.staged, self.stage_tip)
        };
        Ok(Rebase::Ready(WriteTxn {
            alloc_cursor: onto.superblock().first_data_page(),
            moves: self.store.moves(),
            base: onto,
            unstaged, // the new epoch changes every page's bytes, so staged copies no longer match
            staged,
            stage_tip,
            ..self
        }))
    }
}

impl<T: Transport + 'static> std::fmt::Debug for WriteTxn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTxn")
            .field("txid", &self.txid)
            .field("base", &self.base.id())
            .field("reads", &self.reads.len())
            .field("writes", &self.writes.len())
            .field("allocated", &self.allocated.len())
            .finish()
    }
}
