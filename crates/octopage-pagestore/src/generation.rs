use bytes::Bytes;
use octopage_git::{Commit, Object, Signature, Tree};
use serde::{Deserialize, Serialize};

use crate::error::{Result, format};

/// Where the generation pointers live: `refs/octopage/gen/<n>` in the old repository.
pub const GEN_PREFIX: &str = "refs/octopage/gen/";

/// How far a client follows pointers in one go, against a loop of pointers.
pub(crate) const MAX_MOVES: usize = 16;

/// Where a database went, and as which generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    /// The new repository's generation; the first repository is generation 1.
    pub generation: u64,
    /// The new repository: `OWNER/NAME` on GitHub (see `Transport::relocate`).
    pub location: String,
}

impl Moved {
    /// The contents of the `moved` blob.
    pub fn to_bytes(&self) -> Bytes {
        serde_json::to_vec(self).expect("serializes").into()
    }

    pub fn parse(bytes: &[u8]) -> Result<Moved> {
        serde_json::from_slice(bytes).map_err(|e| format(format!("a moved record: {e}")))
    }

    /// `refs/octopage/gen/<n>`.
    pub fn ref_name(&self) -> String {
        format!("{GEN_PREFIX}{}", self.generation)
    }

    /// The pointer commit for [`Moved::ref_name`]: the empty tree, this record in its message.
    /// Returns the tree and the commit.
    pub fn pointer(&self, signature: Signature) -> Result<(Object, Object)> {
        let tree = Tree::new().to_object()?;
        let json = String::from_utf8(self.to_bytes().to_vec()).expect("JSON is UTF-8");
        let commit = Commit {
            tree: tree.id(),
            parents: Vec::new(),
            author: signature.clone(),
            committer: signature,
            message: format!("octopage generation {}\n\n{json}\n", self.generation).into_bytes(),
        }
        .to_object()?;
        Ok((tree, commit))
    }

    /// The record in a pointer commit.
    pub fn from_pointer(commit: &[u8]) -> Result<Moved> {
        let commit = Commit::decode(commit).map_err(|e| format(e.to_string()))?;
        let message = String::from_utf8_lossy(&commit.message);
        let json = message
            .find('{')
            .map(|at| message[at..].trim())
            .ok_or_else(|| format("a generation pointer without a record"))?;
        Moved::parse(json.as_bytes())
    }
}

/// The generation a pointer ref names, from its name.
pub(crate) fn pointer_generation(name: &str) -> Option<u64> {
    name.strip_prefix(GEN_PREFIX)?.parse().ok()
}

/// A trailer on a commit copied into a new generation, naming the commit it was copied from
/// (copies of kept commits get new ids, because the commits before them were dropped).
pub const REWRITTEN_FROM: &str = "Rewritten-From: ";

/// The commit a copied commit came from, from its message.
pub fn rewritten_from(message: &[u8]) -> Option<octopage_git::ObjectId> {
    let text = std::str::from_utf8(message).ok()?;
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix(REWRITTEN_FROM))
        .and_then(|hex| octopage_git::ObjectId::from_hex(hex.trim()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let moved = Moved {
            generation: 3,
            location: "owner/db-g3".into(),
        };
        assert_eq!(Moved::parse(&moved.to_bytes()).unwrap(), moved);
        let (_, commit) = moved.pointer(Signature::new("o", "o@x", 0)).unwrap();
        assert_eq!(Moved::from_pointer(commit.data()).unwrap(), moved);
        assert_eq!(moved.ref_name(), "refs/octopage/gen/3");
        assert_eq!(pointer_generation("refs/octopage/gen/3"), Some(3));
        assert_eq!(pointer_generation("refs/octopage/gen/x"), None);
        let id = octopage_git::ObjectId::from_array([7; 20]);
        let message = format!("octopage txn 1\n\n{REWRITTEN_FROM}{}\n", id.to_hex());
        assert_eq!(rewritten_from(message.as_bytes()), Some(id));
        assert_eq!(rewritten_from(b"octopage txn 1\n"), None);
    }
}
