use octopage_git::{EntryMode, ObjectId, Tree, TreeEntry};

use crate::error::{Result, format};

pub(crate) const PAGES: &[u8] = b"pages";
pub(crate) const CATALOG: &[u8] = b"catalog";
pub(crate) const CHANGELOG: &[u8] = b"changelog";
pub(crate) const KEYS: &[u8] = b"keys";
pub(crate) const SETTINGS: &[u8] = b"settings";
pub(crate) const MOVED: &[u8] = b"moved";

/// A database commit's root tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Root {
    /// The page map.
    pub pages: Option<ObjectId>,
    /// The layer above's schema, packed like pages.
    pub catalog: Option<ObjectId>,
    /// The commit's statements, packed like pages.
    pub changelog: Option<ObjectId>,
    /// Wrapped data keys, stored as they are.
    pub keys: Option<ObjectId>,
    /// Maintenance settings, stored as they are (a maintenance job may hold no keys).
    pub settings: Option<ObjectId>,
    /// Where the database went (see [`crate::Moved`]), stored as it is.
    pub moved: Option<ObjectId>,
    /// Every other entry, kept as it is.
    pub extra: Vec<TreeEntry>,
}

impl Root {
    pub fn decode(tree: &Tree) -> Result<Root> {
        let mut root = Root::default();
        for e in tree.entries() {
            match (e.name.as_slice(), e.mode) {
                (PAGES, EntryMode::Tree) => root.pages = Some(e.id),
                (CATALOG, EntryMode::Blob) => root.catalog = Some(e.id),
                (CHANGELOG, EntryMode::Blob) => root.changelog = Some(e.id),
                (KEYS, EntryMode::Blob) => root.keys = Some(e.id),
                (SETTINGS, EntryMode::Blob) => root.settings = Some(e.id),
                (MOVED, EntryMode::Blob) => root.moved = Some(e.id),
                _ => root.extra.push(e.clone()),
            }
        }
        if root.pages.is_none() {
            return Err(format("the root tree has no pages/ tree"));
        }
        Ok(root)
    }

    pub fn to_tree(&self) -> Tree {
        let mut tree = Tree::new();
        let entries = [
            self.pages.map(|id| TreeEntry::tree(PAGES, id)),
            self.catalog.map(|id| TreeEntry::blob(CATALOG, id)),
            self.changelog.map(|id| TreeEntry::blob(CHANGELOG, id)),
            self.keys.map(|id| TreeEntry::blob(KEYS, id)),
            self.settings.map(|id| TreeEntry::blob(SETTINGS, id)),
            self.moved.map(|id| TreeEntry::blob(MOVED, id)),
        ];
        for entry in entries.into_iter().flatten() {
            tree.insert(entry).expect("fixed names are valid");
        }
        for entry in &self.extra {
            tree.insert(entry.clone()).expect("names from a valid tree");
        }
        tree
    }
}

/// The fan-out entry name for one byte of a page id: two lowercase hex digits.
pub(crate) fn byte_name(b: u8) -> [u8; 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [HEX[(b >> 4) as usize], HEX[(b & 15) as usize]]
}

pub(crate) fn name_byte(name: &[u8]) -> Option<u8> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    match name {
        [hi, lo] => Some(digit(*hi)? << 4 | digit(*lo)?),
        _ => None,
    }
}

/// The child of a fan-out tree for byte `b`.
pub(crate) fn child(tree: &Tree, b: u8) -> Option<ObjectId> {
    tree.get(&byte_name(b)).map(|e| e.id)
}

/// The (byte, id) children of a fan-out tree. Entries with other names are a format error.
pub(crate) fn children(tree: &Tree) -> Result<Vec<(u8, ObjectId)>> {
    tree.entries()
        .iter()
        .map(|e| {
            name_byte(&e.name).map(|b| (b, e.id)).ok_or_else(|| {
                format(format!(
                    "unexpected page-map entry {:?}",
                    String::from_utf8_lossy(&e.name)
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for b in 0..=255u8 {
            assert_eq!(name_byte(&byte_name(b)), Some(b));
        }
        assert_eq!(name_byte(b"AB"), None);
        assert_eq!(name_byte(b"abc"), None);
    }

    #[test]
    fn root_round_trip() {
        let id = |n| ObjectId::from_array([n; 20]);
        let root = Root {
            pages: Some(id(1)),
            catalog: None,
            changelog: Some(id(3)),
            keys: Some(id(4)),
            settings: Some(id(5)),
            moved: None,
            extra: vec![TreeEntry::tree(".github", id(6))],
        };
        assert_eq!(Root::decode(&root.to_tree()).unwrap(), root);
        assert!(Root::decode(&Tree::new()).is_err());
    }
}
