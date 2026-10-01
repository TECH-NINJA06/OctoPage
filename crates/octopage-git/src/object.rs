use std::cmp::Ordering;
use std::fmt;

use bytes::Bytes;
use sha1_checked::{CollisionResult, Digest, Sha1};

use crate::error::{Error, Result, invalid, protocol};
use crate::oid::ObjectId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectKind {
    pub fn name(self) -> &'static str {
        match self {
            ObjectKind::Commit => "commit",
            ObjectKind::Tree => "tree",
            ObjectKind::Blob => "blob",
            ObjectKind::Tag => "tag",
        }
    }

    /// Type code used in packfile entry headers.
    pub(crate) fn pack_code(self) -> u8 {
        match self {
            ObjectKind::Commit => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Blob => 3,
            ObjectKind::Tag => 4,
        }
    }

    pub(crate) fn from_pack_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(ObjectKind::Commit),
            2 => Some(ObjectKind::Tree),
            3 => Some(ObjectKind::Blob),
            4 => Some(ObjectKind::Tag),
            _ => None,
        }
    }
}

/// SHA-1 of `"<kind> <len>\0" + data`, with the same collision detection git and GitHub use.
/// A detected collision attack is an integrity error, never a silently accepted id.
pub fn hash_object(kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
    let mut hasher = Sha1::new();
    hasher.update(format!("{} {}\0", kind.name(), data.len()));
    hasher.update(data);
    match hasher.try_finalize() {
        CollisionResult::Ok(digest) => ObjectId::from_bytes(digest.as_slice()),
        _ => Err(Error::Integrity(format!(
            "SHA-1 collision attack detected in a {} of {} bytes",
            kind.name(),
            data.len()
        ))),
    }
}

/// A git object whose id has been computed from its bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Object {
    kind: ObjectKind,
    id: ObjectId,
    data: Bytes,
}

impl Object {
    pub fn new(kind: ObjectKind, data: impl Into<Bytes>) -> Result<Self> {
        let data = data.into();
        let id = hash_object(kind, &data)?;
        Ok(Object { kind, id, data })
    }

    pub fn blob(data: impl Into<Bytes>) -> Result<Self> {
        Object::new(ObjectKind::Blob, data)
    }

    pub fn kind(&self) -> ObjectKind {
        self.kind
    }

    pub fn id(&self) -> ObjectId {
        self.id
    }

    pub fn data(&self) -> &Bytes {
        &self.data
    }
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Object({} {} {} bytes)",
            self.kind.name(),
            self.id,
            self.data.len()
        )
    }
}

// ------------------------------------------------------------------------------------ trees

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryMode {
    Blob,
    Tree,
    /// Any other mode (executable, symlink, submodule), preserved when decoding foreign trees.
    Other(u32),
}

impl EntryMode {
    pub fn octal(self) -> u32 {
        match self {
            EntryMode::Blob => 0o100644,
            EntryMode::Tree => 0o40000,
            EntryMode::Other(mode) => mode,
        }
    }

    fn from_octal(mode: u32) -> Self {
        match mode {
            0o100644 => EntryMode::Blob,
            0o40000 => EntryMode::Tree,
            other => EntryMode::Other(other),
        }
    }

    fn is_tree(self) -> bool {
        matches!(self, EntryMode::Tree)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: EntryMode,
    pub name: Vec<u8>,
    pub id: ObjectId,
}

impl TreeEntry {
    pub fn blob(name: impl Into<Vec<u8>>, id: ObjectId) -> Self {
        TreeEntry {
            mode: EntryMode::Blob,
            name: name.into(),
            id,
        }
    }

    pub fn tree(name: impl Into<Vec<u8>>, id: ObjectId) -> Self {
        TreeEntry {
            mode: EntryMode::Tree,
            name: name.into(),
            id,
        }
    }
}

/// Git's tree order: names compare bytewise, and a tree sorts as if its name ended in '/'.
fn git_order(a: &TreeEntry, b: &TreeEntry) -> Ordering {
    let n = a.name.len().min(b.name.len());
    a.name[..n].cmp(&b.name[..n]).then_with(|| {
        let end = |e: &TreeEntry| {
            e.name
                .get(n)
                .copied()
                .unwrap_or(if e.mode.is_tree() { b'/' } else { 0 })
        };
        end(a).cmp(&end(b))
    })
}

/// A tree object. Entries are kept in git order with unique names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new() -> Self {
        Tree::default()
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn get(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Insert an entry, replacing any existing entry with the same name.
    pub fn insert(&mut self, entry: TreeEntry) -> Result<()> {
        let name = &entry.name;
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(invalid(format!(
                "invalid tree entry name {:?}",
                String::from_utf8_lossy(name)
            )));
        }
        self.remove(&entry.name);
        let at = self
            .entries
            .partition_point(|e| git_order(e, &entry) == Ordering::Less);
        self.entries.insert(at, entry);
        Ok(())
    }

    pub fn remove(&mut self, name: &[u8]) -> Option<TreeEntry> {
        let at = self.entries.iter().position(|e| e.name == name)?;
        Some(self.entries.remove(at))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.entries.len() * 32);
        for e in &self.entries {
            out.extend_from_slice(format!("{:o} ", e.mode.octal()).as_bytes());
            out.extend_from_slice(&e.name);
            out.push(0);
            out.extend_from_slice(e.id.as_bytes());
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let space = rest
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| protocol("tree entry without mode"))?;
            let mode = std::str::from_utf8(&rest[..space])
                .ok()
                .and_then(|m| u32::from_str_radix(m, 8).ok())
                .ok_or_else(|| protocol("tree entry with a non-octal mode"))?;
            rest = &rest[space + 1..];
            let nul = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| protocol("tree entry without name terminator"))?;
            let name = rest[..nul].to_vec();
            rest = &rest[nul + 1..];
            if rest.len() < ObjectId::LEN {
                return Err(protocol("tree entry with a truncated id"));
            }
            let id = ObjectId::from_bytes(&rest[..ObjectId::LEN])?;
            rest = &rest[ObjectId::LEN..];
            entries.push(TreeEntry {
                mode: EntryMode::from_octal(mode),
                name,
                id,
            });
        }
        Ok(Tree { entries })
    }

    pub fn to_object(&self) -> Result<Object> {
        Object::new(ObjectKind::Tree, self.encode())
    }
}

// ------------------------------------------------------------------------------------ commits

/// An author or committer line: `Name <email> <unix seconds> <+hhmm>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub seconds: i64,
    pub offset_minutes: i32,
}

impl Signature {
    pub fn new(name: impl Into<String>, email: impl Into<String>, seconds: i64) -> Self {
        Signature {
            name: name.into(),
            email: email.into(),
            seconds,
            offset_minutes: 0,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        for field in [&self.name, &self.email] {
            if field.contains(['<', '>', '\n']) {
                return Err(invalid(format!(
                    "signature field {field:?} contains <, > or a newline"
                )));
            }
        }
        let sign = if self.offset_minutes < 0 { '-' } else { '+' };
        let offset = self.offset_minutes.unsigned_abs();
        out.extend_from_slice(
            format!(
                "{} <{}> {} {sign}{:02}{:02}",
                self.name,
                self.email,
                self.seconds,
                offset / 60,
                offset % 60
            )
            .as_bytes(),
        );
        Ok(())
    }

    fn parse(line: &[u8]) -> Result<Self> {
        let line = std::str::from_utf8(line).map_err(|_| protocol("signature is not UTF-8"))?;
        let bad = || protocol(format!("malformed signature {line:?}"));
        let close = line.rfind('>').ok_or_else(bad)?;
        let open = line[..close].rfind('<').ok_or_else(bad)?;
        let mut when = line[close + 1..].split_whitespace();
        let seconds = when.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let tz = when.next().unwrap_or("+0000");
        let digits: i32 = tz.get(1..).and_then(|d| d.parse().ok()).ok_or_else(bad)?;
        let minutes = digits / 100 * 60 + digits % 100;
        Ok(Signature {
            name: line[..open].trim_end().to_string(),
            email: line[open + 1..close].to_string(),
            seconds,
            offset_minutes: if tz.starts_with('-') {
                -minutes
            } else {
                minutes
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: Signature,
    pub committer: Signature,
    pub message: Vec<u8>,
}

impl Commit {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(256 + self.message.len());
        out.extend_from_slice(format!("tree {}\n", self.tree).as_bytes());
        for parent in &self.parents {
            out.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        out.extend_from_slice(b"author ");
        self.author.encode(&mut out)?;
        out.extend_from_slice(b"\ncommitter ");
        self.committer.encode(&mut out)?;
        out.extend_from_slice(b"\n\n");
        out.extend_from_slice(&self.message);
        Ok(out)
    }

    /// Parse a commit. Headers other than tree, parent, author and committer (such as gpgsig) are skipped.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let split = data.windows(2).position(|w| w == b"\n\n");
        let (headers, message) = match split {
            Some(at) => (&data[..at], data[at + 2..].to_vec()),
            None => (data, Vec::new()),
        };
        let (mut tree, mut parents, mut author, mut committer) = (None, Vec::new(), None, None);
        for line in headers.split(|&b| b == b'\n') {
            if line.starts_with(b" ") {
                continue; // continuation of a multi-line header
            }
            let Some(space) = line.iter().position(|&b| b == b' ') else {
                continue;
            };
            let (key, value) = (&line[..space], &line[space + 1..]);
            let id = || {
                std::str::from_utf8(value)
                    .map_err(|_| protocol("commit id is not UTF-8"))
                    .and_then(ObjectId::from_hex)
            };
            match key {
                b"tree" => tree = Some(id()?),
                b"parent" => parents.push(id()?),
                b"author" => author = Some(Signature::parse(value)?),
                b"committer" => committer = Some(Signature::parse(value)?),
                _ => {}
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| protocol("commit without a tree"))?,
            parents,
            author: author.ok_or_else(|| protocol("commit without an author"))?,
            committer: committer.ok_or_else(|| protocol("commit without a committer"))?,
            message,
        })
    }

    pub fn to_object(&self) -> Result<Object> {
        Object::new(ObjectKind::Commit, self.encode()?)
    }

    /// The commit carrying `signature` (armored, as `ssh-keygen -Y sign` or gpg makes it) over
    /// [`Commit::encode`], in a `gpgsig` header: how git stores a signed commit.
    pub fn to_signed_object(&self, signature: &str) -> Result<Object> {
        let payload = self.encode()?;
        let end = payload
            .windows(2)
            .position(|w| w == b"\n\n")
            .expect("the headers end with a blank line");
        let mut out = Vec::with_capacity(payload.len() + signature.len() + 64);
        out.extend_from_slice(&payload[..=end]); // the headers, through the committer's newline
        out.extend_from_slice(b"gpgsig ");
        let lines: Vec<&str> = signature.trim_end().lines().collect();
        out.extend_from_slice(lines.join("\n ").as_bytes()); // continuation lines start with a space
        out.push(b'\n');
        out.extend_from_slice(&payload[end + 1..]); // the blank line and the message
        Object::new(ObjectKind::Commit, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex).unwrap()
    }

    #[test]
    fn known_hashes_match_git() {
        // `echo hello | git hash-object --stdin` and `git hash-object -t tree /dev/null`
        assert_eq!(
            Object::blob(&b"hello\n"[..]).unwrap().id(),
            id("ce013625030ba8dba906f756967f9e9ca394464a")
        );
        assert_eq!(
            Tree::new().to_object().unwrap().id(),
            id("4b825dc642cb6eb9a060e54bf8d69288fbee4904")
        );
        assert_eq!(
            Object::blob(Bytes::new()).unwrap().id(),
            id("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")
        );
    }

    #[test]
    fn trees_sort_like_git() {
        let x = ObjectId::ZERO;
        let mut t = Tree::new();
        t.insert(TreeEntry::blob("a.b", x)).unwrap();
        t.insert(TreeEntry::tree("a", x)).unwrap();
        t.insert(TreeEntry::blob("a-", x)).unwrap();
        t.insert(TreeEntry::blob("b", x)).unwrap();
        let names: Vec<_> = t
            .entries()
            .iter()
            .map(|e| String::from_utf8(e.name.clone()).unwrap())
            .collect();
        // A tree named "a" sorts as "a/", after "a-" and "a.b".
        assert_eq!(names, ["a-", "a.b", "a", "b"]);
    }

    #[test]
    fn tree_insert_replaces_and_round_trips() {
        let mut t = Tree::new();
        t.insert(TreeEntry::blob(
            "00",
            id("ce013625030ba8dba906f756967f9e9ca394464a"),
        ))
        .unwrap();
        t.insert(TreeEntry::tree("01", ObjectId::ZERO)).unwrap();
        t.insert(TreeEntry::blob("00", ObjectId::ZERO)).unwrap();
        assert_eq!(t.entries().len(), 2);
        assert_eq!(t.get(b"00").unwrap().id, ObjectId::ZERO);
        assert_eq!(Tree::decode(&t.encode()).unwrap(), t);
        assert!(t.insert(TreeEntry::blob("a/b", ObjectId::ZERO)).is_err());
        assert!(t.insert(TreeEntry::blob("", ObjectId::ZERO)).is_err());
    }

    #[test]
    fn commit_round_trips() {
        let commit = Commit {
            tree: id("4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
            parents: vec![id("ce013625030ba8dba906f756967f9e9ca394464a")],
            author: Signature {
                name: "A U Thor".into(),
                email: "a@example.invalid".into(),
                seconds: 1_790_000_000,
                offset_minutes: 330,
            },
            committer: Signature {
                name: "C".into(),
                email: "c@example.invalid".into(),
                seconds: 1_790_000_001,
                offset_minutes: -420,
            },
            message: b"txn 1\n".to_vec(),
        };
        let bytes = commit.encode().unwrap();
        assert!(
            String::from_utf8_lossy(&bytes)
                .contains("author A U Thor <a@example.invalid> 1790000000 +0530\n")
        );
        assert!(
            String::from_utf8_lossy(&bytes)
                .contains("committer C <c@example.invalid> 1790000001 -0700\n")
        );
        assert_eq!(Commit::decode(&bytes).unwrap(), commit);
    }

    #[test]
    fn commit_decode_skips_signature_headers() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a@x> 1 +0000\ncommitter A <a@x> 1 +0000\ngpgsig -----BEGIN-----\n line\n -----END-----\n\nmsg";
        let c = Commit::decode(raw).unwrap();
        assert!(c.parents.is_empty());
        assert_eq!(c.message, b"msg");
    }

    #[test]
    fn signature_rejects_injection() {
        let mut s = Signature::new("evil>\nparent 00", "e@x", 0);
        assert!(s.encode(&mut Vec::new()).is_err());
        s.name = "ok".into();
        assert!(s.encode(&mut Vec::new()).is_ok());
    }
}
