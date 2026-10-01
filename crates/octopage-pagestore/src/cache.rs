use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use lru::LruCache;
use octopage_git::{Object, ObjectId, ObjectKind, Tree};

struct Segment {
    lru: LruCache<ObjectId, Object>,
    bytes: usize,
    budget: usize,
}

impl Segment {
    fn new(budget: usize) -> Self {
        Segment {
            lru: LruCache::unbounded(),
            bytes: 0,
            budget,
        }
    }

    fn get(&mut self, id: &ObjectId) -> Option<Object> {
        self.lru.get(id).cloned()
    }

    fn insert(&mut self, object: Object) {
        let size = object.data().len();
        if size > self.budget {
            return;
        }
        if let Some(old) = self.lru.put(object.id(), object) {
            self.bytes -= old.data().len();
        }
        self.bytes += size;
        while self.bytes > self.budget {
            match self.lru.pop_lru() {
                Some((_, evicted)) => self.bytes -= evicted.data().len(),
                None => break,
            }
        }
    }
}

/// The buffer pool: objects in memory, least recently used out first. A protected segment
/// (an eighth of the budget) holds hot pages, so a large scan cannot push out the pages that
/// sit on every lookup path.
pub(crate) struct MemCache {
    hot: Segment,
    cold: Segment,
}

impl MemCache {
    pub fn new(budget: usize) -> Self {
        MemCache {
            hot: Segment::new(budget / 8),
            cold: Segment::new(budget - budget / 8),
        }
    }

    pub fn get(&mut self, id: &ObjectId) -> Option<Object> {
        self.hot.get(id).or_else(|| self.cold.get(id))
    }

    pub fn insert(&mut self, object: Object, hot: bool) {
        if hot {
            self.hot.insert(object);
        } else {
            self.cold.insert(object);
        }
    }
}

/// A tree object with its decoded entries.
pub(crate) struct CachedTree {
    pub object: Object,
    pub tree: Tree,
}

pub(crate) struct TreeCache {
    lru: LruCache<ObjectId, Arc<CachedTree>>,
    bytes: usize,
    budget: usize,
}

impl TreeCache {
    pub fn new(budget: usize) -> Self {
        TreeCache {
            lru: LruCache::unbounded(),
            bytes: 0,
            budget,
        }
    }

    fn cost(tree: &CachedTree) -> usize {
        // Raw bytes plus decoded entries, roughly.
        tree.object.data().len() * 3
    }

    pub fn get(&mut self, id: &ObjectId) -> Option<Arc<CachedTree>> {
        self.lru.get(id).cloned()
    }

    pub fn insert(&mut self, tree: Arc<CachedTree>) {
        let cost = TreeCache::cost(&tree);
        if let Some(old) = self.lru.put(tree.object.id(), tree) {
            self.bytes -= TreeCache::cost(&old);
        }
        self.bytes += cost;
        while self.bytes > self.budget {
            match self.lru.pop_lru() {
                Some((_, evicted)) => self.bytes -= TreeCache::cost(&evicted),
                None => break,
            }
        }
    }
}

const SWEEP_EVERY: u64 = 4096;

/// Objects on disk, shared by every process on the machine: `objects/ab/cdef...` holds one
/// object (a type byte, then its bytes). Writes go through a temporary file and a rename, and
/// every read is re-hashed, so a torn or corrupted file is detected, deleted and refetched.
pub(crate) struct DiskCache {
    dir: PathBuf,
    budget: u64,
    puts: AtomicU64,
}

fn kind_code(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

fn code_kind(code: u8) -> Option<ObjectKind> {
    Some(match code {
        1 => ObjectKind::Commit,
        2 => ObjectKind::Tree,
        3 => ObjectKind::Blob,
        4 => ObjectKind::Tag,
        _ => return None,
    })
}

impl DiskCache {
    pub fn open(dir: &Path, budget: u64) -> std::io::Result<Self> {
        fs::create_dir_all(dir.join("objects"))?;
        fs::create_dir_all(dir.join("heads"))?;
        let cache = DiskCache {
            dir: dir.to_path_buf(),
            budget,
            puts: AtomicU64::new(0),
        };
        cache.sweep();
        Ok(cache)
    }

    fn path(&self, id: ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    pub fn get(&self, id: ObjectId) -> Option<Object> {
        let path = self.path(id);
        let bytes = fs::read(&path).ok()?;
        let object = bytes
            .split_first()
            .and_then(|(&code, data)| Object::new(code_kind(code)?, data.to_vec()).ok());
        match object {
            Some(object) if object.id() == id => Some(object),
            _ => {
                tracing::warn!(%id, "discarding a corrupt disk cache entry");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    pub fn put(&self, object: &Object) {
        let path = self.path(object.id());
        if path.exists() {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut bytes = Vec::with_capacity(1 + object.data().len());
        bytes.push(kind_code(object.kind()));
        bytes.extend_from_slice(object.data());
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), fastrand::u64(..)));
        if fs::write(&tmp, &bytes).is_err() || fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp); // another process may have won the race; either copy is fine
        }
        if self.puts.fetch_add(1, Ordering::Relaxed) % SWEEP_EVERY == SWEEP_EVERY - 1 {
            self.sweep();
        }
    }

    /// Delete the oldest entries until the cache is under 90% of its budget.
    pub fn sweep(&self) {
        let mut files = Vec::new();
        let mut total = 0u64;
        let Ok(fanout) = fs::read_dir(self.dir.join("objects")) else {
            return;
        };
        for dir in fanout.flatten() {
            let Ok(entries) = fs::read_dir(dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                    files.push((
                        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        meta.len(),
                        entry.path(),
                    ));
                }
            }
        }
        if total <= self.budget {
            return;
        }
        files.sort();
        let target = self.budget / 10 * 9;
        for (_, len, path) in files {
            if total <= target {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }

    fn head_path(&self, branch: &str) -> PathBuf {
        let name: String = branch
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.dir.join("heads").join(name)
    }

    /// The last head this machine saw for `branch`, so a new process can fetch only what changed.
    pub fn head(&self, branch: &str) -> Option<ObjectId> {
        ObjectId::from_hex(fs::read_to_string(self.head_path(branch)).ok()?.trim()).ok()
    }

    pub fn set_head(&self, branch: &str, id: ObjectId) {
        let path = self.head_path(branch);
        let tmp = path.with_extension(format!("tmp-{}", fastrand::u64(..)));
        if fs::write(&tmp, id.to_hex()).is_err() || fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(n: usize, fill: u8) -> Object {
        Object::blob(vec![fill; n]).unwrap()
    }

    #[test]
    fn memory_budget_and_protected_segment() {
        let mut cache = MemCache::new(8 * 4096); // hot 4 KB, cold 28 KB
        let hot = blob(4096, 0);
        cache.insert(hot.clone(), true);
        let cold: Vec<Object> = (1..=20).map(|i| blob(4096, i)).collect();
        for o in &cold {
            cache.insert(o.clone(), false);
        }
        assert!(
            cache.get(&hot.id()).is_some(),
            "a scan does not evict hot pages"
        );
        assert!(
            cache.get(&cold[0].id()).is_none(),
            "oldest cold page evicted"
        );
        assert!(cache.get(&cold[19].id()).is_some());
        let held = cold.iter().filter(|o| cache.get(&o.id()).is_some()).count();
        assert_eq!(held, 7, "28 KB of 4 KB pages");
    }

    #[test]
    fn disk_cache_round_trip_detects_corruption_and_sweeps() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(dir.path(), 10 * 4097).unwrap();
        let o = blob(4096, 7);
        assert!(cache.get(o.id()).is_none());
        cache.put(&o);
        assert_eq!(cache.get(o.id()), Some(o.clone()));

        // Flip a byte on disk: the entry is detected, deleted, and reads as a miss.
        let path = cache.path(o.id());
        let mut bytes = fs::read(&path).unwrap();
        bytes[100] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(cache.get(o.id()).is_none());
        assert!(!path.exists());

        for i in 0..20 {
            cache.put(&blob(4096, 100 + i));
        }
        cache.sweep();
        let kept = (0..20)
            .filter(|i| cache.get(blob(4096, 100 + i).id()).is_some())
            .count();
        assert!(kept <= 9, "swept to 90% of a 10-object budget, kept {kept}");

        let head = ObjectId::from_array([9; 20]);
        cache.set_head("refs/heads/main", head);
        assert_eq!(cache.head("refs/heads/main"), Some(head));
        assert_eq!(cache.head("refs/heads/other"), None);
    }
}
