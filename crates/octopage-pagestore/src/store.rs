use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use lru::LruCache;
use octopage_git::{
    Commit, NewObject, Object, ObjectId, ObjectKind, RefUpdate, Signature, Transport, Tree,
    TreeEntry,
};
use tokio::sync::watch;

use crate::cache::{CachedTree, DiskCache, MemCache, TreeCache};
use crate::codec::{Codec, Encryption, KeyFile, Kind, RecoveryKey};
use crate::error::{Error, Result, format, invalid};
use crate::fetcher::Fetcher;
use crate::generation::{self, GEN_PREFIX, MAX_MOVES, Moved};
use crate::layout::{self, Root};
use crate::page::{
    CODEC_COMPRESSED, CODEC_ENCRYPTED, Page, PageBuf, PageId, PageType, Superblock, set_map_bit,
};
use crate::txn::WriteTxn;

#[derive(Clone, Debug)]
pub struct Config {
    /// The ref that is this database's head.
    pub branch: String,
    /// On-disk object cache shared by every process on the machine; `None` keeps objects in memory only.
    pub cache_dir: Option<PathBuf>,
    /// Buffer pool size for page blobs.
    pub memory_budget: usize,
    /// Memory for decoded page-map trees.
    pub tree_budget: usize,
    pub disk_budget: u64,
    /// Read coalescing: misses within this window share one fetch...
    pub batch_window: Duration,
    /// ...of at most this many objects.
    pub batch_size: usize,
    /// How often to poll the head ref; `None` disables the watcher (webhooks can call `notify_head`).
    pub head_poll: Option<Duration>,
    /// A transaction stages its pages under `refs/octopage/stage/<txid>` once they exceed this...
    pub stage_bytes: usize,
    /// ...or once it has been open this long.
    pub stage_age: Duration,
    pub committer_name: String,
    pub committer_email: String,
    /// How many times to resend a push whose outcome was unknown and turned out not to have landed.
    pub push_attempts: u32,
    /// Cooperate through the advisory writer lease (`refs/octopage/lease`): wait while another
    /// client holds it, and take it after this many lost races in a row, so a starving writer
    /// gets its turn. `None` ignores the lease altogether.
    pub lease_after_losses: Option<u32>,
    /// How long a lease lasts unless its holder releases it first.
    pub lease_ttl: Duration,
    /// How often a writer waiting for another client's lease reads the refs.
    pub lease_poll: Duration,
    /// Compress pages (zstd) in a new unencrypted database. Git compresses objects too, so this
    /// saves less than it seems; encrypted databases always compress, before encrypting.
    pub compression: bool,
    /// How to open an encrypted database.
    pub unlock: Option<crate::codec::Unlock>,
    /// Sign each commit (SSH or GPG), so GitHub can show it as verified.
    pub signer: Option<Arc<dyn CommitSigner>>,
}

/// Signs commits: returns an armored signature over the commit's bytes, as
/// `ssh-keygen -Y sign -n git` or `gpg --detach-sign --armor` would.
pub trait CommitSigner: Send + Sync + std::fmt::Debug {
    fn sign(&self, commit: &[u8]) -> std::result::Result<String, String>;
}

impl Default for Config {
    fn default() -> Self {
        Config {
            branch: "refs/heads/main".into(),
            cache_dir: None,
            memory_budget: 256 << 20,
            tree_budget: 64 << 20,
            disk_budget: 2 << 30,
            batch_window: Duration::from_millis(20),
            batch_size: 64,
            head_poll: Some(Duration::from_secs(30)),
            stage_bytes: 16 << 20,
            stage_age: Duration::from_secs(600),
            committer_name: "octopage".into(),
            committer_email: "octopage@example.invalid".into(),
            push_attempts: 4,
            lease_after_losses: Some(3),
            lease_ttl: Duration::from_secs(10),
            lease_poll: Duration::from_millis(250),
            compression: false,
            unlock: None,
            signer: None,
        }
    }
}

/// A database: one branch of one repository. Cheap to clone; clones share caches.
pub struct PageStore<T: Transport + 'static> {
    pub(crate) inner: Arc<Inner<T>>,
}

impl<T: Transport + 'static> Clone for PageStore<T> {
    fn clone(&self) -> Self {
        PageStore {
            inner: self.inner.clone(),
        }
    }
}

/// The transport, replaced when the database moves to another repository.
pub(crate) struct Current<T>(RwLock<Arc<T>>);

impl<T> Current<T> {
    pub(crate) fn new(transport: Arc<T>) -> Self {
        Current(RwLock::new(transport))
    }

    pub(crate) fn get(&self) -> Arc<T> {
        self.0.read().unwrap().clone()
    }

    fn set(&self, transport: Arc<T>) {
        *self.0.write().unwrap() = transport;
    }
}

pub(crate) struct Inner<T> {
    transport: Arc<Current<T>>,
    /// How many times this store followed the database to another repository.
    moves: AtomicU64,
    /// One move at a time.
    moving: tokio::sync::Mutex<()>,
    /// Commits copied into a new generation, and the commits they were copied from.
    origins: Mutex<HashMap<ObjectId, ObjectId>>,
    pub(crate) config: Config,
    fetcher: Fetcher,
    mem: Mutex<MemCache>,
    trees: Mutex<TreeCache>,
    disk: Option<DiskCache>,
    snapshots: Mutex<LruCache<ObjectId, Arc<SnapshotInfo>>>,
    parents: Mutex<HashMap<ObjectId, Vec<ObjectId>>>,
    last_loaded: Mutex<Option<ObjectId>>,
    head: watch::Sender<ObjectId>,
    pub(crate) lease: crate::lease::LeaseState,
    /// How blobs are packed; set once, when the database is created or opened.
    codec: std::sync::OnceLock<Codec>,
    /// The page size, from the superblock; set once, when the database is created or opened.
    page_size: std::sync::OnceLock<usize>,
}

/// What a commit says about the database, loaded once and shared.
#[derive(Debug)]
pub(crate) struct SnapshotInfo {
    pub commit: ObjectId,
    pub parents: Vec<ObjectId>,
    pub tree: ObjectId,
    pub root: Root,
    pub superblock: Superblock,
    pub epoch: u64,
    pub time: i64,
    pub message: String,
}

impl SnapshotInfo {
    pub(crate) fn pages(&self) -> ObjectId {
        self.root.pages.expect("checked when loaded")
    }
}

/// One commit in the database's history, without its changelog.
#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub commit: ObjectId,
    pub parent: Option<ObjectId>,
    /// Commit time, in seconds since the Unix epoch.
    pub time: i64,
    pub message: String,
    /// For a commit copied into a new generation: the commit it was copied from.
    pub rewritten_from: Option<ObjectId>,
}

/// One commit in the database's history.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub commit: ObjectId,
    pub parent: Option<ObjectId>,
    pub time: i64,
    pub message: String,
    /// For a commit copied into a new generation: the commit it was copied from.
    pub rewritten_from: Option<ObjectId>,
    /// The statements this commit ran, as the layer above recorded them.
    pub changelog: Bytes,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn git_format(e: octopage_git::Error) -> Error {
    format(e.to_string())
}

impl<T: Transport + 'static> PageStore<T> {
    /// Create a new database on `config.branch`: a superblock and a free-map, in one commit.
    /// Pages are compressed if `config.compression` says so.
    pub async fn create(transport: T, config: Config, page_size: usize) -> Result<Self> {
        Ok(Self::create_with(transport, config, page_size, None)
            .await?
            .0)
    }

    /// Create a new encrypted database. Returns its recovery key, which exists only here:
    /// whoever creates the database must store it, for when the passphrase or key provider is
    /// lost.
    pub async fn create_encrypted(
        transport: T,
        config: Config,
        page_size: usize,
        encryption: Encryption,
    ) -> Result<(Self, RecoveryKey)> {
        let (store, recovery) =
            Self::create_with(transport, config, page_size, Some(encryption)).await?;
        Ok((
            store,
            recovery.expect("an encrypted database has a recovery key"),
        ))
    }

    async fn create_with(
        transport: T,
        config: Config,
        page_size: usize,
        encryption: Option<Encryption>,
    ) -> Result<(Self, Option<RecoveryKey>)> {
        let (codec, keys, recovery) = match &encryption {
            None => (Codec::plain(config.compression), None, None),
            Some(encryption) => {
                let (file, codec, recovery) = KeyFile::create(encryption)?;
                (codec, Some(Bytes::from(file.to_bytes())), Some(recovery))
            }
        };
        let store = Self::create_from(transport, config, page_size, codec, keys).await?;
        Ok((store, recovery))
    }

    /// Create a database like `model`, with its encryption and keys (so the same passphrase
    /// and recovery key open it), on `config.branch`, with pages of `page_size` bytes: where a
    /// database moves to other pages.
    pub async fn create_like<U: Transport + 'static>(
        transport: T,
        config: Config,
        page_size: usize,
        model: &PageStore<U>,
    ) -> Result<Self> {
        let keys = match model.latest().await?.root().keys {
            Some(id) => Some(model.object(id).await?.data().clone()),
            None => None,
        };
        Self::create_from(transport, config, page_size, model.codec().clone(), keys).await
    }

    async fn create_from(
        transport: T,
        config: Config,
        page_size: usize,
        codec: Codec,
        keys: Option<Bytes>,
    ) -> Result<Self> {
        // A database created in a repository that moved on would be out of every client's reach.
        if let Some(pointer) = transport.list_refs(&[GEN_PREFIX]).await?.first() {
            return Err(invalid(format!(
                "this repository's databases moved to a new generation ({}): create it there",
                pointer.name
            )));
        }
        let mut sb = Superblock::new(page_size)?;
        if codec.compresses() {
            sb.codec |= CODEC_COMPRESSED;
        }
        if codec.is_encrypted() {
            sb.codec |= CODEC_ENCRYPTED;
        }
        let branch = config.branch.clone();
        let existing = transport
            .list_refs(&[&branch])
            .await?
            .into_iter()
            .find(|reference| reference.name == branch);
        let (parent, extras) = if let Some(reference) = existing {
            let commit_object = transport
                .fetch(&[reference.id])
                .await?
                .into_iter()
                .find(|object| object.id() == reference.id)
                .ok_or_else(|| Error::Invalid(format!("missing commit {}", reference.id)))?;
            let commit = Commit::decode(commit_object.data())?;
            let tree_object = transport
                .fetch(&[commit.tree])
                .await?
                .into_iter()
                .find(|object| object.id() == commit.tree)
                .ok_or_else(|| Error::Invalid(format!("missing tree {}", commit.tree)))?;
            let tree = Tree::decode(tree_object.data())?;
            (Some(reference.id), tree.entries().to_vec())
        } else {
            (None, Vec::new())
        };
        let head = parent.unwrap_or(ObjectId::ZERO);
        let store = PageStore::start(transport, config, head)?;
        store.inner.codec.set(codec).expect("a new store");
        store.inner.page_size.set(page_size).expect("a new store");
        let mut map = PageBuf::new(PageType::FreeMap, page_size);
        for id in 0..=sb.map_pages {
            set_map_bit(map.payload_mut(), id, true); // the system pages themselves
        }
        let mut changes = BTreeMap::new();
        let mut objects: Vec<NewObject> = Vec::new();
        for (id, page) in [(PageId::SUPERBLOCK, sb.encode()), (PageId::new(1)?, map)] {
            let blob = Object::blob(store.seal_page(id, page.stamp(id, 1))?)?;
            changes.insert(id, Some(blob.id()));
            store.remember(&blob, true);
            objects.push(blob.into());
        }
        let (pages, trees) = store.build_page_map(None, &changes).await?;
        objects.extend(trees);
        let changelog = Object::blob(store.codec().seal(Kind::Changelog, b"")?)?;
        let mut root = Root {
            pages: Some(pages),
            changelog: Some(changelog.id()),
            extra: extras,
            ..Root::default()
        };
        objects.push(changelog.into());
        if let Some(keys) = &keys {
            let blob = Object::blob(keys.clone())?;
            root.keys = Some(blob.id());
            store.remember(&blob, true);
            objects.push(blob.into());
        }
        let root_tree = store.remember_tree(root.to_tree().to_object()?)?;
        objects.push(root_tree.object.clone().into());
        let commit = store.commit_object(
            root_tree.object.id(),
            Vec::new(),
            "octopage: create database",
            now(),
        )?;
        objects.push(commit.clone().into());

        let branch = store.inner.config.branch.clone();
        match store
            .transport()
            .push(
                &[match parent {
                    Some(parent) => RefUpdate::update(&branch, parent, commit.id()),
                    None => RefUpdate::create(&branch, commit.id()),
                }],
                &objects,
            )
            .await
        {
            Ok(()) => {}
            Err(octopage_git::Error::Conflict { .. }) => return Err(Error::AlreadyExists(branch)),
            Err(e) => return Err(e.into()),
        }
        store.absorb(commit.clone())?;
        store.observe_head(commit.id());
        store.load(commit.id()).await?;
        Ok(store)
    }

    /// Open the database on `config.branch`. An encrypted one needs `config.unlock`.
    pub async fn open(transport: T, config: Config) -> Result<Self> {
        Self::open_shared(Arc::new(transport), config).await
    }

    /// Open a database over a transport another store already uses (see
    /// [`PageStore::shared_transport`]): another branch of the same repository.
    pub async fn open_shared(transport: Arc<T>, config: Config) -> Result<Self> {
        Self::open_with(transport, config, false).await
    }

    /// Open the database without unlocking it, even if it is encrypted: for work on its
    /// structure (history, the page map, blobs) by a maintenance job that holds no keys.
    /// Reading an encrypted page, catalog or changelog then fails with [`Error::Locked`], and
    /// so does committing.
    pub async fn open_locked(transport: Arc<T>, config: Config) -> Result<Self> {
        Self::open_with(transport, config, true).await
    }

    async fn open_with(transport: Arc<T>, config: Config, locked: bool) -> Result<Self> {
        let (transport, head) = locate(transport, &config.branch).await?;
        let store = PageStore::start_shared(transport, config, head)?;
        let info = store.load(head).await?;
        // The superblock is stored as it is; it says how to read everything else.
        let codec = if info.superblock.codec & CODEC_ENCRYPTED == 0 {
            Codec::plain(info.superblock.codec & CODEC_COMPRESSED != 0)
        } else if locked {
            Codec::locked()
        } else {
            let keys = info
                .root
                .keys
                .ok_or_else(|| format("an encrypted database without a keys blob"))?;
            let file = KeyFile::parse(store.object(keys).await?.data())?;
            let unlock = store.inner.config.unlock.as_ref().ok_or(Error::Locked)?;
            file.unlock(unlock)?
        };
        store.inner.codec.set(codec).expect("a new store");
        store
            .inner
            .page_size
            .set(info.superblock.page_size as usize)
            .expect("a new store");
        Ok(store)
    }

    /// The database's page size in bytes (it never changes; a database migrated to another
    /// page size is a new generation, which clients must open again).
    pub fn page_size(&self) -> usize {
        *self
            .inner
            .page_size
            .get()
            .expect("the page size is set when the database is created or opened")
    }

    /// How blobs are packed.
    pub(crate) fn codec(&self) -> &Codec {
        self.inner
            .codec
            .get()
            .expect("the codec is set when the database is created or opened")
    }

    /// Whether this database's pages, catalog and changelogs are encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.codec().is_encrypted()
    }

    /// Whether this database's pages are compressed.
    pub fn is_compressed(&self) -> bool {
        self.codec().compresses()
    }

    /// Whether this store was opened without the keys of an encrypted database
    /// ([`PageStore::open_locked`]).
    pub fn is_locked(&self) -> bool {
        self.codec().is_locked()
    }

    /// The stored form of a stamped page: the superblock as it is, every other page packed.
    pub(crate) fn seal_page(&self, id: PageId, stamped: Bytes) -> Result<Bytes> {
        if id == PageId::SUPERBLOCK {
            return Ok(stamped);
        }
        self.codec().seal(Kind::Page, &stamped)
    }

    fn start(transport: T, config: Config, head: ObjectId) -> Result<Self> {
        Self::start_shared(Arc::new(transport), config, head)
    }

    fn start_shared(transport: Arc<T>, config: Config, head: ObjectId) -> Result<Self> {
        let disk = match &config.cache_dir {
            Some(dir) => Some(
                DiskCache::open(dir, config.disk_budget)
                    .map_err(|e| invalid(format!("cache directory {}: {e}", dir.display())))?,
            ),
            None => None,
        };
        let transport = Arc::new(Current::new(transport));
        let inner = Arc::new(Inner {
            fetcher: Fetcher::spawn(transport.clone(), config.batch_window, config.batch_size),
            moves: AtomicU64::new(0),
            moving: tokio::sync::Mutex::new(()),
            origins: Mutex::new(HashMap::new()),
            mem: Mutex::new(MemCache::new(config.memory_budget)),
            trees: Mutex::new(TreeCache::new(config.tree_budget)),
            disk,
            snapshots: Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())),
            parents: Mutex::new(HashMap::new()),
            last_loaded: Mutex::new(None),
            head: watch::channel(head).0,
            lease: crate::lease::LeaseState::default(),
            codec: std::sync::OnceLock::new(),
            page_size: std::sync::OnceLock::new(),
            transport,
            config,
        });
        if let Some(every) = inner.config.head_poll {
            spawn_watcher(Arc::downgrade(&inner), every);
        }
        Ok(PageStore { inner })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The transport to the repository that holds the database now.
    pub fn transport(&self) -> Arc<T> {
        self.inner.transport.get()
    }

    /// The transport, shareable: for opening another branch of the same repository.
    pub fn shared_transport(&self) -> Arc<T> {
        self.transport()
    }

    /// How many times this store followed the database to a new repository.
    pub fn moves(&self) -> u64 {
        self.inner.moves.load(Ordering::SeqCst)
    }

    /// Where the database lives now (see `Transport::location`).
    pub fn location(&self) -> Option<String> {
        self.transport().location()
    }

    /// The newest head this client knows of, from its own commits, the watcher or `refresh`.
    /// Costs nothing: beginning a transaction never contacts GitHub.
    pub fn head(&self) -> ObjectId {
        *self.inner.head.borrow()
    }

    /// Changes of the known head, for caches and replicas.
    pub fn subscribe(&self) -> watch::Receiver<ObjectId> {
        self.inner.head.subscribe()
    }

    /// Tell the store the head moved (for example from a push webhook).
    pub fn notify_head(&self, head: ObjectId) {
        self.observe_head(head);
    }

    /// Read the head ref now (and, when cooperating, the writer lease, in the same request).
    /// If the database moved to a new repository, follow it there first.
    pub async fn refresh(&self) -> Result<ObjectId> {
        let branch = &self.inner.config.branch;
        let cooperating = self.inner.config.lease_after_losses.is_some();
        let mut prefixes = vec![branch.as_str(), GEN_PREFIX];
        if cooperating {
            prefixes.push(crate::lease::LEASE_REF);
        }
        for _ in 0..MAX_MOVES {
            let transport = self.transport();
            let refs = transport.list_refs(&prefixes).await?;
            if let Some(pointer) = newest_pointer(&refs) {
                self.follow_pointer(&transport, pointer).await?;
                continue;
            }
            let head = refs
                .iter()
                .find(|r| &r.name == branch)
                .map(|r| r.id)
                .ok_or_else(|| Error::NoDatabase(branch.clone()))?;
            self.observe_head(head);
            if cooperating {
                let lease = refs
                    .iter()
                    .find(|r| r.name == crate::lease::LEASE_REF)
                    .map(|r| r.id);
                self.learn_lease(lease).await?;
            }
            return Ok(head);
        }
        Err(format(
            "the database's generation pointers go round in a loop",
        ))
    }

    /// Follow a generation pointer read through `from` (unless another task already did).
    async fn follow_pointer(&self, from: &Arc<T>, pointer: ObjectId) -> Result<()> {
        let moved = read_pointer(from, pointer).await?;
        self.follow(from, &moved).await
    }

    /// Switch to the repository the database moved to, if this store still reads `from`.
    pub(crate) async fn follow(&self, from: &Arc<T>, moved: &Moved) -> Result<()> {
        let _turn = self.inner.moving.lock().await;
        if !Arc::ptr_eq(&self.transport(), from) {
            return Ok(()); // another task followed it already
        }
        let next = Arc::new((**from).relocate(&moved.location)?);
        let branch = &self.inner.config.branch;
        let head = next
            .list_refs(&[branch])
            .await?
            .into_iter()
            .find(|r| &r.name == branch)
            .map(|r| r.id)
            .ok_or_else(|| Error::NoDatabase(format!("{branch} in {}", moved.location)))?;
        tracing::info!(
            location = %moved.location,
            generation = moved.generation,
            "the database moved to a new repository; following it"
        );
        self.inner.transport.set(next);
        // The new repository knows none of the old one's commits, so never offer one as a base.
        *self.inner.last_loaded.lock().unwrap() = None;
        self.inner.lease.forget();
        self.inner.moves.fetch_add(1, Ordering::SeqCst);
        self.observe_head(head);
        Ok(())
    }

    pub(crate) fn observe_head(&self, head: ObjectId) {
        // Subscribers hear only of real changes: most polls read the same head.
        self.inner
            .head
            .send_if_modified(|known| std::mem::replace(known, head) != head);
        if let Some(disk) = &self.inner.disk {
            disk.set_head(&self.inner.config.branch, head);
        }
    }

    /// The database as of `commit` (time travel: any retained commit works).
    pub async fn snapshot(&self, commit: ObjectId) -> Result<Snapshot<T>> {
        Ok(Snapshot {
            store: self.clone(),
            info: self.load(commit).await?,
        })
    }

    /// The database at the known head.
    pub async fn latest(&self) -> Result<Snapshot<T>> {
        self.snapshot(self.head()).await
    }

    /// Page ids that differ between two snapshots (added, changed or removed), found by walking
    /// both page maps together and skipping every unchanged subtree.
    pub async fn diff(&self, a: &Snapshot<T>, b: &Snapshot<T>) -> Result<BTreeSet<PageId>> {
        self.changed_pages(&a.info, &b.info).await
    }

    /// After [`Error::OutcomeUnknown`]: whether `commit` landed (`Some(true)`), can no longer
    /// land because the head moved elsewhere (`Some(false)`), or is still undecided (`None`: the
    /// head is still `base`, and a push in flight could yet move it).
    pub async fn settle(&self, base: ObjectId, commit: ObjectId) -> Result<Option<bool>> {
        let head = self.refresh().await?;
        if head == base {
            return Ok(None);
        }
        Ok(Some(self.is_descendant(head, commit).await?))
    }

    /// Start a transaction on the known head.
    pub async fn begin(&self) -> Result<WriteTxn<T>> {
        Ok(WriteTxn::new(self.latest().await?))
    }

    /// Start a transaction on a specific commit.
    pub async fn begin_at(&self, commit: ObjectId) -> Result<WriteTxn<T>> {
        Ok(WriteTxn::new(self.snapshot(commit).await?))
    }

    /// The most recent `limit` commits from the known head, newest first, following first
    /// parents: one round trip over smart HTTP (a fetched commit brings its ancestors).
    async fn commits(&self, limit: usize) -> Result<Vec<(ObjectId, Commit)>> {
        let mut entries = Vec::new();
        let mut fetched: HashMap<ObjectId, Object> = HashMap::new();
        let mut next = Some(self.head());
        while let (Some(id), true) = (next, entries.len() < limit) {
            let object = match fetched.remove(&id).or_else(|| self.cached(id)) {
                Some(object) => object,
                None => {
                    for o in self.transport().fetch(&[id]).await? {
                        fetched.insert(o.id(), o);
                    }
                    fetched
                        .remove(&id)
                        .ok_or(octopage_git::Error::MissingObjects(vec![id]))?
                }
            };
            let commit = Commit::decode(object.data()).map_err(git_format)?;
            self.record_commit(id, &commit);
            next = commit.parents.first().copied();
            entries.push((id, commit));
        }
        Ok(entries)
    }

    /// The most recent `limit` commits, newest first, from the known head, without their
    /// changelogs (for finding a commit by time or id prefix).
    pub async fn history(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        Ok(self
            .commits(limit)
            .await?
            .into_iter()
            .map(|(id, c)| CommitInfo {
                commit: id,
                parent: c.parents.first().copied(),
                time: c.committer.seconds,
                message: String::from_utf8_lossy(&c.message).trim_end().to_string(),
                rewritten_from: generation::rewritten_from(&c.message),
            })
            .collect())
    }

    /// The most recent `limit` commits, newest first, from the known head.
    pub async fn log(&self, limit: usize) -> Result<Vec<LogEntry>> {
        let entries: Vec<(ObjectId, Option<ObjectId>, Commit)> = self
            .commits(limit)
            .await?
            .into_iter()
            .map(|(id, c)| (id, c.parents.first().copied(), c))
            .collect();
        let roots: Vec<ObjectId> = entries.iter().map(|(_, _, c)| c.tree).collect();
        // Root trees not cached yet come in coalesced batches rather than one round trip each.
        let missing: Vec<ObjectId> = roots
            .iter()
            .filter(|id| self.inner.trees.lock().unwrap().get(id).is_none())
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if !missing.is_empty() {
            for object in self.inner.fetcher.get_many(&missing).await? {
                self.remember_tree(object)?;
            }
        }
        let mut changelogs = Vec::with_capacity(roots.len());
        for root in &roots {
            changelogs.push(Root::decode(&self.tree(*root).await?.tree)?.changelog);
        }
        let blobs: Vec<ObjectId> = changelogs.iter().flatten().copied().collect();
        let fetched: HashMap<ObjectId, Object> = self
            .objects(&blobs)
            .await?
            .into_iter()
            .map(|o| (o.id(), o))
            .collect();
        entries
            .into_iter()
            .zip(changelogs)
            .map(|((commit, parent, c), log)| {
                let changelog = match log.and_then(|id| fetched.get(&id)) {
                    Some(object) => self.codec().open(Kind::Changelog, object.data())?,
                    None => Bytes::new(),
                };
                Ok(LogEntry {
                    commit,
                    parent,
                    time: c.committer.seconds,
                    message: String::from_utf8_lossy(&c.message).trim_end().to_string(),
                    rewritten_from: generation::rewritten_from(&c.message),
                    changelog,
                })
            })
            .collect()
    }

    // ------------------------------------------------------------------ objects and caches

    pub(crate) fn remember(&self, object: &Object, hot: bool) {
        self.inner.mem.lock().unwrap().insert(object.clone(), hot);
        if let Some(disk) = &self.inner.disk {
            disk.put(object);
        }
    }

    fn cached(&self, id: ObjectId) -> Option<Object> {
        let hit = self.inner.mem.lock().unwrap().get(&id);
        if hit.is_some() {
            return hit;
        }
        let object = self.inner.disk.as_ref()?.get(id)?;
        self.inner.mem.lock().unwrap().insert(object.clone(), false);
        Some(object)
    }

    pub(crate) async fn object(&self, id: ObjectId) -> Result<Object> {
        if let Some(object) = self.cached(id) {
            return Ok(object);
        }
        let object = self.inner.fetcher.get(id).await?;
        self.remember(&object, false);
        Ok(object)
    }

    /// Several objects, with every miss in one coalesced fetch.
    pub(crate) async fn objects(&self, ids: &[ObjectId]) -> Result<Vec<Object>> {
        let mut out: Vec<Option<Object>> = ids.iter().map(|id| self.cached(*id)).collect();
        let missing: Vec<ObjectId> = ids
            .iter()
            .zip(&out)
            .filter(|(_, o)| o.is_none())
            .map(|(id, _)| *id)
            .collect();
        if !missing.is_empty() {
            let fetched: HashMap<ObjectId, Object> = self
                .inner
                .fetcher
                .get_many(&missing)
                .await?
                .into_iter()
                .map(|o| (o.id(), o))
                .collect();
            for (slot, id) in out.iter_mut().zip(ids) {
                if slot.is_none() {
                    let object = fetched[id].clone();
                    self.remember(&object, false);
                    *slot = Some(object);
                }
            }
        }
        Ok(out.into_iter().map(|o| o.unwrap()).collect())
    }

    pub(crate) fn remember_tree(&self, object: Object) -> Result<Arc<CachedTree>> {
        if object.kind() != ObjectKind::Tree {
            return Err(format(format!(
                "expected a tree, got a {} ({})",
                object.kind().name(),
                object.id()
            )));
        }
        let tree = Tree::decode(object.data()).map_err(git_format)?;
        if let Some(disk) = &self.inner.disk {
            disk.put(&object);
        }
        let cached = Arc::new(CachedTree { object, tree });
        self.inner.trees.lock().unwrap().insert(cached.clone());
        Ok(cached)
    }

    pub(crate) async fn tree(&self, id: ObjectId) -> Result<Arc<CachedTree>> {
        let hit = self.inner.trees.lock().unwrap().get(&id);
        if let Some(tree) = hit {
            return Ok(tree);
        }
        let object = match self.inner.disk.as_ref().and_then(|d| d.get(id)) {
            Some(object) => object,
            None => self.inner.fetcher.get(id).await?,
        };
        self.remember_tree(object)
    }

    fn record_parents(&self, id: ObjectId, parents: Vec<ObjectId>) {
        self.inner.parents.lock().unwrap().insert(id, parents);
    }

    /// Record a commit's parents, and where it was copied from if it was.
    fn record_commit(&self, id: ObjectId, commit: &Commit) {
        self.record_parents(id, commit.parents.clone());
        if let Some(origin) = generation::rewritten_from(&commit.message) {
            self.inner.origins.lock().unwrap().insert(id, origin);
        }
    }

    /// File a fetched object where it belongs.
    fn absorb(&self, object: Object) -> Result<()> {
        match object.kind() {
            ObjectKind::Tree => {
                self.remember_tree(object)?;
            }
            ObjectKind::Commit => {
                let commit = Commit::decode(object.data()).map_err(git_format)?;
                self.record_commit(object.id(), &commit);
                self.remember(&object, true);
            }
            _ => self.remember(&object, false),
        }
        Ok(())
    }

    pub(crate) fn commit_object(
        &self,
        tree: ObjectId,
        parents: Vec<ObjectId>,
        message: &str,
        time: i64,
    ) -> Result<Object> {
        let config = &self.inner.config;
        let signature = Signature::new(
            config.committer_name.clone(),
            config.committer_email.clone(),
            time,
        );
        let commit = Commit {
            tree,
            parents,
            author: signature.clone(),
            committer: signature,
            message: format!("{message}\n").into_bytes(),
        };
        match &config.signer {
            None => Ok(commit.to_object()?),
            Some(signer) => {
                let signature = signer
                    .sign(&commit.encode()?)
                    .map_err(|e| invalid(format!("signing the commit failed: {e}")))?;
                Ok(commit.to_signed_object(&signature)?)
            }
        }
    }

    // ------------------------------------------------------------------ snapshots and the page map

    fn known_head(&self) -> Option<ObjectId> {
        let loaded = *self.inner.last_loaded.lock().unwrap();
        loaded.or_else(|| self.inner.disk.as_ref()?.head(&self.inner.config.branch))
    }

    pub(crate) async fn load(&self, commit: ObjectId) -> Result<Arc<SnapshotInfo>> {
        let hit = self.inner.snapshots.lock().unwrap().get(&commit).cloned();
        if let Some(info) = hit {
            return Ok(info);
        }
        if self.cached(commit).is_none() {
            // One round trip: the commit, plus every page-map tree it adds over a head we know.
            let have = self.known_head().filter(|h| *h != commit && !h.is_zero());
            for object in self.transport().history(commit, have, true).await? {
                self.absorb(object)?;
            }
        }
        let object = self.object(commit).await?;
        if object.kind() != ObjectKind::Commit {
            return Err(format(format!(
                "{commit} is a {}, not a commit",
                object.kind().name()
            )));
        }
        let parsed = Commit::decode(object.data()).map_err(git_format)?;
        self.record_commit(commit, &parsed);
        let root = Root::decode(&self.tree(parsed.tree).await?.tree)?;
        let pages = root.pages.expect("Root::decode checks it");
        let blob = self
            .resolve(pages, PageId::SUPERBLOCK)
            .await?
            .ok_or_else(|| format("page 0 (the superblock) is missing"))?;
        let bytes = self.object(blob).await?.data().clone();
        let page = Page::parse(bytes.clone(), bytes.len(), PageId::SUPERBLOCK, u64::MAX)?;
        let superblock = Superblock::decode(&page)?;
        let info = Arc::new(SnapshotInfo {
            commit,
            parents: parsed.parents,
            tree: parsed.tree,
            root,
            superblock,
            epoch: page.epoch(),
            time: parsed.committer.seconds,
            message: String::from_utf8_lossy(&parsed.message)
                .trim_end()
                .to_string(),
        });
        self.inner
            .snapshots
            .lock()
            .unwrap()
            .put(commit, info.clone());
        *self.inner.last_loaded.lock().unwrap() = Some(commit);
        Ok(info)
    }

    /// The blob holding `page` under the page map `pages`, if the page exists.
    pub(crate) async fn resolve(&self, pages: ObjectId, page: PageId) -> Result<Option<ObjectId>> {
        let mut tree = pages;
        for (depth, byte) in page.bytes().into_iter().enumerate() {
            let Some(child) = layout::child(&self.tree(tree).await?.tree, byte) else {
                return Ok(None);
            };
            if depth == 2 {
                return Ok(Some(child));
            }
            tree = child;
        }
        unreachable!()
    }

    /// Apply page changes (`None` removes a page) to the page map `base`, copy-on-write: only
    /// trees on the changed paths are new. Returns the new map's root and the new trees, each
    /// carrying its previous version as a delta base.
    pub(crate) async fn build_page_map(
        &self,
        base: Option<ObjectId>,
        changes: &BTreeMap<PageId, Option<ObjectId>>,
    ) -> Result<(ObjectId, Vec<NewObject>)> {
        type Cells = Vec<(u8, Option<ObjectId>)>;
        let mut grouped: BTreeMap<u8, BTreeMap<u8, Cells>> = BTreeMap::new();
        for (id, blob) in changes {
            let [a, b, c] = id.bytes();
            grouped
                .entry(a)
                .or_default()
                .entry(b)
                .or_default()
                .push((c, *blob));
        }
        let base_top = match base {
            Some(id) => Some(self.tree(id).await?),
            None => None,
        };
        let mut top = base_top
            .as_ref()
            .map(|t| t.tree.clone())
            .unwrap_or_default();
        let mut out = Vec::new();
        for (a, by_b) in grouped {
            let base_mid = match base_top.as_ref().and_then(|t| layout::child(&t.tree, a)) {
                Some(id) => Some(self.tree(id).await?),
                None => None,
            };
            let mut mid = base_mid
                .as_ref()
                .map(|t| t.tree.clone())
                .unwrap_or_default();
            for (b, cells) in by_b {
                let base_leaf = match base_mid.as_ref().and_then(|t| layout::child(&t.tree, b)) {
                    Some(id) => Some(self.tree(id).await?),
                    None => None,
                };
                let mut leaf = base_leaf
                    .as_ref()
                    .map(|t| t.tree.clone())
                    .unwrap_or_default();
                for (c, blob) in cells {
                    let name = layout::byte_name(c);
                    match blob {
                        Some(id) => leaf
                            .insert(TreeEntry::blob(name, id))
                            .map_err(|e| invalid(e.to_string()))?,
                        None => {
                            leaf.remove(&name);
                        }
                    }
                }
                self.place(&mut mid, b, leaf, base_leaf.as_deref(), &mut out)?;
            }
            self.place(&mut top, a, mid, base_mid.as_deref(), &mut out)?;
        }
        let top_object = top.to_object()?;
        if base_top.as_ref().map(|t| t.object.id()) != Some(top_object.id()) {
            out.push(with_base(top_object.clone(), base_top.as_deref()));
            self.remember_tree(top_object.clone())?;
        }
        Ok((top_object.id(), out))
    }

    /// Put `child` under `parent` at `byte`, or drop the entry if `child` is empty.
    fn place(
        &self,
        parent: &mut Tree,
        byte: u8,
        child: Tree,
        base: Option<&CachedTree>,
        out: &mut Vec<NewObject>,
    ) -> Result<()> {
        let name = layout::byte_name(byte);
        if child.entries().is_empty() {
            parent.remove(&name);
            return Ok(());
        }
        let object = child.to_object()?;
        if base.map(|b| b.object.id()) != Some(object.id()) {
            out.push(with_base(object.clone(), base));
            self.remember_tree(object.clone())?;
        }
        parent
            .insert(TreeEntry::tree(name, object.id()))
            .map_err(|e| invalid(e.to_string()))
    }

    /// Page ids whose blobs differ between two snapshots (added, changed or removed).
    /// Walks both page maps together and skips every subtree whose id is unchanged.
    pub(crate) async fn changed_pages(
        &self,
        a: &SnapshotInfo,
        b: &SnapshotInfo,
    ) -> Result<BTreeSet<PageId>> {
        let mut changed = BTreeSet::new();
        let mut stack = vec![(Some(a.pages()), Some(b.pages()), 0usize, [0u8; 3])];
        while let Some((x, y, depth, prefix)) = stack.pop() {
            if x == y {
                continue;
            }
            let mut pairs: BTreeMap<u8, (Option<ObjectId>, Option<ObjectId>)> = BTreeMap::new();
            if let Some(id) = x {
                for (byte, child) in layout::children(&self.tree(id).await?.tree)? {
                    pairs.entry(byte).or_default().0 = Some(child);
                }
            }
            if let Some(id) = y {
                for (byte, child) in layout::children(&self.tree(id).await?.tree)? {
                    pairs.entry(byte).or_default().1 = Some(child);
                }
            }
            for (byte, (cx, cy)) in pairs {
                if cx == cy {
                    continue;
                }
                let mut next = prefix;
                next[depth] = byte;
                if depth == 2 {
                    changed.insert(PageId::from_bytes(next));
                } else {
                    stack.push((cx, cy, depth + 1, next));
                }
            }
        }
        Ok(changed)
    }

    /// Does `tip` descend from `ancestor` (first-parent chain)? Uses the commit graph this client
    /// has seen, fetching the commits in between at most once. A commit copied into a new
    /// generation stands for the commit it was copied from.
    pub(crate) async fn is_descendant(&self, tip: ObjectId, ancestor: ObjectId) -> Result<bool> {
        self.is_descendant_on(&self.transport(), tip, ancestor)
            .await
    }

    /// [`PageStore::is_descendant`], fetching what is missing through `transport`.
    pub(crate) async fn is_descendant_on(
        &self,
        transport: &Arc<T>,
        tip: ObjectId,
        ancestor: ObjectId,
    ) -> Result<bool> {
        let mut fetched = false;
        let mut id = tip;
        loop {
            let origin = self.inner.origins.lock().unwrap().get(&id).copied();
            if id == ancestor || origin == Some(ancestor) {
                return Ok(true);
            }
            let parents = self.inner.parents.lock().unwrap().get(&id).cloned();
            match parents {
                Some(parents) => match parents.first() {
                    Some(first) => id = *first,
                    None => return Ok(false),
                },
                None if !fetched => {
                    fetched = true;
                    // With trees: a writer that lost a race loads this head next, and the same
                    // round trip brings every page-map tree it changed.
                    for object in transport.history(tip, Some(ancestor), true).await? {
                        self.absorb(object)?;
                    }
                }
                None => return Ok(false),
            }
        }
    }

    /// Record a commit this client just published.
    pub(crate) fn published(
        &self,
        info: SnapshotInfo,
        commit: &Object,
        objects: &[(Object, bool)],
    ) {
        for (object, hot) in objects {
            self.remember(object, *hot);
        }
        self.remember(commit, true);
        self.record_parents(info.commit, info.parents.clone());
        let id = info.commit;
        self.inner.snapshots.lock().unwrap().put(id, Arc::new(info));
        *self.inner.last_loaded.lock().unwrap() = Some(id);
        self.observe_head(id);
    }
}

fn with_base(object: Object, base: Option<&CachedTree>) -> NewObject {
    match base {
        Some(b) => NewObject::with_delta_base(object, b.object.clone()),
        None => object.into(),
    }
}

fn spawn_watcher<T: Transport + 'static>(inner: Weak<Inner<T>>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // the first tick is immediate
        loop {
            tick.tick().await;
            let Some(inner) = inner.upgrade() else { break };
            // Through `refresh`, which also follows the database if it moved.
            if let Err(e) = (PageStore { inner }).refresh().await {
                tracing::debug!(error = %e, "head poll failed");
            }
        }
    });
}

/// The newest generation pointer among `refs`, if any.
fn newest_pointer(refs: &[octopage_git::Ref]) -> Option<ObjectId> {
    refs.iter()
        .filter_map(|r| Some((generation::pointer_generation(&r.name)?, r.id)))
        .max_by_key(|(n, _)| *n)
        .map(|(_, id)| id)
}

/// The record in the generation pointer commit `pointer`.
async fn read_pointer<T: Transport>(transport: &Arc<T>, pointer: ObjectId) -> Result<Moved> {
    let object = transport
        .fetch(&[pointer])
        .await?
        .into_iter()
        .find(|o| o.id() == pointer)
        .ok_or(octopage_git::Error::MissingObjects(vec![pointer]))?;
    Moved::from_pointer(object.data())
}

/// The transport to the repository that holds `branch` now, following generation pointers,
/// and the branch's head there.
async fn locate<T: Transport>(mut transport: Arc<T>, branch: &str) -> Result<(Arc<T>, ObjectId)> {
    for _ in 0..MAX_MOVES {
        let refs = transport.list_refs(&[branch, GEN_PREFIX]).await?;
        if let Some(pointer) = newest_pointer(&refs) {
            let moved = read_pointer(&transport, pointer).await?;
            transport = Arc::new((*transport).relocate(&moved.location)?);
            continue;
        }
        let head = refs
            .into_iter()
            .find(|r| r.name == branch)
            .map(|r| r.id)
            .ok_or_else(|| Error::NoDatabase(branch.to_string()))?;
        return Ok((transport, head));
    }
    Err(format(
        "the database's generation pointers go round in a loop",
    ))
}

/// The database as of one commit. Reads never block and never change.
pub struct Snapshot<T: Transport + 'static> {
    pub(crate) store: PageStore<T>,
    pub(crate) info: Arc<SnapshotInfo>,
}

impl<T: Transport + 'static> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Snapshot {
            store: self.store.clone(),
            info: self.info.clone(),
        }
    }
}

impl<T: Transport + 'static> Snapshot<T> {
    pub fn id(&self) -> ObjectId {
        self.info.commit
    }

    /// The commit sequence number: pages in this snapshot carry epochs at most this.
    pub fn epoch(&self) -> u64 {
        self.info.epoch
    }

    pub fn superblock(&self) -> Superblock {
        self.info.superblock
    }

    pub fn page_size(&self) -> usize {
        self.info.superblock.page_size as usize
    }

    pub fn time(&self) -> i64 {
        self.info.time
    }

    pub fn message(&self) -> &str {
        &self.info.message
    }

    pub async fn get(&self, id: PageId) -> Result<Option<Page>> {
        let Some(blob) = self.store.resolve(self.info.pages(), id).await? else {
            return Ok(None);
        };
        let object = self.store.object(blob).await?;
        let bytes = self.store.codec().open(Kind::Page, object.data())?;
        let page = Page::parse(bytes, self.page_size(), id, self.info.epoch)?;
        if page.page_type().is_hot() {
            self.store.remember(&object, true);
        }
        Ok(Some(page))
    }

    /// Several pages; every page not in a cache comes in one coalesced fetch.
    pub async fn get_many(&self, ids: &[PageId]) -> Result<Vec<Option<Page>>> {
        let mut blobs = Vec::with_capacity(ids.len());
        for id in ids {
            blobs.push(self.store.resolve(self.info.pages(), *id).await?);
        }
        let wanted: Vec<ObjectId> = blobs.iter().flatten().copied().collect();
        let mut objects = self.store.objects(&wanted).await?.into_iter();
        ids.iter()
            .zip(blobs)
            .map(|(id, blob)| match blob {
                Some(_) => {
                    let object = objects.next().expect("one object per blob");
                    let bytes = self.store.codec().open(Kind::Page, object.data())?;
                    Page::parse(bytes, self.page_size(), *id, self.info.epoch).map(Some)
                }
                None => Ok(None),
            })
            .collect()
    }

    pub async fn catalog(&self) -> Result<Option<Bytes>> {
        match self.info.root.catalog {
            Some(id) => {
                let object = self.store.object(id).await?;
                Ok(Some(self.store.codec().open(Kind::Catalog, object.data())?))
            }
            None => Ok(None),
        }
    }

    /// What the commit that made this snapshot recorded about itself.
    pub async fn changelog(&self) -> Result<Bytes> {
        match self.info.root.changelog {
            Some(id) => {
                let object = self.store.object(id).await?;
                self.store.codec().open(Kind::Changelog, object.data())
            }
            None => Ok(Bytes::new()),
        }
    }

    /// This commit's root tree.
    pub fn root(&self) -> &Root {
        &self.info.root
    }

    /// The root tree's id.
    pub fn tree(&self) -> ObjectId {
        self.info.tree
    }

    /// The commit's parents.
    pub fn parents(&self) -> &[ObjectId] {
        &self.info.parents
    }

    /// The maintenance settings, as stored (plain; see [`crate::WriteTxn::set_settings`]).
    pub async fn settings(&self) -> Result<Option<Bytes>> {
        match self.info.root.settings {
            Some(id) => Ok(Some(self.store.object(id).await?.data().clone())),
            None => Ok(None),
        }
    }

    /// Where the database went, if this is its last commit in a repository it moved out of.
    pub async fn moved(&self) -> Result<Option<Moved>> {
        match self.info.root.moved {
            Some(id) => Ok(Some(Moved::parse(self.store.object(id).await?.data())?)),
            None => Ok(None),
        }
    }

    /// The highest page id in this snapshot: the rightmost path through the page map.
    pub async fn max_page(&self) -> Result<PageId> {
        let mut tree = self.info.pages();
        let mut id = [0u8; 3];
        for (depth, slot) in id.iter_mut().enumerate() {
            let children = layout::children(&self.store.tree(tree).await?.tree)?;
            let (byte, child) = children
                .into_iter()
                .max_by_key(|(b, _)| *b)
                .ok_or_else(|| format("empty page-map tree"))?;
            *slot = byte;
            if depth < 2 {
                tree = child;
            }
        }
        Ok(PageId::from_bytes(id))
    }

    /// Every page that exists in this snapshot, in id order.
    pub async fn page_ids(&self) -> Result<Vec<PageId>> {
        let mut ids = Vec::new();
        let mut stack = vec![(self.info.pages(), 0usize, [0u8; 3])];
        while let Some((tree, depth, prefix)) = stack.pop() {
            for (byte, child) in layout::children(&self.store.tree(tree).await?.tree)? {
                let mut next = prefix;
                next[depth] = byte;
                if depth == 2 {
                    ids.push(PageId::from_bytes(next));
                } else {
                    stack.push((child, depth + 1, next));
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}
